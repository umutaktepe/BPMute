use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{Mutex, broadcast, mpsc};
use zbus::proxy;
use zbus::zvariant::{OwnedFd, Value};

use crate::config::PowerAction;

use super::traits::{
    ActionError, ActionExecutor, BoxFuture, CommandOutput, GraceNoticeSpec, GracePhase,
    NotificationEvent, Notifier, PowerActionResult, Urgency, is_player_ignored,
};

#[proxy(
    default_service = "org.freedesktop.DBus",
    default_path = "/org/freedesktop/DBus",
    interface = "org.freedesktop.DBus"
)]
trait DBus1 {
    fn list_names(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait MediaPlayer2Player {
    fn pause(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn can_pause(&self) -> zbus::Result<bool>;
}

#[proxy(
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications",
    interface = "org.freedesktop.Notifications"
)]
trait FreedesktopNotifications {
    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[allow(
        clippy::too_many_arguments,
        reason = "org.freedesktop.Notifications.Notify interface defines 8 parameters"
    )]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

#[proxy(
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    interface = "org.freedesktop.login1.Manager"
)]
pub trait LogindManager {
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
    fn lock_sessions(&self) -> zbus::Result<()>;
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
    fn can_suspend(&self) -> zbus::Result<String>;
    fn can_power_off(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[proxy(
    default_service = "org.freedesktop.login1",
    interface = "org.freedesktop.login1.Session"
)]
trait LogindSession {
    fn lock(&self) -> zbus::Result<()>;
}

/// Production system action executor communicating over session and system D-Bus.
#[derive(Debug, Default, Clone)]
pub struct SystemActionExecutor;

impl ActionExecutor for SystemActionExecutor {
    fn pause_mpris_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<String>, ActionError>> {
        Box::pin(async move {
            let conn = match zbus::Connection::session().await {
                Ok(c) => c,
                Err(err) => return Err(ActionError::Mpris(format!("session bus error: {err}"))),
            };

            let dbus_proxy = match DBus1Proxy::new(&conn).await {
                Ok(p) => p,
                Err(err) => return Err(ActionError::Mpris(format!("DBus1Proxy error: {err}"))),
            };

            let names = match dbus_proxy.list_names().await {
                Ok(n) => n,
                Err(err) => return Err(ActionError::Mpris(format!("list_names error: {err}"))),
            };

            let mut targets = Vec::new();
            for name in names {
                if name.starts_with("org.mpris.MediaPlayer2.")
                    && !is_player_ignored(&name, ignore_players)
                {
                    targets.push(name);
                }
            }

            if targets.is_empty() {
                return Ok(Vec::new());
            }

            let mut tasks = Vec::new();
            for bus_name in targets {
                let c = conn.clone();
                tasks.push(tokio::spawn(async move {
                    let opt: Option<String> = async {
                        let builder = MediaPlayer2PlayerProxy::builder(&c)
                            .destination(bus_name.as_str())
                            .ok()?;
                        let player_proxy = builder.build().await.ok()?;

                        let status = tokio::time::timeout(
                            Duration::from_secs(2),
                            player_proxy.playback_status(),
                        )
                        .await
                        .ok()?
                        .ok()?;

                        if status != "Playing" {
                            return None;
                        }

                        if let Ok(Ok(can_pause)) =
                            tokio::time::timeout(Duration::from_secs(1), player_proxy.can_pause())
                                .await
                            && !can_pause
                        {
                            return None;
                        }

                        match tokio::time::timeout(Duration::from_secs(2), player_proxy.pause())
                            .await
                        {
                            Ok(Ok(())) => Some(bus_name),
                            _ => None,
                        }
                    }
                    .await;
                    opt
                }));
            }

            let mut paused = Vec::new();
            for task in tasks {
                if let Ok(Some(name)) = task.await {
                    paused.push(name);
                }
            }

            Ok(paused)
        })
    }

    fn has_playing_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<bool, ActionError>> {
        Box::pin(async move {
            let count = self.count_playing_players(ignore_players).await?;
            Ok(count > 0)
        })
    }

    fn count_playing_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<usize, ActionError>> {
        Box::pin(async move {
            let conn = match zbus::Connection::session().await {
                Ok(c) => c,
                Err(err) => return Err(ActionError::Mpris(format!("session bus error: {err}"))),
            };

            let dbus_proxy = match DBus1Proxy::new(&conn).await {
                Ok(p) => p,
                Err(err) => return Err(ActionError::Mpris(format!("DBus1Proxy error: {err}"))),
            };

            let names = match dbus_proxy.list_names().await {
                Ok(n) => n,
                Err(err) => return Err(ActionError::Mpris(format!("list_names error: {err}"))),
            };

            let mut targets = Vec::new();
            for name in names {
                if name.starts_with("org.mpris.MediaPlayer2.")
                    && !is_player_ignored(&name, ignore_players)
                {
                    targets.push(name);
                }
            }

            if targets.is_empty() {
                return Ok(0);
            }

            let mut tasks = Vec::new();
            for bus_name in targets {
                let c = conn.clone();
                tasks.push(tokio::spawn(async move {
                    let opt: Option<bool> = async {
                        let builder = MediaPlayer2PlayerProxy::builder(&c)
                            .destination(bus_name.as_str())
                            .ok()?;
                        let player_proxy = builder.build().await.ok()?;

                        let status = tokio::time::timeout(
                            Duration::from_secs(2),
                            player_proxy.playback_status(),
                        )
                        .await
                        .ok()?
                        .ok()?;

                        Some(status == "Playing")
                    }
                    .await;
                    opt
                }));
            }

            let mut count = 0_usize;
            for task in tasks {
                if let Ok(Some(true)) = task.await {
                    count += 1;
                }
            }

            Ok(count)
        })
    }

    fn run_custom_command<'a>(
        &'a self,
        command: &'a [String],
        env: &'a [(&'a str, &'a str)],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<CommandOutput, ActionError>> {
        Box::pin(async move {
            let Some(prog) = command.first() else {
                return Err(ActionError::CustomCommand(
                    "empty command vector".to_string(),
                ));
            };

            let mut cmd = tokio::process::Command::new(prog);
            cmd.args(&command[1..]);
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.kill_on_drop(true);

            let run_future = cmd.output();
            let output = match tokio::time::timeout(timeout, run_future).await {
                Ok(Ok(out)) => out,
                Ok(Err(err)) => return Err(ActionError::CustomCommand(err.to_string())),
                Err(_) => return Err(ActionError::CustomCommandTimeout(timeout)),
            };

            let stdout_truncated =
                String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(4096)])
                    .to_string();
            let stderr_truncated =
                String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
                    .to_string();

            Ok(CommandOutput {
                exit_code: output.status.code(),
                success: output.status.success(),
                stdout_truncated,
                stderr_truncated,
            })
        })
    }

    fn lock_screen<'a>(&'a self) -> BoxFuture<'a, Result<(), ActionError>> {
        Box::pin(async move {
            let conn = match zbus::Connection::system().await {
                Ok(c) => c,
                Err(err) => {
                    return Err(ActionError::LockScreen(format!("system bus error: {err}")));
                }
            };

            // First attempt Session.Lock() on caller's active session, fallback to Manager.LockSessions()
            let session_proxy = LogindSessionProxy::builder(&conn)
                .destination("org.freedesktop.login1")
                .ok()
                .and_then(|b| b.path("/org/freedesktop/login1/session/auto").ok());

            let session_lock_ok = if let Some(sp) = session_proxy {
                if let Ok(proxy) = sp.build().await {
                    proxy.lock().await.is_ok()
                } else {
                    false
                }
            } else {
                false
            };

            if !session_lock_ok {
                let manager = match LogindManagerProxy::new(&conn).await {
                    Ok(m) => m,
                    Err(err) => {
                        return Err(ActionError::LockScreen(format!(
                            "LogindManagerProxy error: {err}"
                        )));
                    }
                };

                if let Err(err) = manager.lock_sessions().await {
                    return Err(ActionError::LockScreen(err.to_string()));
                }
            }

            Ok(())
        })
    }

    fn execute_power_action<'a>(
        &'a self,
        action: PowerAction,
    ) -> BoxFuture<'a, Result<PowerActionResult, ActionError>> {
        Box::pin(async move {
            let conn = match zbus::Connection::system().await {
                Ok(c) => c,
                Err(err) => {
                    return Err(ActionError::PowerAction(format!("system bus error: {err}")));
                }
            };

            let manager = match LogindManagerProxy::new(&conn).await {
                Ok(m) => m,
                Err(err) => {
                    return Err(ActionError::PowerAction(format!(
                        "LogindManagerProxy error: {err}"
                    )));
                }
            };

            let call_res = match action {
                PowerAction::Suspend => manager.suspend(false).await,
                PowerAction::Poweroff => manager.power_off(false).await,
                PowerAction::None => return Ok(PowerActionResult::Executed),
            };

            match call_res {
                Ok(()) => Ok(PowerActionResult::Executed),
                Err(err) => {
                    let err_str = err.to_string();
                    if err_str.contains("AccessDenied")
                        || err_str.contains("inhibited")
                        || err_str.contains("Inhibited")
                        || err_str.contains("Authorization")
                        || err_str.contains("not authorized")
                    {
                        Ok(PowerActionResult::RefusedInhibited(err_str))
                    } else {
                        Ok(PowerActionResult::Failed(err_str))
                    }
                }
            }
        })
    }

    fn watch_mpris_resume<'a>(&'a self, paused_players: &'a [String]) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let conn = match zbus::Connection::session().await {
                Ok(c) => c,
                Err(_) => return false,
            };

            let (resumed_tx, mut resumed_rx) = mpsc::channel::<()>(1);
            let mut join_handles = Vec::new();

            for target in paused_players {
                let c = conn.clone();
                let tx = resumed_tx.clone();
                let target_name = target.clone();
                join_handles.push(tokio::spawn(async move {
                    let builder = match MediaPlayer2PlayerProxy::builder(&c)
                        .destination(target_name.as_str())
                    {
                        Ok(b) => b,
                        Err(_) => return,
                    };
                    let proxy = match builder.build().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    let mut stream = proxy.receive_playback_status_changed().await;
                    use zbus::export::futures_util::StreamExt;
                    while let Some(change) = stream.next().await {
                        if let Ok(val) = change.get().await
                            && val == "Playing"
                        {
                            let _ = tx.send(()).await;
                            return;
                        }
                    }
                }));
            }
            drop(resumed_tx);

            let resumed = tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(300)) => false,
                res = resumed_rx.recv() => res.is_some(),
            };

            for h in join_handles {
                h.abort();
            }

            resumed
        })
    }
}

