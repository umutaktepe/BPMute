use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection as WlConnection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::{
    self, ExtIdleNotifierV1,
};
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
struct WaylandIdleState {
    seat: Option<WlSeat>,
    notifier: Option<ExtIdleNotifierV1>,
    has_ext_idle_v2: bool,
    has_ext_idle_v1: bool,
    idled: bool,
    resumed: bool,
}

impl Dispatch<WlRegistry, ()> for WaylandIdleState {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &WlConnection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == "wl_seat" && state.seat.is_none() {
                state.seat = Some(registry.bind(name, 1, qh, ()));
            } else if interface == "ext_idle_notifier_v1" {
                if version >= 2 {
                    state.has_ext_idle_v2 = true;
                    state.notifier = Some(registry.bind(name, 2, qh, ()));
                } else {
                    state.has_ext_idle_v1 = true;
                    state.notifier = Some(registry.bind(name, 1, qh, ()));
                }
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandIdleState {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wayland_client::protocol::wl_seat::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for WaylandIdleState {
    fn event(
        _: &mut Self,
        _: &ExtIdleNotifierV1,
        _: ext_idle_notifier_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for WaylandIdleState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => state.idled = true,
            ext_idle_notification_v1::Event::Resumed => state.resumed = true,
            _ => {}
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

    async fn mutter_idletime(&self) -> Result<u64, IdleProbeError> {
        let conn = self
            .session_bus()
            .await
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
        let proxy = MutterIdleMonitorProxy::new(&conn)
            .await
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
        proxy
            .get_idletime()
            .await
            .map_err(|e| IdleProbeError::Protocol(e.to_string()))
    }

    async fn spawn_mutter_active_watch(
        &self,
        tx: mpsc::Sender<()>,
        start: tokio::time::Instant,
        settle_delay: Duration,
    ) {
        let Ok(conn) = self.session_bus().await else {
            return;
        };
        let Ok(proxy) = MutterIdleMonitorProxy::new(&conn).await else {
            return;
        };
        let Ok(mut watch_id) = proxy.add_user_active_watch().await else {
            return;
        };
        let Ok(mut stream) = proxy.receive_watch_fired().await else {
            return;
        };
        tokio::spawn(async move {
            while let Some(signal) = stream.next().await {
                if let Ok(fired_id) = signal.args()
                    && fired_id.id == watch_id
                {
                    if tokio::time::Instant::now().duration_since(start) >= settle_delay {
                        let _ = tx.send(()).await;
                        let _ = proxy.remove_watch(watch_id).await;
                        break;
                    } else {
                        tracing::debug!(
                            "Mutter user activity fired within settle delay; re-arming one-shot watch"
                        );
                        if let Ok(new_id) = proxy.add_user_active_watch().await {
                            watch_id = new_id;
                        }
                    }
                }
            }
        });
    }

    /// Detects system desktop idle capabilities across Wayland, GNOME Mutter, and X11 in strict priority order.
    pub async fn probe_system_capabilities(
        &self,
        display_env: &DisplayEnvironment,
    ) -> (SystemCapabilities, IdleBackendKind) {
        let mut caps = SystemCapabilities::default();

        // 1. Wayland probe
        if let Some(ref w_display) = display_env.wayland_display {
            tracing::debug!("probing Wayland compositor capabilities on {w_display}");
            if let Ok(conn) = WlConnection::connect_to_env() {
                let display = conn.display();
                let mut event_queue = conn.new_event_queue();
                let qh = event_queue.handle();
                let _ = display.get_registry(&qh, ());
                let mut state = WaylandIdleState::default();
                if event_queue.roundtrip(&mut state).is_ok() {
                    if state.has_ext_idle_v2 {
                        caps.wayland_ext_idle_version = Some(2);
                    } else if state.has_ext_idle_v1 {
                        caps.wayland_ext_idle_version = Some(1);
                    }
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

    fn resolve_backend<'a>(&'a self) -> BoxFuture<'a, IdleBackendKind> {
        Box::pin(async move { self.resolve_backend().await })
    }

    fn is_user_active_trailing<'a>(
        &'a self,
        window: Duration,
    ) -> BoxFuture<'a, Result<bool, IdleProbeError>> {
        Box::pin(async move {
            let backend = self.resolve_backend().await;
            match backend {
                IdleBackendKind::WaylandExtIdleV2 | IdleBackendKind::WaylandExtIdleV1 => {
                    // Try Mutter D-Bus first if available on this compositor
                    if let Ok(idle_ms) = self.mutter_idletime().await {
                        return Ok(idle_ms < window.as_millis() as u64);
                    }
                    // Native Wayland ext-idle-notify-v1 probe
                    if let Ok(conn) = WlConnection::connect_to_env() {
                        let display = conn.display();
                        let mut event_queue = conn.new_event_queue();
                        let qh = event_queue.handle();
                        let _ = display.get_registry(&qh, ());
                        let mut state = WaylandIdleState::default();
                        if event_queue.roundtrip(&mut state).is_ok()
                            && let (Some(notifier), Some(seat)) =
                                (state.notifier.take(), state.seat.take())
                        {
                            let window_ms = window.as_millis() as u32;
                            let _notif = if backend == IdleBackendKind::WaylandExtIdleV2 {
                                notifier.get_input_idle_notification(window_ms, &seat, &qh, ())
                            } else {
                                notifier.get_idle_notification(window_ms, &seat, &qh, ())
                            };
                            let _ = event_queue.roundtrip(&mut state);
                            // If compositor delivers Idled, user was idle for >= window
                            return Ok(!state.idled);
                        }
                    }
                    Ok(false)
                }
                IdleBackendKind::GnomeMutter => {
                    let idle_ms = self.mutter_idletime().await?;
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
                    self.spawn_mutter_active_watch(tx, start, settle_delay)
                        .await;
                    Ok(rx)
                }
                IdleBackendKind::WaylandExtIdleV2 | IdleBackendKind::WaylandExtIdleV1 => {
                    // Mutter watch on GNOME Wayland if GNOME Mutter is present
                    if let Ok(conn) = self.session_bus().await
                        && let Ok(proxy) = MutterIdleMonitorProxy::new(&conn).await
                        && proxy.get_idletime().await.is_ok()
                    {
                        self.spawn_mutter_active_watch(tx, start, settle_delay)
                            .await;
                    } else {
                        // Native Wayland ext-idle-notify-v1 notification watch (Sway/Hyprland)
                        tokio::task::spawn_blocking(move || {
                            let Ok(conn) = WlConnection::connect_to_env() else {
                                return;
                            };
                            let display = conn.display();
                            let mut event_queue = conn.new_event_queue();
                            let qh = event_queue.handle();
                            let _ = display.get_registry(&qh, ());
                            let mut state = WaylandIdleState::default();
                            if event_queue.roundtrip(&mut state).is_err() {
                                return;
                            }
                            let (Some(notifier), Some(seat)) =
                                (state.notifier.take(), state.seat.take())
                            else {
                                return;
                            };
                            let _notif = if backend == IdleBackendKind::WaylandExtIdleV2 {
                                notifier.get_input_idle_notification(1000, &seat, &qh, ())
                            } else {
                                notifier.get_idle_notification(1000, &seat, &qh, ())
                            };
                            loop {
                                if event_queue.blocking_dispatch(&mut state).is_err() {
                                    break;
                                }
                                if state.resumed {
                                    if tokio::time::Instant::now().duration_since(start)
                                        >= settle_delay
                                    {
                                        let _ = tx.blocking_send(());
                                        break;
                                    } else {
                                        state.resumed = false;
                                    }
                                }
                            }
                        });
                    }
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

                        use crate::idle::jitter::{InputEvent, PointerJitterFilter};
                        use x11rb::protocol::Event;
                        use x11rb::protocol::xinput::{
                            ConnectionExt as _, Device, EventMask, XIEventMask,
                        };

                        let has_xi2 = conn.xinput_xi_query_version(2, 0).is_ok();
                        if has_xi2 {
                            let mask = vec![
                                XIEventMask::RAW_MOTION,
                                XIEventMask::RAW_BUTTON_PRESS,
                                XIEventMask::RAW_KEY_PRESS,
                            ];
                            let em = EventMask {
                                deviceid: Device::ALL_MASTER.into(),
                                mask,
                            };
                            if conn.xinput_xi_select_events(root, &[em]).is_ok() {
                                let mut filter =
                                    PointerJitterFilter::new(25.0, Duration::from_secs(2));
                                loop {
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                    while let Ok(Some(event)) = conn.poll_for_event() {
                                        let now_ms = tokio::time::Instant::now()
                                            .duration_since(start)
                                            .as_millis()
                                            as i64;
                                        let is_active = match event {
                                            Event::XinputRawKeyPress(_) => {
                                                filter.record_event(InputEvent::RawKeyPress, now_ms)
                                            }
                                            Event::XinputRawButtonPress(_) => filter
                                                .record_event(InputEvent::RawButtonPress, now_ms),
                                            Event::XinputRawMotion(m) => {
                                                let (dx, dy) = if m.axisvalues_raw.len() >= 2 {
                                                    (
                                                        m.axisvalues_raw[0].integral as f64,
                                                        m.axisvalues_raw[1].integral as f64,
                                                    )
                                                } else if !m.axisvalues_raw.is_empty() {
                                                    (m.axisvalues_raw[0].integral as f64, 0.0)
                                                } else {
                                                    (0.0, 0.0)
                                                };
                                                filter.record_event(
                                                    InputEvent::RawMotion { dx, dy },
                                                    now_ms,
                                                )
                                            }
                                            _ => false,
                                        };
                                        if is_active
                                            && tokio::time::Instant::now().duration_since(start)
                                                >= settle_delay
                                        {
                                            let _ = tx.send(()).await;
                                            return;
                                        }
                                    }
                                }
                            }
                        }

                        // Fallback: 500 ms MIT-SCREEN-SAVER polling if XInput2 is unavailable
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
