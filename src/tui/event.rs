use crossterm::event::{KeyEvent, MouseEvent};

use crate::calibration::CalibrationState;
use crate::ipc::protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrollAuthResponseParams,
    EnrolledWearableStatus, EnrollmentUpdateEvent, EpochUpdateEvent, GraceTickEvent, HrSampleEvent,
    PairingPrompt, PauseParams,
};

/// Events routed into the TUI event loop.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize(u16, u16),
    Tick,
    Ipc(Box<IpcEvent>),
}

impl TuiEvent {
    pub fn ipc(event: IpcEvent) -> Self {
        Self::Ipc(Box::new(event))
    }
}

/// Asynchronous events produced by the daemon IPC client connection.
#[derive(Debug, Clone)]
pub enum IpcEvent {
    Connected(DaemonStatus),
    Disconnected,
    Snapshot {
        status: DaemonStatus,
        hr_ring_buffer: Vec<HrSampleEvent>,
        alerts: Vec<DaemonAlert>,
    },
    HrSample(HrSampleEvent),
    GraceTick(GraceTickEvent),
    EpochUpdate(EpochUpdateEvent),
    StateChanged(DaemonStatus),
    AlertChanged(Vec<DaemonAlert>),
    CalibrationUpdated(CalibrationState),
    WearablesUpdated(Vec<EnrolledWearableStatus>),
    EnrollmentDiscovered(Vec<DiscoveredWearable>),
    EnrollmentUpdate(EnrollmentUpdateEvent),
    PairingPrompt(PairingPrompt),
    OpCompleted {
        op: String,
        success: bool,
        message: Option<String>,
    },
}

/// Outgoing commands dispatched from the TUI to the IPC client background worker.
#[derive(Debug, Clone, PartialEq)]
pub enum TuiCommand {
    Pause(PauseParams),
    Resume,
    CancelGrace,
    SnoozeGrace(Option<u32>),
    EnrollStart(Option<u32>),
    EnrollSelect {
        address: String,
        alias: Option<String>,
    },
    EnrollCancel,
    EnrollAuthReply(EnrollAuthResponseParams),
    WearableRename {
        id: String,
        alias: Option<String>,
    },
    WearableConnect {
        id: String,
    },
    WearableUnenroll {
        id: String,
        forget_bluez: bool,
        delete_sessions: bool,
    },
    AcceptShift,
    DismissShift,
    DismissAlert {
        id: String,
    },
    RefreshWearables,
}