/// Production desktop notifications manager over `org.freedesktop.Notifications`.
///
/// Maintains a persistent session D-Bus connection so notification servers (such as KDE Plasma's
/// `libnotificationmanager`) keep interactive action buttons attached across the notification's
/// lifetime instead of stripping them when a short-lived connection disconnects.
#[derive(Debug, Clone)]
pub struct DesktopNotifier {
    bus_address: Option<String>,
    conn: Arc<Mutex<Option<zbus::Connection>>>,
    events_tx: Arc<broadcast::Sender<NotificationEvent>>,
}

impl Default for DesktopNotifier {
    fn default() -> Self {
        let (events_tx, _) = broadcast::channel(64);
        Self {
            bus_address: None,
            conn: Arc::new(Mutex::new(None)),
            events_tx: Arc::new(events_tx),
        }
    }
}

impl DesktopNotifier {
    /// Creates a notifier backed by the ambient session D-Bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a notifier targeting a specific D-Bus address (used in hermetic tests).
    pub fn with_bus_address(address: impl Into<String>) -> Self {
        let (events_tx, _) = broadcast::channel(64);
        Self {
            bus_address: Some(address.into()),
            conn: Arc::new(Mutex::new(None)),
            events_tx: Arc::new(events_tx),
        }
    }

    async fn session_conn(&self) -> Result<zbus::Connection, ActionError> {
        let mut guard = self.conn.lock().await;
        if let Some(ref existing) = *guard {
            return Ok(existing.clone());
        }
        let created = match &self.bus_address {
            Some(addr) => zbus::connection::Builder::address(addr.as_str())
                .map_err(|e| ActionError::Notification(format!("session bus error: {e}")))?
                .build()
                .await
                .map_err(|e| ActionError::Notification(format!("session bus error: {e}")))?,
            None => zbus::Connection::session()
                .await
                .map_err(|e| ActionError::Notification(format!("session bus error: {e}")))?,
        };
        Self::spawn_signal_listener(&created, Arc::downgrade(&self.events_tx)).await;
        *guard = Some(created.clone());
        Ok(created)
    }

