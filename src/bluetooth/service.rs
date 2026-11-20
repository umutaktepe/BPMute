//! Zero-polling BlueZ background auto-reconnect service and WearableSource implementation (ADR-0001, ADR-0003).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};
use zbus::{MatchRule, MessageStream};

use crate::daemon::DaemonClock;
use crate::daemon::wearable::{WearableDescriptor, WearableEvent, WearableSource};
use crate::sleep_engine::{StreamState, StreamStatus};
use crate::storage::LedgerStore;

use super::dbus::{GattCharacteristic1Proxy, GattManager1Proxy, HRM_CHAR_UUID};
use super::operations::{
    extract_bool, extract_byte_array, extract_mac_from_path, extract_string, extract_u8,
    find_characteristic_path, inspect_bluez_state, read_battery_percentage,
    read_body_sensor_location,
};
use super::packet::parse_hr_measurement;
use super::profile::{GATT_HRS_PROFILE_PATH, GATT_PROFILE_APP_PATH, GattProfileHrs};
use super::stream::{DisconnectReason, LifecycleAction, StreamEvent, StreamTracker};

const DEFAULT_ADAPTER_PATH: &str = "/org/bluez/hci0";

/// Pure-Rust BlueZ BLE heart-rate wearable stream source.
#[derive(Clone)]
pub struct BluezWearableSource {
    db_path: PathBuf,
    connection_lifecycle_notifications: bool,
    pinned_adapter: Option<String>,
    clock: Arc<dyn DaemonClock>,
    allowlist_tx: watch::Sender<u64>,
}

