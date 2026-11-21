pub mod client;
pub mod framing;
pub mod protocol;
pub mod pubsub;
pub mod server;

pub use client::{IpcClient, IpcClientError};
pub use protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrollAuthResponseParams,
    EnrolledWearableStatus, EnrollmentUpdateEvent, EpochUpdateEvent, GraceTickEvent, HrSampleEvent,
    OfflineStatus, PairingPrompt, PauseMode, SessionClosedEvent, SessionStartedEvent,
    WearablesUpdatedEvent, alert_ids,
};