    async fn spawn_signal_listener(
        conn: &zbus::Connection,
        events_tx: Weak<broadcast::Sender<NotificationEvent>>,
    ) {
        let Ok(proxy) = FreedesktopNotificationsProxy::new(conn).await else {
            return;
        };
        let Ok(mut action_stream) = proxy.receive_action_invoked().await else {
            return;
        };
        let Ok(mut close_stream) = proxy.receive_notification_closed().await else {
            return;
        };

        tokio::spawn(async move {
            use zbus::export::ordered_stream::OrderedStreamExt;
            loop {
                tokio::select! {
                    Some(sig) = action_stream.next() => {
                        let Some(tx) = events_tx.upgrade() else { break };
                        if let Ok(args) = sig.args() {
                            let _ = tx.send(NotificationEvent::ActionInvoked {
                                id: args.id,
                                action_key: args.action_key.to_string(),
                            });
                        }
                    }
                    Some(sig) = close_stream.next() => {
                        let Some(tx) = events_tx.upgrade() else { break };
                        if let Ok(args) = sig.args() {
                            let _ = tx.send(NotificationEvent::NotificationClosed {
                                id: args.id,
                                reason: args.reason,
                            });
                        }
                    }
                    else => break,
                }
            }
        });
    }

    async fn notifications_proxy(
        &self,
    ) -> Result<FreedesktopNotificationsProxy<'static>, ActionError> {
        let conn = self.session_conn().await?;
        FreedesktopNotificationsProxy::new(&conn)
            .await
            .map_err(|e| ActionError::Notification(e.to_string()))
    }

    async fn map_zbus_err(&self, err: zbus::Error) -> ActionError {
        if matches!(err, zbus::Error::InputOutput(_)) {
            *self.conn.lock().await = None;
        }
        ActionError::Notification(err.to_string())
    }
}

