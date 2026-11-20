//! BlueZ `org.bluez.GattProfile1` export for zero-polling background reconnection (ADR-0003).

use zbus::interface;

use super::dbus::HRS_UUID;

/// Object path root for BPMute's exported GATT profile hierarchy.
pub const GATT_PROFILE_APP_PATH: &str = "/org/bpmute/gatt";
/// Object path for the specific Heart Rate Service profile.
pub const GATT_HRS_PROFILE_PATH: &str = "/org/bpmute/gatt/hrs_profile";

/// Server-side D-Bus object implementing `org.bluez.GattProfile1`.
pub struct GattProfileHrs;

#[interface(name = "org.bluez.GattProfile1")]
impl GattProfileHrs {
    /// Called by BlueZ when the profile registration is released or unregistered.
    fn release(&self) {
        tracing::debug!("BlueZ GattProfile1 for HRS released");
    }

    /// List of 128-bit UUID strings matching the profile.
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> Vec<String> {
        vec![HRS_UUID.to_string()]
    }
}
