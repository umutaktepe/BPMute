//! Pure-Rust BlueZ D-Bus integration, GATT parsing, and zero-polling auto-reconnect (ADR-0001, ADR-0003).
//!
//! Subsystem structure:
//! - [`dbus`]: Typed `zbus::proxy` interfaces (`Adapter1`, `Device1`, `GattManager1`, `GattCharacteristic1`, `Battery1`, `AgentManager1`) and RPA helpers.
//! - [`operations`]: Stateless BlueZ `ObjectManager` inspection, LE discovery, adaptive bonding, and GATT characteristic lookup.
//! - [`profile`]: Exported `org.bluez.GattProfile1` (`0x180D`) D-Bus object under `/org/bpmute/gatt`.
//! - [`agent`]: Temporary enrollment-scoped `org.bluez.Agent1` bridging interactive PIN/passkey prompts over daemon IPC.
//! - [`packet`]: Bluetooth SIG `0x2A37` Heart Rate Measurement and `0x2A38` Body Sensor Location parsers.
//! - [`stream`]: Pure-sync staged BLE stream state machine (`StreamTracker`), silence timers, and multi-wearable standby arbitration.
//! - [`service`]: Background `BluezWearableSource` listening to BlueZ `PropertiesChanged`/`InterfacesAdded` signals and driving `StreamTracker`.

pub mod agent;
pub mod dbus;
pub mod operations;
pub mod packet;
pub mod profile;
pub mod service;
pub mod stream;

pub use packet::{
    BodySensorLocation, HrPacketError, parse_body_sensor_location, parse_hr_measurement,
};
pub use service::BluezWearableSource;
pub use stream::{DisconnectReason, LifecycleAction, StreamEvent, StreamTracker};