impl Notifier for DesktopNotifier {
    fn show_grace_notice<'a>(
        &'a self,
        replaces_id: u32,
        spec: GraceNoticeSpec,
    ) -> BoxFuture<'a, Result<u32, ActionError>> {
        Box::pin(async move {
            let proxy = self.notifications_proxy().await?;

            let summary = if spec.dry_run {
                "[Dry Run] Sleep action pending".to_string()
            } else {
                "Sleep action pending".to_string()
            };

            let body = match spec.phase {
                GracePhase::Countdown => {
                    format!(
                        "Sleep action in {}s... Click Cancel if you're awake.",
                        spec.remaining_sec
                    )
                }
                GracePhase::ActivityHold => format!(
                    "Activity detected — cancelling in {}s. Click Snooze to pause sleep actions.",
                    spec.remaining_sec
                ),
            };
            let actions = [
                "cancel",
                "Cancel (I'm Awake)",
                "snooze",
                "Snooze",
                "default",
                "Cancel",
            ];

            let mut hints = HashMap::new();
            hints.insert("urgency", Value::U8(spec.urgency.as_byte()));
            hints.insert("resident", Value::Bool(true));
            hints.insert("transient", Value::Bool(false));

            match proxy
                .notify(
                    "BPMute",
                    replaces_id,
                    "alarm-symbolic",
                    &summary,
                    &body,
                    &actions,
                    hints,
                    0,
                )
                .await
            {
                Ok(id) => Ok(id),
                Err(err) => Err(self.map_zbus_err(err).await),
            }
        })
    }

    fn close_notice<'a>(&'a self, id: u32) -> BoxFuture<'a, Result<(), ActionError>> {
        Box::pin(async move {
            let proxy = self.notifications_proxy().await?;
            match proxy.close_notification(id).await {
                Ok(()) => Ok(()),
                Err(err) => Err(self.map_zbus_err(err).await),
            }
        })
    }

    fn show_warning<'a>(
        &'a self,
        summary: &'a str,
        body: &'a str,
        transient: bool,
    ) -> BoxFuture<'a, Result<u32, ActionError>> {
        Box::pin(async move {
            let proxy = self.notifications_proxy().await?;

            let mut hints = HashMap::new();
            hints.insert("urgency", Value::U8(Urgency::Critical.as_byte()));
            hints.insert("transient", Value::Bool(transient));

            match proxy
                .notify("BPMute", 0, "dialog-warning", summary, body, &[], hints, 0)
                .await
            {
                Ok(id) => Ok(id),
                Err(err) => Err(self.map_zbus_err(err).await),
            }
        })
    }

    fn show_info<'a>(
        &'a self,
        summary: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<u32, ActionError>> {
        Box::pin(async move {
            let proxy = self.notifications_proxy().await?;

            let mut hints = HashMap::new();
            hints.insert("urgency", Value::U8(Urgency::Normal.as_byte()));
            hints.insert("transient", Value::Bool(true));

            match proxy
                .notify(
                    "BPMute",
                    0,
                    "dialog-information",
                    summary,
                    body,
                    &[],
                    hints,
                    5000,
                )
                .await
            {
                Ok(id) => Ok(id),
                Err(err) => Err(self.map_zbus_err(err).await),
            }
        })
    }

    fn subscribe_actions(&self) -> mpsc::Receiver<NotificationEvent> {
        let (tx, rx) = mpsc::channel(32);
        let mut b_rx = self.events_tx.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    res = b_rx.recv() => {
                        match res {
                            Ok(ev) => {
                                if tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
        rx
    }
}
