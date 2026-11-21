//! High-level D-Bus operations for BlueZ LE discovery, bonding, device removal, and GATT inspection.

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{MatchRule, MessageStream};

use crate::ipc::protocol::DiscoveredWearable;

use super::dbus::{
    Adapter1Proxy, BATTERY_LEVEL_CHAR_UUID, Battery1Proxy, Device1Proxy, GattCharacteristic1Proxy,
    HRM_CHAR_UUID, HRS_UUID,
};
use super::packet::{BodySensorLocation, parse_body_sensor_location};

/// Live BlueZ adapter and device state snapshot queried from `ObjectManager`.
#[derive(Debug, Default)]
pub(crate) struct BluezSnapshot {
    pub adapters: Vec<String>,
    pub powered_adapters: Vec<String>,
    pub devices: HashMap<String, BluezDeviceSnapshot>,
}

/// Snapshot of a single BlueZ `Device1` and its optional `0x2A37` characteristic path.
#[derive(Debug, Default)]
pub(crate) struct BluezDeviceSnapshot {
    pub connected: bool,
    pub hrm_char_path: Option<String>,
    pub battery_char_path: Option<String>,
    pub battery_pct: Option<u8>,
}

/// Inspects current BlueZ adapters, connected devices, `0x2A37` characteristics, and battery levels.
pub(crate) async fn inspect_bluez_state(conn: &zbus::Connection) -> zbus::Result<BluezSnapshot> {
    let obj_mgr = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let managed = obj_mgr.get_managed_objects().await?;

    let mut snap = BluezSnapshot::default();
    for (path, ifaces) in &managed {
        if let Some(adapter_props) = ifaces.get("org.bluez.Adapter1") {
            let adapter_path = path.as_str().to_string();
            snap.adapters.push(adapter_path.clone());
            if extract_bool(adapter_props.get("Powered")).unwrap_or(true) {
                snap.powered_adapters.push(adapter_path);
            }
        }
        if let Some(dev_props) = ifaces.get("org.bluez.Device1")
            && let Some(addr) = extract_string(dev_props.get("Address"))
        {
            let mac = addr.to_ascii_uppercase();
            let entry = snap.devices.entry(mac).or_default();
            entry.connected = extract_bool(dev_props.get("Connected")).unwrap_or(false);
        }
        if let Some(bat_props) = ifaces.get("org.bluez.Battery1")
            && let Some(mac) = extract_mac_from_path(path.as_str())
        {
            let entry = snap.devices.entry(mac).or_default();
            if let Some(pct) = extract_u8(bat_props.get("Percentage")) {
                entry.battery_pct = Some(pct);
            }
        }
        if let Some(char_props) = ifaces.get("org.bluez.GattCharacteristic1")
            && let Some(uuid) = extract_string(char_props.get("UUID"))
            && let Some(mac) = extract_mac_from_path(path.as_str())
        {
            let entry = snap.devices.entry(mac).or_default();
            if uuid.eq_ignore_ascii_case(HRM_CHAR_UUID) {
                entry.hrm_char_path = Some(path.as_str().to_string());
            } else if uuid.eq_ignore_ascii_case(BATTERY_LEVEL_CHAR_UUID) {
                entry.battery_char_path = Some(path.as_str().to_string());
                if entry.battery_pct.is_none()
                    && let Some(bytes) = extract_byte_array(char_props.get("Value"))
                    && !bytes.is_empty()
                {
                    entry.battery_pct = Some(bytes[0]);
                }
            }
        }
    }

    Ok(snap)
}

/// Extracts a normalized uppercase `AA:BB:CC:DD:EE:FF` MAC address from a BlueZ object path.
pub(crate) fn extract_mac_from_path(path: &str) -> Option<String> {
    let pos = path.find("/dev_")?;
    let rest = &path[pos + 5..];
    let dev_seg = rest.split('/').next().unwrap_or(rest);
    let mac = dev_seg.replace('_', ":").to_ascii_uppercase();
    if mac.len() == 17 && mac.bytes().filter(|&b| b == b':').count() == 5 {
        Some(mac)
    } else {
        None
    }
}

