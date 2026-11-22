use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::Interest;
use tokio::net::UnixStream as TokioUnixStream;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection as WlConnection, Dispatch, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::{
    self, ExtIdleNotifierV1,
};

use crate::paths::EnvSource;

use super::traits::{IdleBackendKind, IdleProbeError};

/// Connects to the Wayland display socket using the injected `EnvSource` instead of global process env.
pub(crate) fn connect_wayland_socket(
    env: &dyn EnvSource,
    wayland_display: &str,
) -> Result<UnixStream, IdleProbeError> {
    let display_path = PathBuf::from(wayland_display);
    let socket_path = if display_path.is_absolute() {
        display_path
    } else {
        let runtime_dir = env
            .get("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or_else(|| {
                IdleProbeError::Connection(
                    "XDG_RUNTIME_DIR missing or relative for Wayland socket".to_string(),
                )
            })?;
        runtime_dir.join(display_path)
    };
    UnixStream::connect(&socket_path).map_err(|e| IdleProbeError::Connection(e.to_string()))
}

/// Pure deterministic state tracker for a persistent `ext_idle_notification_v1` subscription.
#[derive(Debug, Clone)]
pub(crate) struct WaylandIdleTrackerState {
    notification_timeout: Duration,
    idled_at: Option<Instant>,
    connected: bool,
}

impl WaylandIdleTrackerState {
    pub(crate) fn new(notification_timeout: Duration) -> Self {
        Self {
            notification_timeout,
            idled_at: None,
            connected: true,
        }
    }

    pub(crate) fn on_idled(&mut self, now: Instant) {
        self.idled_at = Some(now);
    }

    pub(crate) fn on_resumed(&mut self) {
        self.idled_at = None;
    }

    pub(crate) fn on_disconnected(&mut self) {
        self.connected = false;
        self.idled_at = None;
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.connected
    }

    /// Returns `Some(true)` if user activity occurred within `window`, `Some(false)` if continuously
    /// idle for at least `window`, or `None` if the compositor monitor disconnected.
    pub(crate) fn is_user_active_trailing(&self, window: Duration, now: Instant) -> Option<bool> {
        if !self.connected {
            return None;
        }
        match self.idled_at {
            Some(idled_at) => {
                let idle_for = now
                    .saturating_duration_since(idled_at)
                    .saturating_add(self.notification_timeout);
                Some(idle_for < window)
            }
            None => Some(true),
        }
    }
}

struct WaylandMonitorDispatchState {
    seat: Option<WlSeat>,
    notifier: Option<ExtIdleNotifierV1>,
    prefer_v2: bool,
    bound_v2: bool,
    shared: Arc<StdMutex<WaylandIdleTrackerState>>,
    activity_tx: broadcast::Sender<()>,
}

impl Dispatch<WlRegistry, ()> for WaylandMonitorDispatchState {
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
            } else if interface == "ext_idle_notifier_v1" && state.notifier.is_none() {
                let bind_v2 = state.prefer_v2 && version >= 2;
                let bind_version = if bind_v2 { 2 } else { 1 };
                state.bound_v2 = bind_v2;
                state.notifier = Some(registry.bind(name, bind_version, qh, ()));
            }
        }
    }
}

impl Dispatch<WlSeat, ()> for WaylandMonitorDispatchState {
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

impl Dispatch<ExtIdleNotifierV1, ()> for WaylandMonitorDispatchState {
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

impl Dispatch<ExtIdleNotificationV1, ()> for WaylandMonitorDispatchState {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &WlConnection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                state.shared.lock().unwrap().on_idled(Instant::now());
            }
            ext_idle_notification_v1::Event::Resumed => {
                state.shared.lock().unwrap().on_resumed();
                let _ = state.activity_tx.send(());
            }
            _ => {}
        }
    }
}

/// Persistent Wayland `ext-idle-notify-v1` monitor tracking idle onset and input resumption.
pub(crate) struct WaylandIdleMonitor {
    shared: Arc<StdMutex<WaylandIdleTrackerState>>,
    activity_tx: broadcast::Sender<()>,
    task: JoinHandle<()>,
}

