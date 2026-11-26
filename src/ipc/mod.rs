pub mod client;
pub mod framing;
pub mod protocol;
pub mod pubsub;
pub mod server;

pub use client::{IpcClient, IpcClientError};
pub use protocol::{
    AlertChangedEvent, DaemonAlert, DaemonStatus, DiscoveredWearable, EnrolledWearableStatus,
    EnrollmentUpdateEvent, EpochUpdateEvent, GraceTickEvent, HrSampleEvent, OfflineStatus,
    PairingPrompt, PauseMode, SessionClosedEvent, SessionDiscardedEvent, SessionStartedEvent,
    SnapshotEvent, WearablesUpdatedEvent, alert_ids,
};
