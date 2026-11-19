//! Pure-Rust zbus proxies and interface definitions for BlueZ D-Bus (ADR-0001, ADR-0003).

use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::{ObjectPath, Value};

/// Standard BlueZ 16-bit Heart Rate Service UUID in expanded 128-bit form.
pub const HRS_UUID: &str = "0000180d-0000-1000-8000-00805f9b34fb";
/// Standard BlueZ 16-bit Heart Rate Measurement characteristic UUID.
pub const HRM_CHAR_UUID: &str = "00002a37-0000-1000-8000-00805f9b34fb";
/// Standard BlueZ 16-bit Body Sensor Location characteristic UUID.
pub const BODY_SENSOR_LOC_UUID: &str = "00002a38-0000-1000-8000-00805f9b34fb";

#[proxy(default_service = "org.bluez", interface = "org.bluez.Adapter1")]
pub trait Adapter1 {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn set_discovery_filter(&self, filter: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
    fn remove_device(&self, device: ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
}

#[proxy(default_service = "org.bluez", interface = "org.bluez.Device1")]
pub trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn pair(&self) -> zbus::Result<()>;
    fn cancel_pairing(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn address_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn bonded(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn trusted(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_trusted(&self, value: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn services_resolved(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn uuids(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn adapter(&self) -> zbus::Result<ObjectPath<'_>>;
    #[zbus(property)]
    fn blocked(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn modalias(&self) -> zbus::Result<String>;
}

#[proxy(default_service = "org.bluez", interface = "org.bluez.GattManager1")]
pub trait GattManager1 {
    fn register_application(
        &self,
        application: ObjectPath<'_>,
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_application(&self, application: ObjectPath<'_>) -> zbus::Result<()>;
    fn register_profile(
        &self,
        profile: ObjectPath<'_>,
        uuids: &[&str],
        options: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_profile(&self, profile: ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(
    default_service = "org.bluez",
    interface = "org.bluez.GattCharacteristic1"
)]
pub trait GattCharacteristic1 {
    fn start_notify(&self) -> zbus::Result<()>;
    fn stop_notify(&self) -> zbus::Result<()>;
    fn read_value(&self, flags: HashMap<&str, Value<'_>>) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn uuid(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn value(&self) -> zbus::Result<Vec<u8>>;
    #[zbus(property)]
    fn notifying(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn service(&self) -> zbus::Result<ObjectPath<'_>>;
    #[zbus(property)]
    fn flags(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(default_service = "org.bluez", interface = "org.bluez.Battery1")]
pub trait Battery1 {
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<u8>;
}

#[proxy(
    default_service = "org.bluez",
    interface = "org.bluez.AgentManager1",
    default_path = "/org/bluez"
)]
pub trait AgentManager1 {
    fn register_agent(&self, agent: ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: ObjectPath<'_>) -> zbus::Result<()>;
}

/// Checks whether a Bluetooth device address and type classify as a Resolvable Private Address (RPA).
pub fn is_resolvable_private_address(address: &str, address_type: &str) -> bool {
    if !address_type.eq_ignore_ascii_case("random") {
        return false;
    }
    let parts: Vec<&str> = address.split(':').collect();
    if parts.len() != 6 {
        return false;
    }
    if let Ok(first_byte) = u8::from_str_radix(parts[0], 16) {
        // Top 2 bits: 01 indicates Resolvable Private Address (RPA)
        (first_byte & 0xC0) == 0x40
    } else {
        false
    }
}

/// Checks whether a device indicates a dual-mode BR/EDR preferred bearer or classic profile.
pub fn is_dual_mode_bredr_candidate(uuids: &[String]) -> bool {
    // Check for common BR/EDR audio profiles: Handsfree (0x111E), A2DP Sink (0x110B), Audio Source (0x110A)
    uuids.iter().any(|u| {
        let lower = u.to_ascii_lowercase();
        lower.contains("0000111e") || lower.contains("0000110b") || lower.contains("0000110a")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpa_detection_identifies_resolvable_random_addresses() {
        // Top 2 bits 01: 0x40..=0x7F
        assert!(is_resolvable_private_address("4A:12:34:56:78:90", "random"));
        assert!(is_resolvable_private_address("7F:AA:BB:CC:DD:EE", "random"));
        // Top 2 bits 11: Static random address (0xC0..=0xFF) -> not RPA
        assert!(!is_resolvable_private_address(
            "C5:12:34:56:78:90",
            "random"
        ));
        assert!(!is_resolvable_private_address(
            "FF:12:34:56:78:90",
            "random"
        ));
        // Top 2 bits 00: Non-resolvable private address (0x00..=0x3F) -> not RPA
        assert!(!is_resolvable_private_address(
            "1A:12:34:56:78:90",
            "random"
        ));
        // Public address type -> not RPA
        assert!(!is_resolvable_private_address(
            "4A:12:34:56:78:90",
            "public"
        ));
    }

    #[test]
    fn dual_mode_detection_identifies_classic_audio_uuids() {
        let bredr_uuids = vec![
            "0000180d-0000-1000-8000-00805f9b34fb".to_string(),
            "0000111e-0000-1000-8000-00805f9b34fb".to_string(), // Handsfree
        ];
        assert!(is_dual_mode_bredr_candidate(&bredr_uuids));

        let pure_le_uuids = vec![
            "0000180d-0000-1000-8000-00805f9b34fb".to_string(),
            "0000180f-0000-1000-8000-00805f9b34fb".to_string(),
        ];
        assert!(!is_dual_mode_bredr_candidate(&pure_le_uuids));
    }
}
