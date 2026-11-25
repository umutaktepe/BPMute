use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{Mutex, broadcast, mpsc};
use zbus::proxy;
use zbus::zvariant::Value;

use super::traits::{
    ActionError, BoxFuture, GraceNoticeSpec, GracePhase, NotificationEvent, Notifier, Urgency,
};

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
        let proxy = match FreedesktopNotificationsProxy::new(conn).await {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to build FreedesktopNotificationsProxy for notification signals"
                );
                return;
            }
        };
        let mut action_stream = match proxy.receive_action_invoked().await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to subscribe to Notifications.ActionInvoked"
                );
                return;
            }
        };
        let mut close_stream = match proxy.receive_notification_closed().await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "failed to subscribe to Notifications.NotificationClosed"
                );
                return;
            }
        };

        tokio::spawn(async move {
            use zbus::export::ordered_stream::OrderedStreamExt;
            loop {
                tokio::select! {
                    Some(sig) = action_stream.next() => {
                        let Some(tx) = events_tx.upgrade() else { break };
                        match sig.args() {
                            Ok(args) => {
                                let _ = tx.send(NotificationEvent::ActionInvoked {
                                    id: args.id,
                                    action_key: args.action_key.to_string(),
                                });
                            }
                            Err(err) => {
                                tracing::debug!(
                                    error = %err,
                                    "failed to decode Notifications.ActionInvoked signal"
                                );
                            }
                        }
                    }
                    Some(sig) = close_stream.next() => {
                        let Some(tx) = events_tx.upgrade() else { break };
                        match sig.args() {
                            Ok(args) => {
                                let _ = tx.send(NotificationEvent::NotificationClosed {
                                    id: args.id,
                                    reason: args.reason,
                                });
                            }
                            Err(err) => {
                                tracing::debug!(
                                    error = %err,
                                    "failed to decode Notifications.NotificationClosed signal"
                                );
                            }
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
