use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection as WlConnection, Dispatch, QueueHandle};
use x11rb::connection::Connection as X11Connection;

use crate::paths::EnvSource;

use super::traits::{
    BoxFuture, DisplayEnvironment, IdleBackendKind, IdleProbe, IdleProbeError, SystemCapabilities,
    parse_systemd_environment, select_idle_backend,
};

#[zbus::proxy(
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1",
    interface = "org.freedesktop.systemd1.Manager"
)]
trait Systemd1Manager {
    #[zbus(property)]
    fn environment(&self) -> zbus::Result<Vec<String>>;
}

#[zbus::proxy(
    default_service = "org.gnome.Mutter.IdleMonitor",
    default_path = "/org/gnome/Mutter/IdleMonitor/Core",
    interface = "org.gnome.Mutter.IdleMonitor"
)]
trait MutterIdleMonitor {
    fn get_idletime(&self) -> zbus::Result<u64>;
    fn add_user_active_watch(&self) -> zbus::Result<u32>;
    fn remove_watch(&self, id: u32) -> zbus::Result<()>;
    #[zbus(signal)]
    fn watch_fired(&self, id: u32) -> zbus::Result<()>;
}

#[derive(Default)]
struct WaylandRegistryState {
    has_ext_idle_v2: bool,
    has_ext_idle_v1: bool,
}

impl Dispatch<WlRegistry, ()> for WaylandRegistryState {
    fn event(
        state: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            interface, version, ..
        } = event
            && interface == "ext_idle_notifier_v1"
        {
            if version >= 2 {
                state.has_ext_idle_v2 = true;
            } else {
                state.has_ext_idle_v1 = true;
            }
        }
    }
}

/// Concrete production desktop user activity and idle probe chain (ADR-0006).
pub struct SystemIdleProbe {
    env: Arc<dyn EnvSource>,
    cached_backend: Mutex<Option<IdleBackendKind>>,
    cached_display_env: Mutex<Option<DisplayEnvironment>>,
}

impl SystemIdleProbe {
    /// Creates a new system idle probe configured with the injected environment source.
    pub fn new(env: Arc<dyn EnvSource>) -> Self {
        Self {
            env,
            cached_backend: Mutex::new(None),
            cached_display_env: Mutex::new(None),
        }
    }

    /// Lazily discovers display server environment variables (`WAYLAND_DISPLAY`, `DISPLAY`),
    /// falling back to session D-Bus `systemd1.Manager.Environment` when absent at boot without spawning subprocesses.
    pub async fn discover_display_environment(&self) -> DisplayEnvironment {
        let mut guard = self.cached_display_env.lock().await;
        if let Some(ref env) = *guard {
            return env.clone();
        }

        let mut wayland = self.env.get("WAYLAND_DISPLAY");
        let mut display = self.env.get("DISPLAY");

        if (wayland.is_none() || display.is_none())
            && let Ok(conn) = self.session_bus().await
            && let Ok(proxy) = Systemd1ManagerProxy::new(&conn).await
            && let Ok(sys_env) = proxy.environment().await
        {
            let discovered = parse_systemd_environment(&sys_env);
            if wayland.is_none() {
                wayland = discovered.wayland_display;
            }
            if display.is_none() {
                display = discovered.display;
            }
        }

        let result = DisplayEnvironment {
            wayland_display: wayland,
            display,
        };
        *guard = Some(result.clone());
        result
    }

    async fn session_bus(&self) -> zbus::Result<zbus::Connection> {
        if let Some(addr) = self.env.get("DBUS_SESSION_BUS_ADDRESS") {
            zbus::connection::Builder::address(addr.as_str())?
                .build()
                .await
        } else {
            zbus::Connection::session().await
        }
    }

