use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::PowerAction;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Desktop notification urgency levels (Desktop Notifications Specification § urgencies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

impl Urgency {
    pub fn as_byte(self) -> u8 {
        self as u8
    }
}

/// Errors occurring during `Sleep Action Pipeline` execution or notification dispatch.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ActionError {
    #[error("MPRIS2 error: {0}")]
    Mpris(String),
    #[error("custom command error: {0}")]
    CustomCommand(String),
    #[error("custom command timed out after {0:?}")]
    CustomCommandTimeout(Duration),
    #[error("screen lock error: {0}")]
    LockScreen(String),
    #[error("power action error: {0}")]
    PowerAction(String),
    #[error("notification error: {0}")]
    Notification(String),
    #[error("inhibitor error: {0}")]
    Inhibitor(String),
}

/// Output captured from direct execution of `custom_command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout_truncated: String,
    pub stderr_truncated: String,
}

/// Outcome of terminal host power action execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PowerActionResult {
    Executed,
    RefusedInhibited(String),
    Failed(String),
}

/// Notification interaction or dismissal signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    ActionInvoked { id: u32, action_key: String },
    NotificationClosed { id: u32, reason: u32 },
}

/// Boundary trait for desktop and system automation primitives (ADR-0006, ADR-0007).
pub trait ActionExecutor: Send + Sync {
    /// Discovers active MPRIS2 players with `PlaybackStatus == "Playing"` and `CanPause == true`,
    /// filters out `ignore_players` (case-insensitive prefix/exact), and concurrently calls `Pause()`.
    /// Returns the list of successfully paused bus names.
    fn pause_mpris_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<String>, ActionError>>;

    /// Checks whether any MPRIS2 players with `PlaybackStatus == "Playing"` are active
    /// without sending `Pause()` or causing side effects.
    fn has_playing_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<bool, ActionError>>;

    /// Direct `exec` of argv command without a shell, injecting BPMute environment variables
    /// with bounded execution timeout.
    fn run_custom_command<'a>(
        &'a self,
        command: &'a [String],
        env: &'a [(&'a str, &'a str)],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<CommandOutput, ActionError>>;

    /// Locks active desktop session via `logind` `Session.Lock()` with fallback to `Manager.LockSessions()`.
    fn lock_screen<'a>(&'a self) -> BoxFuture<'a, Result<(), ActionError>>;

    /// Executes unprivileged `logind` `Suspend(false)` or `PowerOff(false)`.
    fn execute_power_action<'a>(
        &'a self,
        action: PowerAction,
    ) -> BoxFuture<'a, Result<PowerActionResult, ActionError>>;

    /// Monitors paused players for up to 5 minutes (300s).
    /// Returns `true` if any player resumed playback (`PlaybackStatus == "Playing"`),
    /// or `false` on clean completion / timeout.
    fn watch_mpris_resume<'a>(&'a self, paused_players: &'a [String]) -> BoxFuture<'a, bool>;
}

/// Boundary trait for interactive desktop notification management (ADR-0006).
pub trait Notifier: Send + Sync {
    /// Emits or updates the countdown notice using `replaces_id`, `transient = true`, and `expire_timeout = 0`.
    fn show_grace_notice<'a>(
        &'a self,
        replaces_id: u32,
        remaining_sec: u32,
        total_sec: u32,
        urgency: Urgency,
        dry_run: bool,
    ) -> BoxFuture<'a, Result<u32, ActionError>>;

    /// Dismisses the active countdown notice via `CloseNotification(id)`.
    fn close_notice<'a>(&'a self, id: u32) -> BoxFuture<'a, Result<(), ActionError>>;

    /// Shows a persistent (`transient = false`) warning notice.
    fn show_warning<'a>(
        &'a self,
        summary: &'a str,
        body: &'a str,
        transient: bool,
    ) -> BoxFuture<'a, Result<u32, ActionError>>;

    /// Shows an informational notice (e.g. asymmetric `Signal Lost` notice).
    fn show_info<'a>(
        &'a self,
        summary: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<u32, ActionError>>;

    /// Subscribes to notification signals (`ActionInvoked`, `NotificationClosed`).
    fn subscribe_actions(&self) -> mpsc::Receiver<NotificationEvent>;
}

/// Pure helper checking whether a player bus name matches an ignore list (case-insensitive prefix or exact).
pub fn is_player_ignored(bus_name: &str, ignore_list: &[String]) -> bool {
    let lower_bus = bus_name.to_ascii_lowercase();
    let short_name = bus_name
        .strip_prefix("org.mpris.MediaPlayer2.")
        .unwrap_or(bus_name)
        .to_ascii_lowercase();

    for ignore in ignore_list {
        let pattern = ignore.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            continue;
        }
        if lower_bus == pattern
            || lower_bus.starts_with(&pattern)
            || short_name == pattern
            || short_name.starts_with(&pattern)
        {
            return true;
        }
    }
    false
}
