use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::PowerAction;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Active desktop idle / user activity detection backend kind (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleBackendKind {
    /// Wayland ext-idle-notify-v1 v2 (`get_input_idle_notification`, ignoring idle inhibitors).
    WaylandExtIdleV2,
    /// GNOME Mutter D-Bus (`org.gnome.Mutter.IdleMonitor`).
    GnomeMutter,
    /// Wayland ext-idle-notify-v1 v1 (`get_idle_notification`, inhibitor-sensitive).
    WaylandExtIdleV1,
    /// Native X11 (`MIT-SCREEN-SAVER` + `XInput2`).
    X11,
    /// Degraded fallback when no desktop idle backend is available.
    Degraded,
}

impl IdleBackendKind {
    /// Returns true if this backend is affected by media player idle inhibitors (v1 Wayland).
    pub fn is_inhibitor_sensitive(self) -> bool {
        matches!(self, Self::WaylandExtIdleV1)
    }

    /// Returns true if running in degraded mode with no desktop idle probe backend.
    pub fn is_degraded(self) -> bool {
        matches!(self, Self::Degraded)
    }

    /// Human-readable display label for status and diagnostics.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::WaylandExtIdleV2 => "Wayland (ext-idle-notify-v1 v2, inhibitor-insensitive)",
            Self::GnomeMutter => "GNOME (org.gnome.Mutter.IdleMonitor)",
            Self::WaylandExtIdleV1 => "Wayland (ext-idle-notify-v1 v1, inhibitor-sensitive)",
            Self::X11 => "X11 (MIT-SCREEN-SAVER + XInput2)",
            Self::Degraded => "Degraded (no idle backend available)",
        }
    }
}

/// Errors produced during desktop idle detection or display server communication.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IdleProbeError {
    #[error("display server connection error: {0}")]
    Connection(String),
    #[error("idle protocol error: {0}")]
    Protocol(String),
    #[error("idle backend unavailable: {0}")]
    Unavailable(String),
}

/// Discovered display server environment variables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayEnvironment {
    pub wayland_display: Option<String>,
    pub display: Option<String>,
}

/// Discovered system desktop idle probe capabilities.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SystemCapabilities {
    pub wayland_ext_idle_version: Option<u32>,
    pub gnome_mutter_available: bool,
    pub x11_available: bool,
}

/// Parses session D-Bus `systemd1.Manager.Environment` array into `DisplayEnvironment`.
pub fn parse_systemd_environment(env_vars: &[String]) -> DisplayEnvironment {
    let mut discovered = DisplayEnvironment::default();
    for entry in env_vars {
        if let Some((k, v)) = entry.split_once('=') {
            match k {
                "WAYLAND_DISPLAY" if !v.trim().is_empty() => {
                    discovered.wayland_display = Some(v.trim().to_string());
                }
                "DISPLAY" if !v.trim().is_empty() => {
                    discovered.display = Some(v.trim().to_string());
                }
                _ => {}
            }
        }
    }
    discovered
}

/// Selects the highest priority idle probe backend based on runtime display environment and capabilities.
///
/// Priority order (ADR-0006):
/// 1. Wayland `ext-idle-notify-v1` v2 (`get_input_idle_notification`, ignoring media idle inhibitors)
/// 2. GNOME `org.gnome.Mutter.IdleMonitor` (`/org/gnome/Mutter/IdleMonitor/Core`)
/// 3. Wayland `ext-idle-notify-v1` v1 (`get_idle_notification`, inhibitor-sensitive)
/// 4. Native X11 (`MIT-SCREEN-SAVER` + `XInput2`)
/// 5. Degraded fallback (`Degraded`)
pub fn select_idle_backend(env: &DisplayEnvironment, caps: &SystemCapabilities) -> IdleBackendKind {
    if env.wayland_display.is_some() {
        if let Some(v) = caps.wayland_ext_idle_version
            && v >= 2
        {
            return IdleBackendKind::WaylandExtIdleV2;
        }
        if caps.gnome_mutter_available {
            return IdleBackendKind::GnomeMutter;
        }
        if let Some(v) = caps.wayland_ext_idle_version
            && v == 1
        {
            return IdleBackendKind::WaylandExtIdleV1;
        }
        return IdleBackendKind::Degraded;
    }

    if env.display.is_some() {
        if caps.gnome_mutter_available {
            return IdleBackendKind::GnomeMutter;
        }
        if caps.x11_available {
            return IdleBackendKind::X11;
        }
        return IdleBackendKind::Degraded;
    }

    if caps.gnome_mutter_available {
        return IdleBackendKind::GnomeMutter;
    }

    IdleBackendKind::Degraded
}

/// Enforces the `>= 90 s` safety floor on degraded backends for destructive power actions (`Suspend`, `Poweroff`).
pub fn compute_effective_grace_period(
    configured_sec: u32,
    backend: IdleBackendKind,
    action: PowerAction,
) -> u32 {
    if backend.is_degraded() && matches!(action, PowerAction::Suspend | PowerAction::Poweroff) {
        configured_sec.max(90)
    } else {
        configured_sec
    }
}

/// Determines whether a destructive power action must be refused due to strict idle backend mode on a degraded backend.
pub fn is_power_action_refused(
    backend: IdleBackendKind,
    action: PowerAction,
    strict_idle_backend: bool,
) -> bool {
    backend.is_degraded()
        && strict_idle_backend
        && matches!(action, PowerAction::Suspend | PowerAction::Poweroff)
}

/// Boundary trait for desktop user activity and idle probes (ADR-0006, ADR-0007).
pub trait IdleProbe: Send + Sync {
    /// Returns the active backend kind.
    fn backend_kind(&self) -> IdleBackendKind;

    /// Eagerly resolves and caches the active idle backend kind.
    fn resolve_backend<'a>(&'a self) -> BoxFuture<'a, IdleBackendKind> {
        Box::pin(async move { self.backend_kind() })
    }

    /// Checks whether user activity occurred in the trailing `window` (e.g. 2 minutes = 120s).
    /// Used by the pre-trigger gate to suppress entering Grace Period.
    fn is_user_active_trailing<'a>(
        &'a self,
        window: Duration,
    ) -> BoxFuture<'a, Result<bool, IdleProbeError>>;

    /// Watches for user activity during the Grace Period.
    ///
    /// Respects the `settle_delay` (e.g. 1 s) on Grace Period entry before emitting activity signals,
    /// avoiding in-flight races.
    fn watch_grace_period_activity<'a>(
        &'a self,
        settle_delay: Duration,
    ) -> BoxFuture<'a, Result<mpsc::Receiver<()>, IdleProbeError>>;

    /// Watches for desktop user activity during the 5-minute post-trigger false-positive watch window.
    ///
    /// Returns `true` if physical input occurred before `timeout` elapsed, or `false` on clean completion.
    fn watch_post_trigger_activity<'a>(&'a self, timeout: Duration) -> BoxFuture<'a, bool>;
}