    /// Detects system desktop idle capabilities across Wayland, GNOME Mutter, and X11 in strict priority order.
    pub async fn probe_system_capabilities(
        &self,
        display_env: &DisplayEnvironment,
    ) -> (SystemCapabilities, IdleBackendKind) {
        let mut caps = SystemCapabilities::default();

        // 1. Wayland probe
        if let Some(ref _w_display) = display_env.wayland_display
            && let Ok(conn) = WlConnection::connect_to_env()
        {
            let display = conn.display();
            let mut event_queue = conn.new_event_queue();
            let qh = event_queue.handle();
            let _ = display.get_registry(&qh, ());
            let mut state = WaylandRegistryState::default();
            if event_queue.roundtrip(&mut state).is_ok() {
                if state.has_ext_idle_v2 {
                    caps.wayland_ext_idle_version = Some(2);
                } else if state.has_ext_idle_v1 {
                    caps.wayland_ext_idle_version = Some(1);
                }
            }
        }

        // 2. GNOME Mutter probe via session D-Bus
        if let Ok(conn) = self.session_bus().await
            && let Ok(proxy) = MutterIdleMonitorProxy::new(&conn).await
            && proxy.get_idletime().await.is_ok()
        {
            caps.gnome_mutter_available = true;
        }

        // 3. Native X11 probe
        if display_env.wayland_display.is_none()
            && let Some(ref d_str) = display_env.display
            && let Ok((conn, _)) = x11rb::connect(Some(d_str.as_str()))
        {
            use x11rb::protocol::screensaver::ConnectionExt as _;
            if conn.screensaver_query_version(1, 1).is_ok() {
                caps.x11_available = true;
            }
        }

        let backend = select_idle_backend(display_env, &caps);
        (caps, backend)
    }

    async fn resolve_backend(&self) -> IdleBackendKind {
        let mut guard = self.cached_backend.lock().await;
        if let Some(b) = *guard {
            return b;
        }

        let display_env = self.discover_display_environment().await;
        let (_caps, backend) = self.probe_system_capabilities(&display_env).await;
        *guard = Some(backend);
        backend
    }
}

impl IdleProbe for SystemIdleProbe {
    fn backend_kind(&self) -> IdleBackendKind {
        if let Ok(guard) = self.cached_backend.try_lock()
            && let Some(b) = *guard
        {
            return b;
        }
        IdleBackendKind::Degraded
    }

