//! Zero-polling BlueZ background auto-reconnect service and WearableSource implementation (ADR-0001, ADR-0003).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use zbus::zvariant::ObjectPath;

use crate::daemon::DaemonClock;
use crate::daemon::wearable::{WearableDescriptor, WearableEvent, WearableSource};
use crate::sleep_engine::StreamState;
use crate::storage::LedgerStore;

use super::dbus::{GattCharacteristic1Proxy, GattManager1Proxy, HRM_CHAR_UUID, HRS_UUID};
use super::operations::{
    find_characteristic_path, read_battery_percentage, read_body_sensor_location,
};
use super::profile::{GATT_HRS_PROFILE_PATH, GattProfileHrs};
use super::stream::{LifecycleAction, StreamEvent, StreamTracker};

/// Pure-Rust BlueZ BLE heart-rate wearable stream source.
#[derive(Clone)]
pub struct BluezWearableSource {
    db_path: PathBuf,
    connection_lifecycle_notifications: bool,
    pinned_adapter: Option<String>,
    clock: Arc<dyn DaemonClock>,
}

impl BluezWearableSource {
    /// Creates a new `BluezWearableSource`.
    pub fn new(
        db_path: PathBuf,
        connection_lifecycle_notifications: bool,
        pinned_adapter: Option<String>,
        clock: Arc<dyn DaemonClock>,
    ) -> Self {
        Self {
            db_path,
            connection_lifecycle_notifications,
            pinned_adapter,
            clock,
        }
    }

    /// Loads enrolled wearables from local SQLite ledger store.
    fn load_enrolled_descriptors(&self) -> Vec<WearableDescriptor> {
        let Ok(Some(store)) = LedgerStore::open_read_only(&self.db_path) else {
            return Vec::new();
        };
        let Ok(enrolled) = store.list_enrolled_wearables() else {
            return Vec::new();
        };
        enrolled
            .into_iter()
            .map(|w| WearableDescriptor {
                mac: w.mac,
                address_type: w.address_type,
                alias: w.alias,
                advertised_name: w.advertised_name,
                supports_rr: w.supports_rr,
                supports_contact: w.supports_contact,
            })
            .collect()
    }
}

impl WearableSource for BluezWearableSource {
    fn subscribe(&self, shutdown_rx: watch::Receiver<bool>) -> mpsc::Receiver<WearableEvent> {
        let (event_tx, event_rx) = mpsc::channel(64);
        let this = self.clone();

        tokio::spawn(async move {
            run_bluez_source_task(this, event_tx, shutdown_rx).await;
        });

        event_rx
    }
}

