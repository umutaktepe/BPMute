//! Pure-Rust BlueZ D-Bus integration, GATT parsing, and zero-polling auto-reconnect (ADR-0001, ADR-0003).

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