    fn is_user_active_trailing<'a>(
        &'a self,
        window: Duration,
    ) -> BoxFuture<'a, Result<bool, IdleProbeError>> {
        Box::pin(async move {
            let backend = self.resolve_backend().await;
            match backend {
                IdleBackendKind::WaylandExtIdleV2 | IdleBackendKind::WaylandExtIdleV1 => {
                    // Wayland pre-gate check: query via Mutter if available, or pass pre-gate
                    if let Ok(conn) = self.session_bus().await
                        && let Ok(proxy) = MutterIdleMonitorProxy::new(&conn).await
                        && let Ok(idle_ms) = proxy.get_idletime().await
                    {
                        return Ok(idle_ms < window.as_millis() as u64);
                    }
                    Ok(false)
                }
                IdleBackendKind::GnomeMutter => {
                    let conn = self
                        .session_bus()
                        .await
                        .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
                    let proxy = MutterIdleMonitorProxy::new(&conn)
                        .await
                        .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
                    let idle_ms = proxy
                        .get_idletime()
                        .await
                        .map_err(|e| IdleProbeError::Protocol(e.to_string()))?;
                    Ok(idle_ms < window.as_millis() as u64)
                }
                IdleBackendKind::X11 => {
                    let d_env = self.discover_display_environment().await;
                    let display_str = d_env.display.unwrap_or_else(|| ":0".to_string());
                    let (conn, screen_num) = x11rb::connect(Some(display_str.as_str()))
                        .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
                    let root = conn.setup().roots[screen_num].root;
                    use x11rb::protocol::screensaver::ConnectionExt as _;
                    let reply = conn
                        .screensaver_query_info(root)
                        .map_err(|e| IdleProbeError::Protocol(e.to_string()))?
                        .reply()
                        .map_err(|e| IdleProbeError::Protocol(e.to_string()))?;
                    let idle_ms = reply.ms_since_user_input as u64;
                    Ok(idle_ms < window.as_millis() as u64)
                }
                IdleBackendKind::Degraded => Ok(false),
            }
        })
    }

    fn watch_grace_period_activity<'a>(
        &'a self,
        settle_delay: Duration,
    ) -> BoxFuture<'a, Result<mpsc::Receiver<()>, IdleProbeError>> {
        Box::pin(async move {
            let backend = self.resolve_backend().await;
            let (tx, rx) = mpsc::channel(16);
            let start = tokio::time::Instant::now();

            match backend {
                IdleBackendKind::GnomeMutter => {
                    let conn_res = self.session_bus().await;
                    tokio::spawn(async move {
                        let Ok(conn) = conn_res else {
                            return;
                        };
                        let Ok(proxy) = MutterIdleMonitorProxy::new(&conn).await else {
                            return;
                        };
                        let Ok(watch_id) = proxy.add_user_active_watch().await else {
                            return;
                        };
                        let mut stream = match proxy.receive_watch_fired().await {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        while let Some(signal) = stream.next().await {
                            if let Ok(fired_id) = signal.args()
                                && fired_id.id == watch_id
                            {
                                if tokio::time::Instant::now().duration_since(start) >= settle_delay
                                {
                                    let _ = tx.send(()).await;
                                    break;
                                } else {
                                    tracing::debug!(
                                        "Mutter activity fired within settle delay; ignoring"
                                    );
                                }
                            }
                        }
                    });
                    Ok(rx)
                }
                IdleBackendKind::X11 => {
                    let d_env = self.discover_display_environment().await;
                    let display_str = d_env.display.unwrap_or_else(|| ":0".to_string());
                    tokio::spawn(async move {
                        let Ok((conn, screen_num)) = x11rb::connect(Some(display_str.as_str()))
                        else {
                            return;
                        };
                        let root = conn.setup().roots[screen_num].root;
                        use x11rb::protocol::screensaver::ConnectionExt as _;
                        let mut prev_idle: Option<u32> = None;
                        loop {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                            let Ok(cookie) = conn.screensaver_query_info(root) else {
                                break;
                            };
                            let Ok(reply) = cookie.reply() else {
                                break;
                            };
                            if let Some(prev) = prev_idle
                                && reply.ms_since_user_input < prev
                                && tokio::time::Instant::now().duration_since(start) >= settle_delay
                            {
                                let _ = tx.send(()).await;
                                break;
                            }
                            prev_idle = Some(reply.ms_since_user_input);
                        }
                    });
                    Ok(rx)
                }
                IdleBackendKind::WaylandExtIdleV2 | IdleBackendKind::WaylandExtIdleV1 => {
                    // Mutter watch on Wayland if GNOME Mutter is present
                    if let Ok(conn) = self.session_bus().await
                        && let Ok(proxy) = MutterIdleMonitorProxy::new(&conn).await
                    {
                        tokio::spawn(async move {
                            let Ok(watch_id) = proxy.add_user_active_watch().await else {
                                return;
                            };
                            let mut stream = match proxy.receive_watch_fired().await {
                                Ok(s) => s,
                                Err(_) => return,
                            };
                            while let Some(signal) = stream.next().await {
                                if let Ok(fired_id) = signal.args()
                                    && fired_id.id == watch_id
                                    && tokio::time::Instant::now().duration_since(start)
                                        >= settle_delay
                                {
                                    let _ = tx.send(()).await;
                                    break;
                                }
                            }
                        });
                    }
                    Ok(rx)
                }
                IdleBackendKind::Degraded => Ok(rx),
            }
        })
    }

    fn watch_post_trigger_activity<'a>(&'a self, timeout: Duration) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut rx = match self.watch_grace_period_activity(Duration::ZERO).await {
                Ok(r) => r,
                Err(_) => return false,
            };
            tokio::select! {
                biased;
                Some(()) = rx.recv() => true,
                _ = tokio::time::sleep(timeout) => false,
            }
        })
    }
}