/// Finds the BlueZ D-Bus object path corresponding to a normalized MAC address.
pub async fn find_device_object_path(
    conn: &zbus::Connection,
    mac: &str,
) -> zbus::Result<Option<OwnedObjectPath>> {
    let obj_mgr = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let managed = obj_mgr.get_managed_objects().await?;
    let dev_pattern = format!("dev_{}", mac.replace(':', "_").to_ascii_uppercase());

    for (path, ifaces) in managed {
        if ifaces.contains_key("org.bluez.Device1") && path.as_str().contains(&dev_pattern) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Normalizes a BlueZ adapter identifier (e.g., `"hci0"` or `"/org/bluez/hci0"`) into an object path.
pub(crate) fn normalize_adapter_path(adapter: &str) -> String {
    if adapter.starts_with('/') {
        adapter.to_string()
    } else {
        format!("/org/bluez/{adapter}")
    }
}

/// Target BlueZ adapter paths resolved from config or live `ObjectManager` powered adapters.
pub(crate) struct ResolvedAdapters {
    pub targets: Vec<String>,
    pub any_present: bool,
}

/// Resolves target BlueZ adapter object paths from an optional pinned adapter or powered `ObjectManager` adapters.
pub(crate) async fn resolve_target_adapters(
    conn: &zbus::Connection,
    pinned_adapter: Option<&str>,
) -> zbus::Result<ResolvedAdapters> {
    if let Some(pinned) = pinned_adapter {
        return Ok(ResolvedAdapters {
            targets: vec![normalize_adapter_path(pinned)],
            any_present: true,
        });
    }
    let mut snap = inspect_bluez_state(conn).await?;
    snap.powered_adapters.sort();
    Ok(ResolvedAdapters {
        any_present: !snap.adapters.is_empty(),
        targets: snap.powered_adapters,
    })
}

/// Builds a `zbus::MessageStream` matching signals emitted by `org.bluez`.
pub(crate) async fn build_bluez_signal_stream(
    conn: &zbus::Connection,
    interface: &'static str,
    member: &'static str,
) -> Option<MessageStream> {
    let rule = match MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.bluez")
        .and_then(|b| b.interface(interface))
        .and_then(|b| b.member(member))
        .map(|b| b.build())
    {
        Ok(rule) => rule,
        Err(err) => {
            tracing::warn!(interface, member, error = %err, "failed to build BlueZ D-Bus MatchRule");
            return None;
        }
    };
    match MessageStream::for_match_rule(rule, conn, None).await {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::warn!(interface, member, error = %err, "failed to subscribe to BlueZ D-Bus signal stream");
            None
        }
    }
}

/// Awaits the next message from an optional BlueZ D-Bus `MessageStream`, pending forever if `None`.
pub(crate) async fn next_stream_msg(
    stream: &mut Option<MessageStream>,
) -> Option<zbus::Result<zbus::Message>> {
    match stream.as_mut() {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// Returns whether a BlueZ `ObjectManager.InterfacesAdded` message announces a `Device1` object.
pub(crate) fn is_wearable_interfaces_added(msg: &zbus::Message) -> bool {
    msg.body()
        .deserialize::<(
            OwnedObjectPath,
            HashMap<String, HashMap<String, OwnedValue>>,
        )>()
        .is_ok_and(|(_path, ifaces)| ifaces.contains_key("org.bluez.Device1"))
}

/// Returns whether a BlueZ `PropertiesChanged` message updates discovery-relevant `Device1` fields.
pub(crate) fn is_relevant_wearable_props_changed(msg: &zbus::Message) -> bool {
    msg.body()
        .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
        .is_ok_and(|(iface, changed, _invalidated)| {
            iface == "org.bluez.Device1"
                && changed.keys().any(|k| {
                    matches!(
                        k.as_str(),
                        "UUIDs" | "Name" | "Alias" | "Address" | "AddressType" | "Paired"
                    )
                })
        })
}

/// Discovers all known or currently discovered BlueZ devices exposing Heart Rate Service (0x180D).
pub async fn list_heart_rate_devices(
    conn: &zbus::Connection,
) -> zbus::Result<Vec<DiscoveredWearable>> {
    list_heart_rate_devices_for_adapters(conn, &[]).await
}

/// Discovers BlueZ devices exposing Heart Rate Service (0x180D), optionally scoped to `adapter_paths`.
pub(crate) async fn list_heart_rate_devices_for_adapters(
    conn: &zbus::Connection,
    adapter_paths: &[String],
) -> zbus::Result<Vec<DiscoveredWearable>> {
    let obj_mgr = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let managed = obj_mgr.get_managed_objects().await?;
    let mut devices = Vec::new();

    for (path, ifaces) in managed {
        if !adapter_paths.is_empty()
            && !adapter_paths
                .iter()
                .any(|adapter| path.as_str().starts_with(&format!("{adapter}/")))
        {
            continue;
        }
        let Some(dev_props) = ifaces.get("org.bluez.Device1") else {
            continue;
        };

        let uuids = extract_string_list(dev_props.get("UUIDs"));
        let is_hrs = uuids.iter().any(|u| u.eq_ignore_ascii_case(HRS_UUID));

        if !is_hrs {
            continue;
        }

        let address = extract_string(dev_props.get("Address")).unwrap_or_default();
        if address.is_empty() {
            continue;
        }

        let address_type =
            extract_string(dev_props.get("AddressType")).unwrap_or_else(|| "random".to_string());
        let name = extract_string(dev_props.get("Name"))
            .or_else(|| extract_string(dev_props.get("Alias")));
        let rssi = extract_i16(dev_props.get("RSSI"));
        let paired = extract_bool(dev_props.get("Paired")).unwrap_or(false);

        devices.push(DiscoveredWearable {
            address,
            address_type,
            name,
            rssi,
            paired,
            already_enrolled: false,
            uuids,
        });
    }

    devices.sort_by(|a, b| {
        a.address
            .to_ascii_uppercase()
            .cmp(&b.address.to_ascii_uppercase())
    });
    Ok(devices)
}

/// Starts bounded Bluetooth LE discovery scan for Heart Rate Service devices.
pub async fn start_le_discovery(conn: &zbus::Connection, adapter_path: &str) -> zbus::Result<()> {
    let adapter_obj = ObjectPath::try_from(adapter_path)?;
    let adapter = Adapter1Proxy::builder(conn)
        .path(adapter_obj)?
        .build()
        .await?;

    let mut filter = HashMap::new();
    filter.insert("Transport", Value::from("le"));
    filter.insert("DuplicateData", Value::from(false));
    filter.insert("UUIDs", Value::from(vec![HRS_UUID]));

    if let Err(err) = adapter.set_discovery_filter(filter).await {
        tracing::warn!(adapter = %adapter_path, error = %err, "BlueZ Adapter1.SetDiscoveryFilter failed");
    }
    match adapter.start_discovery().await {
        Ok(()) => Ok(()),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.InProgress" =>
        {
            tracing::debug!(adapter = %adapter_path, "BlueZ Adapter1 discovery already in progress");
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Stops Bluetooth discovery scan on the specified adapter.
pub async fn stop_le_discovery(conn: &zbus::Connection, adapter_path: &str) -> zbus::Result<()> {
    let adapter_obj = ObjectPath::try_from(adapter_path)?;
    let adapter = Adapter1Proxy::builder(conn)
        .path(adapter_obj)?
        .build()
        .await?;
    adapter.stop_discovery().await
}

/// Initiates connection to a BlueZ device by MAC address unless already connected.
pub async fn connect_device(conn: &zbus::Connection, mac: &str) -> zbus::Result<()> {
    let Some(path) = find_device_object_path(conn, mac).await? else {
        return Err(zbus::Error::Failure(format!(
            "device {mac} not found in BlueZ"
        )));
    };
    let device = Device1Proxy::builder(conn).path(path)?.build().await?;
    if device.connected().await.unwrap_or(false) {
        return Ok(());
    }
    match device.connect().await {
        Ok(()) => Ok(()),
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.AlreadyConnected" =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// Sets a device as trusted, optionally initiates pairing, and returns an updated
/// `(mac, address_type)` if BlueZ resolved an RPA to a permanent Identity Address.
pub async fn bond_and_trust_device(
    conn: &zbus::Connection,
    mac: &str,
    pair: bool,
) -> zbus::Result<Option<(String, String)>> {
    let Some(path) = find_device_object_path(conn, mac).await? else {
        return Err(zbus::Error::Failure(format!(
            "device {mac} not found in BlueZ"
        )));
    };
    let device = Device1Proxy::builder(conn).path(path)?.build().await?;
    if let Err(err) = device.set_trusted(true).await {
        tracing::warn!(error = %err, "BlueZ Device1.Trusted=true failed");
    }
    if pair {
        let is_paired = device.paired().await.unwrap_or(false);
        if !is_paired {
            device.pair().await?;
        }
        if let Ok(resolved_addr) = device.address().await {
            let resolved_norm = resolved_addr.trim().to_ascii_uppercase();
            if !resolved_norm.is_empty() && !resolved_norm.eq_ignore_ascii_case(mac) {
                let resolved_type = device
                    .address_type()
                    .await
                    .unwrap_or_else(|_| "public".to_string());
                return Ok(Some((resolved_norm, resolved_type)));
            }
        }
    }
    Ok(None)
}

/// Removes a device from BlueZ adapter (forget pairing keys).
pub async fn remove_device(
    conn: &zbus::Connection,
    adapter_path: &str,
    mac: &str,
) -> zbus::Result<()> {
    let Some(dev_path) = find_device_object_path(conn, mac).await? else {
        return Ok(());
    };
    let owning_adapter = dev_path
        .as_str()
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .unwrap_or(adapter_path);
    let adapter_obj = ObjectPath::try_from(owning_adapter)?;
    let adapter = Adapter1Proxy::builder(conn)
        .path(adapter_obj)?
        .build()
        .await?;
    adapter.remove_device(dev_path.as_ref()).await
}

/// Finds the object path of a GATT characteristic belonging to a specific device.
pub async fn find_characteristic_path(
    conn: &zbus::Connection,
    mac: &str,
    target_uuid: &str,
) -> zbus::Result<Option<OwnedObjectPath>> {
    let obj_mgr = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let managed = obj_mgr.get_managed_objects().await?;
    let dev_pattern = format!("dev_{}", mac.replace(':', "_").to_ascii_uppercase());

    for (path, ifaces) in managed {
        if !path.as_str().contains(&dev_pattern) {
            continue;
        }
        if let Some(char_props) = ifaces.get("org.bluez.GattCharacteristic1")
            && let Some(uuid_str) = extract_string(char_props.get("UUID"))
            && uuid_str.eq_ignore_ascii_case(target_uuid)
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Reads the optional Body Sensor Location characteristic (`0x2A38`).
pub async fn read_body_sensor_location(
    conn: &zbus::Connection,
    mac: &str,
) -> zbus::Result<Option<BodySensorLocation>> {
    let Some(path) = find_characteristic_path(conn, mac, super::dbus::BODY_SENSOR_LOC_UUID).await?
    else {
        return Ok(None);
    };
    let characteristic = GattCharacteristic1Proxy::builder(conn)
        .path(path)?
        .build()
        .await?;
    let val = characteristic.read_value(HashMap::new()).await?;
    if val.is_empty() {
        Ok(None)
    } else {
        Ok(Some(parse_body_sensor_location(val[0])))
    }
}

/// Reads the battery percentage from `org.bluez.Battery1` if available, or falls back to
/// standard GATT Battery Level characteristic (`0x2A19`).
pub async fn read_battery_percentage(
    conn: &zbus::Connection,
    mac: &str,
) -> zbus::Result<Option<u8>> {
    if let Some(path) = find_device_object_path(conn, mac).await?
        && let Ok(builder) = Battery1Proxy::builder(conn).path(path)
        && let Ok(battery) = builder.build().await
        && let Ok(pct) = battery.percentage().await
    {
        return Ok(Some(pct));
    }

    if let Some(path) =
        find_characteristic_path(conn, mac, super::dbus::BATTERY_LEVEL_CHAR_UUID).await?
        && let Ok(builder) = GattCharacteristic1Proxy::builder(conn).path(path)
        && let Ok(characteristic) = builder.build().await
        && let Ok(val) = characteristic.read_value(HashMap::new()).await
        && !val.is_empty()
    {
        return Ok(Some(val[0]));
    }

    Ok(None)
}

pub(crate) fn extract_string(val: Option<&OwnedValue>) -> Option<String> {
    val.and_then(|v| {
        if let Ok(s) = <&str>::try_from(v) {
            Some(s.to_string())
        } else {
            None
        }
    })
}

fn extract_i16(val: Option<&OwnedValue>) -> Option<i16> {
    val.and_then(|v| i16::try_from(v).ok())
}

pub(crate) fn extract_u8(val: Option<&OwnedValue>) -> Option<u8> {
    val.and_then(|v| u8::try_from(v).ok())
}

pub(crate) fn extract_bool(val: Option<&OwnedValue>) -> Option<bool> {
    val.and_then(|v| bool::try_from(v).ok())
}

pub(crate) fn extract_byte_array(val: Option<&OwnedValue>) -> Option<Vec<u8>> {
    let v: &Value<'_> = val?;
    let Value::Array(arr) = v else {
        return None;
    };
    let mut out = Vec::with_capacity(arr.len());
    for i in 0..arr.len() {
        let byte = arr.get::<u8>(i).ok().flatten()?;
        out.push(byte);
    }
    Some(out)
}

fn extract_string_list(val: Option<&OwnedValue>) -> Vec<String> {
    val.and_then(|v| {
        let val: &Value = v;
        if let Value::Array(arr) = val {
            let mut res = Vec::new();
            for i in 0..arr.len() {
                if let Ok(Some(s)) = arr.get::<&str>(i) {
                    res.push(s.to_string());
                }
            }
            Some(res)
        } else {
            None
        }
    })
    .unwrap_or_default()
}