impl BluezWearableSource {
    /// Creates a new `BluezWearableSource`.
    pub fn new(
        db_path: PathBuf,
        connection_lifecycle_notifications: bool,
        pinned_adapter: Option<String>,
        clock: Arc<dyn DaemonClock>,
    ) -> Self {
        let (allowlist_tx, _) = watch::channel(0u64);
        Self {
            db_path,
            connection_lifecycle_notifications,
            pinned_adapter,
            clock,
            allowlist_tx,
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

    fn notify_allowlist_changed(&self) {
        self.allowlist_tx.send_modify(|seq| {
            *seq = seq.wrapping_add(1);
        });
    }
}

struct BluezRuntimeCtx<'a> {
    source: &'a BluezWearableSource,
    tracker: &'a mut StreamTracker,
    known_descriptors: &'a mut HashMap<String, WearableDescriptor>,
    hrm_char_paths: &'a mut HashMap<String, String>,
    event_tx: &'a mpsc::Sender<WearableEvent>,
    system_conn: &'a zbus::Connection,
    session_conn: Option<&'a zbus::Connection>,
    last_notification_id: &'a mut u32,
}

async fn run_bluez_source_task(
    source: BluezWearableSource,
    event_tx: mpsc::Sender<WearableEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
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

    let session_conn = if source.connection_lifecycle_notifications {
        match zbus::connection::Builder::session() {
            Ok(builder) => builder.build().await.ok(),
            Err(_) => None,
        }
    } else {
        None
    };

    if let Err(err) = system_conn
        .object_server()
        .at(GATT_PROFILE_APP_PATH, zbus::fdo::ObjectManager)
        .await
    {
        tracing::warn!(error = %err, "failed to export ObjectManager for BlueZ GattProfile1");
    }
    if let Err(err) = system_conn
        .object_server()
        .at(GATT_HRS_PROFILE_PATH, GattProfileHrs)
        .await
    {
        tracing::warn!(error = %err, "failed to export GattProfile1 HRS object");
    }

    let mut props_stream = build_bluez_signal_stream(
        &system_conn,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
    )
    .await;
    let mut iface_added_stream = build_bluez_signal_stream(
        &system_conn,
        "org.freedesktop.DBus.ObjectManager",
        "InterfacesAdded",
    )
    .await;

    let mut tracker = StreamTracker::new();
    let mut known_descriptors: HashMap<String, WearableDescriptor> = HashMap::new();
    let mut hrm_char_paths: HashMap<String, String> = HashMap::new();
    let mut last_notification_id: u32 = 0;

    let registered_adapters =
        register_profiles_from_snapshot(&system_conn, source.pinned_adapter.as_deref()).await;

    {
        let mut ctx = BluezRuntimeCtx {
            source: &source,
            tracker: &mut tracker,
            known_descriptors: &mut known_descriptors,
            hrm_char_paths: &mut hrm_char_paths,
            event_tx: &event_tx,
            system_conn: &system_conn,
            session_conn: session_conn.as_ref(),
            last_notification_id: &mut last_notification_id,
        };
        sync_allowlist_and_snapshot(&mut ctx).await;
    }

    let mut allowlist_rx = source.allowlist_tx.subscribe();
    let mut tick_interval = tokio::time::interval(Duration::from_millis(1000));

    loop {
        tokio::select! {
            _ = tick_interval.tick() => {
                let now_ms = source.clock.now_utc().timestamp_millis();
                let actions = tracker.handle_event(StreamEvent::Tick { ts_ms: now_ms });
                let mut ctx = BluezRuntimeCtx {
                    source: &source,
                    tracker: &mut tracker,
                    known_descriptors: &mut known_descriptors,
                    hrm_char_paths: &mut hrm_char_paths,
                    event_tx: &event_tx,
                    system_conn: &system_conn,
                    session_conn: session_conn.as_ref(),
                    last_notification_id: &mut last_notification_id,
                };
                process_lifecycle_actions(&actions, &mut ctx).await;
            }
            changed = allowlist_rx.changed() => {
                if changed.is_ok() {
                    let mut ctx = BluezRuntimeCtx {
                        source: &source,
                        tracker: &mut tracker,
                        known_descriptors: &mut known_descriptors,
                        hrm_char_paths: &mut hrm_char_paths,
                        event_tx: &event_tx,
                        system_conn: &system_conn,
                        session_conn: session_conn.as_ref(),
                        last_notification_id: &mut last_notification_id,
                    };
                    sync_allowlist_and_snapshot(&mut ctx).await;
                }
            }
            Some(msg_res) = next_stream_msg(&mut props_stream) => {
                if let Ok(msg) = msg_res {
                    let mut ctx = BluezRuntimeCtx {
                        source: &source,
                        tracker: &mut tracker,
                        known_descriptors: &mut known_descriptors,
                        hrm_char_paths: &mut hrm_char_paths,
                        event_tx: &event_tx,
                        system_conn: &system_conn,
                        session_conn: session_conn.as_ref(),
                        last_notification_id: &mut last_notification_id,
                    };
                    handle_properties_changed_msg(&msg, &mut ctx).await;
                }
            }
            Some(msg_res) = next_stream_msg(&mut iface_added_stream) => {
                if let Ok(msg) = msg_res {
                    let mut ctx = BluezRuntimeCtx {
                        source: &source,
                        tracker: &mut tracker,
                        known_descriptors: &mut known_descriptors,
                        hrm_char_paths: &mut hrm_char_paths,
                        event_tx: &event_tx,
                        system_conn: &system_conn,
                        session_conn: session_conn.as_ref(),
                        last_notification_id: &mut last_notification_id,
                    };
                    handle_interfaces_added_msg(&msg, &mut ctx).await;
                }
            }
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    for adapter_path in &registered_adapters {
        unregister_hrs_profile(&system_conn, adapter_path).await;
    }
}

async fn next_stream_msg(
    stream: &mut Option<MessageStream>,
) -> Option<zbus::Result<zbus::Message>> {
    match stream.as_mut() {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

async fn build_bluez_signal_stream(
    conn: &zbus::Connection,
    interface: &'static str,
    member: &'static str,
) -> Option<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")
        .ok()?
        .interface(interface)
        .ok()?
        .member(member)
        .ok()?
        .build();
    MessageStream::for_match_rule(rule, conn, None).await.ok()
}

fn normalize_adapter_path(adapter: &str) -> String {
    if adapter.starts_with('/') {
        adapter.to_string()
    } else {
        format!("/org/bluez/{adapter}")
    }
}

async fn register_profiles_from_snapshot(
    conn: &zbus::Connection,
    pinned_adapter: Option<&str>,
) -> Vec<String> {
    let targets = if let Some(pinned) = pinned_adapter {
        vec![normalize_adapter_path(pinned)]
    } else if let Ok(snap) = inspect_bluez_state(conn).await
        && !snap.adapters.is_empty()
    {
        snap.adapters
    } else {
        vec![DEFAULT_ADAPTER_PATH.to_string()]
    };

    for adapter_path in &targets {
        register_hrs_profile(conn, adapter_path).await;
    }
    targets
}

async fn sync_allowlist_and_snapshot(ctx: &mut BluezRuntimeCtx<'_>) {
    let loaded = ctx.source.load_enrolled_descriptors();
    let next_macs: HashSet<String> = loaded.iter().map(|d| d.mac.to_ascii_uppercase()).collect();

    let removed_macs: Vec<String> = ctx
        .known_descriptors
        .keys()
        .filter(|mac| !next_macs.contains(*mac))
        .cloned()
        .collect();
    for mac in removed_macs {
        ctx.tracker.remove_enrolled(&mac);
        ctx.known_descriptors.remove(&mac);
        ctx.hrm_char_paths.remove(&mac);
    }

    for desc in loaded {
        let norm_mac = desc.mac.to_ascii_uppercase();
        ctx.known_descriptors.insert(norm_mac, desc.clone());
        ctx.tracker.add_enrolled(desc);
    }

    let Ok(snap) = inspect_bluez_state(ctx.system_conn).await else {
        return;
    };
    let now_ms = ctx.source.clock.now_utc().timestamp_millis();

    for (mac, dev_snap) in snap.devices {
        if !ctx.known_descriptors.contains_key(&mac) {
            continue;
        }
        if let Some(char_path) = dev_snap.hrm_char_path {
            ctx.hrm_char_paths.insert(mac.clone(), char_path);
        }
        if let Some(pct) = dev_snap.battery_pct {
            let _ = ctx.tracker.handle_event(StreamEvent::BatteryUpdated {
                mac: mac.clone(),
                percentage: pct,
            });
        }
        if dev_snap.connected && !ctx.tracker.is_connected(&mac) {
            let actions = ctx
                .tracker
                .handle_event(StreamEvent::Connected { mac, ts_ms: now_ms });
            process_lifecycle_actions(&actions, ctx).await;
        }
    }
}

async fn handle_properties_changed_msg(msg: &zbus::Message, ctx: &mut BluezRuntimeCtx<'_>) {
    let header = msg.header();
    let Some(path) = header.path() else {
        return;
    };
    let path_str = path.as_str();
    let Some(mac) = extract_mac_from_path(path_str) else {
        return;
    };
    if !ctx.known_descriptors.contains_key(&mac) {
        return;
    }

    let Ok((iface, changed, _invalidated)) =
        msg.body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        return;
    };

    let now_ms = ctx.source.clock.now_utc().timestamp_millis();
    match iface.as_str() {
        "org.bluez.Device1" => handle_device_props_changed(&mac, &changed, now_ms, ctx).await,
        "org.bluez.GattCharacteristic1" => {
            handle_characteristic_props_changed(&mac, path_str, &changed, now_ms, ctx).await;
        }
        "org.bluez.Battery1" => {
            if let Some(pct) = extract_u8(changed.get("Percentage")) {
                let _ = ctx.tracker.handle_event(StreamEvent::BatteryUpdated {
                    mac: mac.clone(),
                    percentage: pct,
                });
                if let Some(desc) = ctx.known_descriptors.get(&mac) {
                    let _ = ctx
                        .event_tx
                        .send(WearableEvent::Metadata {
                            wearable: desc.clone(),
                            battery: Some(pct),
                            body_location: None,
                        })
                        .await;
                }
            }
        }
        _ => {}
    }
}

async fn handle_device_props_changed(
    mac: &str,
    changed: &HashMap<String, OwnedValue>,
    now_ms: i64,
    ctx: &mut BluezRuntimeCtx<'_>,
) {
    if let Some(connected) = extract_bool(changed.get("Connected")) {
        if connected {
            let actions = ctx.tracker.handle_event(StreamEvent::Connected {
                mac: mac.to_string(),
                ts_ms: now_ms,
            });
            process_lifecycle_actions(&actions, ctx).await;
        } else {
            ctx.hrm_char_paths.remove(mac);
            let actions = ctx.tracker.handle_event(StreamEvent::Disconnected {
                mac: mac.to_string(),
                reason: DisconnectReason::Timeout,
                ts_ms: now_ms,
            });
            process_lifecycle_actions(&actions, ctx).await;
        }
    }

    if extract_bool(changed.get("ServicesResolved")) == Some(true)
        && ctx.tracker.active_mac() == Some(mac)
        && ctx.tracker.active_stream_status() == Some(StreamStatus::Connecting)
    {
        execute_start_notify(mac, ctx).await;
    }
}

async fn handle_characteristic_props_changed(
    mac: &str,
    path_str: &str,
    changed: &HashMap<String, OwnedValue>,
    now_ms: i64,
    ctx: &mut BluezRuntimeCtx<'_>,
) {
    let Some(bytes) = extract_byte_array(changed.get("Value")) else {
        return;
    };

    let known_hrm = match ctx.hrm_char_paths.get(mac) {
        Some(p) => p == path_str,
        None => {
            if let Ok(Some(found)) =
                find_characteristic_path(ctx.system_conn, mac, HRM_CHAR_UUID).await
            {
                let found_str = found.as_str().to_string();
                let matches = found_str == path_str;
                ctx.hrm_char_paths.insert(mac.to_string(), found_str);
                matches
            } else {
                false
            }
        }
    };
    if !known_hrm {
        return;
    }

    let Ok(packet) = parse_hr_measurement(&bytes, now_ms) else {
        return;
    };

    let actions = ctx.tracker.handle_event(StreamEvent::Packet {
        mac: mac.to_string(),
        packet: packet.clone(),
    });
    process_lifecycle_actions(&actions, ctx).await;

    if ctx.tracker.active_mac() == Some(mac)
        && let Some(desc) = ctx.known_descriptors.get(mac)
    {
        let _ = ctx
            .event_tx
            .send(WearableEvent::HrPacket {
                wearable: desc.clone(),
                packet,
            })
            .await;
    }
}

async fn handle_interfaces_added_msg(msg: &zbus::Message, ctx: &mut BluezRuntimeCtx<'_>) {
    let Ok((obj_path, ifaces)) = msg.body().deserialize::<(
        OwnedObjectPath,
        HashMap<String, HashMap<String, OwnedValue>>,
    )>() else {
        return;
    };
    let path_str = obj_path.as_str();
    let Some(mac) = extract_mac_from_path(path_str) else {
        return;
    };
    if !ctx.known_descriptors.contains_key(&mac) {
        return;
    }

    if let Some(char_props) = ifaces.get("org.bluez.GattCharacteristic1")
        && let Some(uuid) = extract_string(char_props.get("UUID"))
        && uuid.eq_ignore_ascii_case(HRM_CHAR_UUID)
    {
        ctx.hrm_char_paths.insert(mac.clone(), path_str.to_string());
        if ctx.tracker.active_mac() == Some(mac.as_str())
            && ctx.tracker.active_stream_status() == Some(StreamStatus::Connecting)
        {
            execute_start_notify(&mac, ctx).await;
        }
    }
}

async fn register_hrs_profile(conn: &zbus::Connection, adapter_path: &str) {
    let Ok(adapter_obj) = ObjectPath::try_from(adapter_path) else {
        return;
    };
    let Ok(builder) = GattManager1Proxy::builder(conn).path(adapter_obj) else {
        return;
    };
    let mgr = match builder.build().await {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(adapter = %adapter_path, error = %err, "failed to build GattManager1Proxy");
            return;
        }
    };
    let Ok(app_obj) = ObjectPath::try_from(GATT_PROFILE_APP_PATH) else {
        return;
    };
    if let Err(err) = mgr.register_application(app_obj, HashMap::new()).await {
        tracing::warn!(
            adapter = %adapter_path,
            error = %err,
            "BlueZ GattManager1.RegisterApplication failed; passive auto-reconnect may be inactive"
        );
    } else {
        tracing::info!(
            adapter = %adapter_path,
            "registered BlueZ GattProfile1 (0x180D) via GattManager1.RegisterApplication"
        );
    }
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
    let Ok(app_obj) = ObjectPath::try_from(GATT_PROFILE_APP_PATH) else {
        return;
    };
    if let Err(err) = mgr.unregister_application(app_obj).await {
        tracing::debug!(adapter = %adapter_path, error = %err, "BlueZ GattManager1.UnregisterApplication failed during shutdown");
    }
}

async fn process_lifecycle_actions(actions: &[LifecycleAction], ctx: &mut BluezRuntimeCtx<'_>) {
    for action in actions {
        handle_lifecycle_action(action, ctx).await;
    }
}

async fn execute_start_notify(mac: &str, ctx: &mut BluezRuntimeCtx<'_>) {
    let char_path = if let Some(cached) = ctx.hrm_char_paths.get(mac) {
        ObjectPath::try_from(cached.as_str())
            .ok()
            .map(OwnedObjectPath::from)
    } else {
        let found = find_characteristic_path(ctx.system_conn, mac, HRM_CHAR_UUID)
            .await
            .ok()
            .flatten();
        if let Some(ref p) = found {
            ctx.hrm_char_paths
                .insert(mac.to_string(), p.as_str().to_string());
        }
        found
    };

    let Some(path) = char_path else {
        return;
    };
    let Ok(builder) = GattCharacteristic1Proxy::builder(ctx.system_conn).path(path) else {
        return;
    };
    let Ok(proxy) = builder.build().await else {
        return;
    };

    let notify_ok = match proxy.start_notify().await {
        Ok(()) => true,
        Err(err) => {
            if matches!(proxy.notifying().await, Ok(true)) {
                true
            } else {
                let err_str = err.to_string();
                let reason = if err_str.contains("Authentication") {
                    DisconnectReason::Authentication
                } else {
                    DisconnectReason::Other(err_str)
                };
                let now_ms = ctx.source.clock.now_utc().timestamp_millis();
                let actions = ctx.tracker.handle_event(StreamEvent::Disconnected {
                    mac: mac.to_string(),
                    reason,
                    ts_ms: now_ms,
                });
                Box::pin(process_lifecycle_actions(&actions, ctx)).await;
                false
            }
        }
    };

    if notify_ok {
        let now_ms = ctx.source.clock.now_utc().timestamp_millis();
        let actions = ctx.tracker.handle_event(StreamEvent::NotifyStarted {
            mac: mac.to_string(),
            ts_ms: now_ms,
        });
        Box::pin(process_lifecycle_actions(&actions, ctx)).await;
    }

    let body_location = read_body_sensor_location(ctx.system_conn, mac)
        .await
        .ok()
        .flatten()
        .map(|loc| loc.display_label().to_string());
    let battery = read_battery_percentage(ctx.system_conn, mac)
        .await
        .ok()
        .flatten();
    if (battery.is_some() || body_location.is_some())
        && let Some(desc) = ctx.known_descriptors.get(mac)
    {
        let _ = ctx
            .event_tx
            .send(WearableEvent::Metadata {
                wearable: desc.clone(),
                battery,
                body_location,
            })
            .await;
    }
}

async fn handle_lifecycle_action(action: &LifecycleAction, ctx: &mut BluezRuntimeCtx<'_>) {
    match action {
        LifecycleAction::StatusTransition { mac, status, ts_ms } => {
            if let Some(desc) = ctx.known_descriptors.get(mac) {
                let _ = ctx
                    .event_tx
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
            execute_start_notify(mac, ctx).await;
        }
        LifecycleAction::CheckNotifying { mac } => {
            if let Ok(Some(char_path)) =
                find_characteristic_path(ctx.system_conn, mac, HRM_CHAR_UUID).await
                && let Ok(builder) =
                    GattCharacteristic1Proxy::builder(ctx.system_conn).path(char_path)
                && let Ok(proxy) = builder.build().await
                && let Ok(false) = proxy.notifying().await
                && let Err(err) = proxy.start_notify().await
            {
                tracing::warn!(error = %err, "BlueZ GattCharacteristic1.StartNotify failed during CheckNotifying");
            }
        }
        LifecycleAction::NotifyConnected { mac, name, battery } => {
            let body = match battery {
                Some(pct) => format!("Streaming started from {name} ({mac}). Battery: {pct}%"),
                None => format!("Streaming started from {name} ({mac})."),
            };
            send_desktop_notification(
                ctx.session_conn,
                ctx.last_notification_id,
                "BPMute Wearable Connected",
                &body,
                1,
            )
            .await;
        }
        LifecycleAction::NotifyDisconnected { mac, name } => {
            let body = format!("Wearable disconnected: {name} ({mac})");
            send_desktop_notification(
                ctx.session_conn,
                ctx.last_notification_id,
                "BPMute Wearable Disconnected",
                &body,
                1,
            )
            .await;
        }
        LifecycleAction::AlertActionRequired { mac, message } => {
            let body = format!("Action required for {mac}: {message}");
            send_desktop_notification(
                ctx.session_conn,
                ctx.last_notification_id,
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