async fn run_bluez_source_task(
    source: BluezWearableSource,
    event_tx: mpsc::Sender<WearableEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    // 1. Attempt connection to system D-Bus for BlueZ
    let system_conn = match zbus::connection::Builder::system() {
        Ok(builder) => match builder.build().await {
            Ok(c) => c,
            Err(err) => {
                tracing::info!(
                    error = %err,
                    "BlueZ system D-Bus bus unavailable; background BLE streaming inactive"
                );
                wait_for_shutdown(&mut shutdown_rx).await;
                return;
            }
        },
        Err(err) => {
            tracing::info!(
                error = %err,
                "BlueZ system D-Bus builder failed; background BLE streaming inactive"
            );
            wait_for_shutdown(&mut shutdown_rx).await;
            return;
        }
    };

    // 2. Optional session D-Bus for freedesktop notifications
    let session_conn = if source.connection_lifecycle_notifications {
        match zbus::connection::Builder::session() {
            Ok(builder) => builder.build().await.ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    // 3. Register GattProfile1 object on D-Bus connection
    let _ = system_conn
        .object_server()
        .at(GATT_HRS_PROFILE_PATH, GattProfileHrs)
        .await;

    // Register profile with Adapter's GattManager1
    let adapter_path = source
        .pinned_adapter
        .clone()
        .unwrap_or_else(|| "/org/bluez/hci0".to_string());

    register_hrs_profile(&system_conn, &adapter_path).await;

    // 4. Initialize StreamTracker with enrolled wearables
    let mut tracker = StreamTracker::new();
    let mut known_descriptors: HashMap<String, WearableDescriptor> = HashMap::new();
    for desc in source.load_enrolled_descriptors() {
        known_descriptors.insert(desc.mac.clone(), desc.clone());
        tracker.add_enrolled(desc);
    }

    let mut tick_interval = tokio::time::interval(Duration::from_millis(1000));
    let mut last_notification_id: u32 = 0;

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                let now_ms = source.clock.now_utc().timestamp_millis();

                let actions = tracker.handle_event(StreamEvent::Tick { ts_ms: now_ms });
                for action in actions {
                    handle_lifecycle_action(
                        &action,
                        &known_descriptors,
                        &event_tx,
                        &system_conn,
                        session_conn.as_ref(),
                        &mut last_notification_id,
                    ).await;
                }
            }
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    // Clean up profile on shutdown
    unregister_hrs_profile(&system_conn, &adapter_path).await;
}

async fn register_hrs_profile(conn: &zbus::Connection, adapter_path: &str) {
    let Ok(adapter_obj) = ObjectPath::try_from(adapter_path) else {
        return;
    };
    let Ok(builder) = GattManager1Proxy::builder(conn).path(adapter_obj) else {
        return;
    };
    let Ok(mgr) = builder.build().await else {
        return;
    };
    let Ok(profile_obj) = ObjectPath::try_from(GATT_HRS_PROFILE_PATH) else {
        return;
    };
    let _ = mgr
        .register_profile(profile_obj, &[HRS_UUID], HashMap::new())
        .await;
}

async fn unregister_hrs_profile(conn: &zbus::Connection, adapter_path: &str) {
    let Ok(adapter_obj) = ObjectPath::try_from(adapter_path) else {
        return;
    };
    let Ok(builder) = GattManager1Proxy::builder(conn).path(adapter_obj) else {
        return;
    };
    let Ok(mgr) = builder.build().await else {
        return;
    };
    let Ok(profile_obj) = ObjectPath::try_from(GATT_HRS_PROFILE_PATH) else {
        return;
    };
    let _ = mgr.unregister_profile(profile_obj).await;
}

async fn handle_lifecycle_action(
    action: &LifecycleAction,
    known: &HashMap<String, WearableDescriptor>,
    event_tx: &mpsc::Sender<WearableEvent>,
    system_conn: &zbus::Connection,
    session_conn: Option<&zbus::Connection>,
    last_notif_id: &mut u32,
) {
    match action {
        LifecycleAction::StatusTransition { mac, status, ts_ms } => {
            if let Some(desc) = known.get(mac) {
                let _ = event_tx
                    .send(WearableEvent::StreamState {
                        wearable: desc.clone(),
                        state: StreamState {
                            status: *status,
                            ts_ms: *ts_ms,
                        },
                    })
                    .await;
            }
        }
        LifecycleAction::StartNotify { mac } => {
            // BlueZ GATT notification handling
            if let Ok(Some(char_path)) =
                find_characteristic_path(system_conn, mac, HRM_CHAR_UUID).await
                && let Ok(builder) = GattCharacteristic1Proxy::builder(system_conn).path(char_path)
                && let Ok(proxy) = builder.build().await
            {
                let _ = proxy.start_notify().await;
            }
            // Read 0x2A38 body sensor location
            let _ = read_body_sensor_location(system_conn, mac).await;
            // Read Battery1 percentage
            let _ = read_battery_percentage(system_conn, mac).await;
        }
        LifecycleAction::CheckNotifying { mac } => {
            // Verify GATT notification status after silence and re-subscribe if needed
            if let Ok(Some(char_path)) =
                find_characteristic_path(system_conn, mac, HRM_CHAR_UUID).await
                && let Ok(builder) = GattCharacteristic1Proxy::builder(system_conn).path(char_path)
                && let Ok(proxy) = builder.build().await
                && let Ok(false) = proxy.notifying().await
            {
                let _ = proxy.start_notify().await;
            }
        }
        LifecycleAction::NotifyConnected { mac, name, battery } => {
            let body = match battery {
                Some(pct) => format!("Streaming started from {name} ({mac}). Battery: {pct}%"),
                None => format!("Streaming started from {name} ({mac})."),
            };
            send_desktop_notification(
                session_conn,
                last_notif_id,
                "BPMute Wearable Connected",
                &body,
                1,
            )
            .await;
        }
        LifecycleAction::NotifyDisconnected { mac, name } => {
            let body = format!("Wearable disconnected: {name} ({mac})");
            send_desktop_notification(
                session_conn,
                last_notif_id,
                "BPMute Wearable Disconnected",
                &body,
                1,
            )
            .await;
        }
        LifecycleAction::AlertActionRequired { mac, message } => {
            let body = format!("Action required for {mac}: {message}");
            send_desktop_notification(
                session_conn,
                last_notif_id,
                "BPMute Wearable Pairing Alert",
                &body,
                2,
            )
            .await;
        }
    }
}

async fn send_desktop_notification(
    session_conn: Option<&zbus::Connection>,
    replaces_id: &mut u32,
    summary: &str,
    body: &str,
    urgency: u8,
) {
    let Some(conn) = session_conn else { return };
    let mut hints = HashMap::new();
    hints.insert("urgency", zbus::zvariant::Value::U8(urgency));
    let Ok(reply) = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &(
                "bpmute",
                *replaces_id,
                "preferences-system-bluetooth",
                summary,
                body,
                Vec::<&str>::new(),
                hints,
                5000i32,
            ),
        )
        .await
    else {
        return;
    };

    if let Ok(id) = reply.body().deserialize::<u32>() {
        *replaces_id = id;
    }
}

async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<bool>) {
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            break;
        }
    }
}