impl WaylandIdleMonitor {
    pub(crate) fn connect(
        env: &dyn EnvSource,
        wayland_display: &str,
        backend: IdleBackendKind,
        notification_timeout: Duration,
    ) -> Result<Self, IdleProbeError> {
        let stream = connect_wayland_socket(env, wayland_display)?;
        let async_stream = stream
            .try_clone()
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
        let conn = WlConnection::from_socket(stream)
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;

        let shared = Arc::new(StdMutex::new(WaylandIdleTrackerState::new(
            notification_timeout,
        )));
        let (activity_tx, _) = broadcast::channel(32);

        let mut dispatch_state = WaylandMonitorDispatchState {
            seat: None,
            notifier: None,
            prefer_v2: backend == IdleBackendKind::WaylandExtIdleV2,
            bound_v2: false,
            shared: Arc::clone(&shared),
            activity_tx: activity_tx.clone(),
        };

        let display = conn.display();
        let mut event_queue = conn.new_event_queue();
        let qh = event_queue.handle();
        let _ = display.get_registry(&qh, ());
        event_queue
            .roundtrip(&mut dispatch_state)
            .map_err(|e| IdleProbeError::Protocol(e.to_string()))?;

        let (Some(notifier), Some(seat)) =
            (dispatch_state.notifier.take(), dispatch_state.seat.take())
        else {
            return Err(IdleProbeError::Unavailable(
                "Wayland seat or ext_idle_notifier_v1 unavailable".to_string(),
            ));
        };

        let timeout_ms = notification_timeout
            .as_millis()
            .clamp(1, u128::from(u32::MAX)) as u32;
        let notification = if dispatch_state.bound_v2 {
            notifier.get_input_idle_notification(timeout_ms, &seat, &qh, ())
        } else {
            notifier.get_idle_notification(timeout_ms, &seat, &qh, ())
        };
        conn.flush()
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;

        // Switch socket to non-blocking only after the initial synchronous registry roundtrip.
        async_stream
            .set_nonblocking(true)
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;
        let tokio_stream = TokioUnixStream::from_std(async_stream)
            .map_err(|e| IdleProbeError::Connection(e.to_string()))?;

        let shared_bg = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            let _keep_notifier = notifier;
            let _keep_seat = seat;
            let _keep_notification = notification;

            loop {
                if event_queue.dispatch_pending(&mut dispatch_state).is_err() {
                    break;
                }
                if conn.flush().is_err() {
                    break;
                }
                let Some(guard) = event_queue.prepare_read() else {
                    continue;
                };
                if tokio_stream.readable().await.is_err() {
                    break;
                }
                match tokio_stream.try_io(Interest::READABLE, || match guard.read() {
                    Ok(_) => Ok(Ok(())),
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        Err(e)
                    }
                    Err(e) => Ok(Err(e)),
                }) {
                    Ok(Ok(())) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Ok(Err(_)) | Err(_) => break,
                }
            }

            shared_bg.lock().unwrap().on_disconnected();
        });

        Ok(Self {
            shared,
            activity_tx,
            task,
        })
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.shared.lock().unwrap().is_connected()
    }

    pub(crate) fn is_user_active_trailing(&self, window: Duration) -> Option<bool> {
        self.shared
            .lock()
            .unwrap()
            .is_user_active_trailing(window, Instant::now())
    }

    pub(crate) fn subscribe_activity(&self) -> broadcast::Receiver<()> {
        self.activity_tx.subscribe()
    }
}

impl Drop for WaylandIdleMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn tracker_state_enforces_exact_trailing_window_and_resets() {
        let mut tracker = WaylandIdleTrackerState::new(Duration::from_secs(1));
        let window = Duration::from_secs(120);

        // Before `idled` fires, user is active.
        assert_eq!(
            tracker.is_user_active_trailing(window, Instant::now()),
            Some(true)
        );

        // Compositor signals `idled` after 1s notification timeout.
        tracker.on_idled(Instant::now());
        tokio::time::advance(Duration::from_secs(118)).await;
        // Total idle = 1s + 118s = 119s < 120s -> still within trailing activity window.
        assert_eq!(
            tracker.is_user_active_trailing(window, Instant::now()),
            Some(true)
        );

        tokio::time::advance(Duration::from_secs(1)).await;
        // Total idle = 1s + 119s = 120s -> not active in trailing 120s window.
        assert_eq!(
            tracker.is_user_active_trailing(window, Instant::now()),
            Some(false)
        );

        // User input resumes -> immediately active again.
        tracker.on_resumed();
        assert_eq!(
            tracker.is_user_active_trailing(window, Instant::now()),
            Some(true)
        );

        // Subsequent idle cycle becomes inactive again after 120s.
        tracker.on_idled(Instant::now());
        tokio::time::advance(Duration::from_secs(119)).await;
        assert_eq!(
            tracker.is_user_active_trailing(window, Instant::now()),
            Some(false)
        );

        // Disconnect returns None so caller knows to reconnect.
        tracker.on_disconnected();
        assert_eq!(
            tracker.is_user_active_trailing(window, Instant::now()),
            None
        );
    }
}
