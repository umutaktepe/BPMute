//! High-level D-Bus operations for BlueZ LE discovery, bonding, device removal, and GATT inspection.

use std::collections::HashMap;

use zbus::fdo::ObjectManagerProxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::ipc::protocol::DiscoveredWearable;

use super::dbus::{Adapter1Proxy, Battery1Proxy, Device1Proxy, GattCharacteristic1Proxy, HRS_UUID};
use super::packet::{BodySensorLocation, parse_body_sensor_location};

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

/// Discovers all known or currently discovered BlueZ devices exposing Heart Rate Service (0x180D).
pub async fn list_heart_rate_devices(
    conn: &zbus::Connection,
) -> zbus::Result<Vec<DiscoveredWearable>> {
    let obj_mgr = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;
    let managed = obj_mgr.get_managed_objects().await?;
    let mut devices = Vec::new();

    for (_path, ifaces) in managed {
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
    let uuids = vec![Value::from(HRS_UUID)];
    filter.insert("UUIDs", Value::from(uuids));

    let _ = adapter.set_discovery_filter(filter).await;
    adapter.start_discovery().await
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

/// Initiates connection to a BlueZ device by MAC address.
pub async fn connect_device(conn: &zbus::Connection, mac: &str) -> zbus::Result<()> {
    let Some(path) = find_device_object_path(conn, mac).await? else {
        return Err(zbus::Error::Failure(format!(
            "device {mac} not found in BlueZ"
        )));
    };
    let device = Device1Proxy::builder(conn).path(path)?.build().await?;
    device.connect().await
}

/// Sets a device as trusted and optionally initiates pairing (adaptive bonding).
pub async fn bond_and_trust_device(
    conn: &zbus::Connection,
    mac: &str,
    pair: bool,
) -> zbus::Result<()> {
    let Some(path) = find_device_object_path(conn, mac).await? else {
        return Err(zbus::Error::Failure(format!(
            "device {mac} not found in BlueZ"
        )));
    };
    let device = Device1Proxy::builder(conn).path(path)?.build().await?;
    let _ = device.set_trusted(true).await;
    if pair {
        let is_paired = device.paired().await.unwrap_or(false);
        if !is_paired {
            let _ = device.pair().await;
        }
    }
    Ok(())
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
    let adapter_obj = ObjectPath::try_from(adapter_path)?;
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

/// Reads the battery percentage from `org.bluez.Battery1` if available.
pub async fn read_battery_percentage(
    conn: &zbus::Connection,
    mac: &str,
) -> zbus::Result<Option<u8>> {
    let Some(path) = find_device_object_path(conn, mac).await? else {
        return Ok(None);
    };
    let battery = Battery1Proxy::builder(conn).path(path)?.build().await?;
    battery.percentage().await.map(Some)
}

fn extract_string(val: Option<&OwnedValue>) -> Option<String> {
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

fn extract_bool(val: Option<&OwnedValue>) -> Option<bool> {
    val.and_then(|v| bool::try_from(v).ok())
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
