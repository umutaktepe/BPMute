use crossterm::event::{KeyEvent, MouseEvent};

use crate::calibration::{CalibrationBoundaryRecord, CalibrationState, FeedbackVerdict};
use crate::config::{ClockTime, Config, ConfigPatch};
use crate::ipc::protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrollAuthResponseParams,
    EnrolledWearableStatus, EnrollmentUpdateEvent, EpochUpdateEvent, GraceTickEvent, HrSampleEvent,
    PauseParams, SessionDiscardedEvent, SessionStartedEvent, SnapshotEvent,
};
use crate::replay::TraceFormat;
use crate::storage::{SessionFullDetail, SessionListEntry, StorageStats};

/// Tick cadence of the interactive TUI event loop in milliseconds.
pub const TUI_TICK_MS: u32 = 250;

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
    Snapshot(Box<SnapshotEvent>),
    HrSample(HrSampleEvent),
    GraceTick(GraceTickEvent),
    EpochUpdate(EpochUpdateEvent),
    SessionStarted(SessionStartedEvent),
    SessionClosed(Box<SessionFullDetail>),
    SessionDiscarded(SessionDiscardedEvent),
    StateChanged(DaemonStatus),
    AlertChanged(Vec<DaemonAlert>),
    CalibrationUpdated(CalibrationState),
    WearablesUpdated(Vec<EnrolledWearableStatus>),
    EnrollmentDiscovered(Vec<DiscoveredWearable>),
    EnrollmentUpdate(EnrollmentUpdateEvent),
    SessionsUpdated {
        sessions: Vec<SessionListEntry>,
        boundaries: Vec<CalibrationBoundaryRecord>,
    },
    SessionDetailLoaded(Box<SessionFullDetail>),
    ConfigLoaded(Config),
    ConfigError(String),
    StorageStatsLoaded(StorageStats),
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
    RefreshStatus,
    RefreshSessions,
    SessionGet {
        id: String,
    },
    SessionSetFeedback {
        session_id: String,
        verdict: Option<FeedbackVerdict>,
        onset_hhmm: Option<ClockTime>,
        avg_sleep_hr: Option<f64>,
        clear: bool,
    },
    SessionExportTrace {
        id: String,
        path: String,
        format: TraceFormat,
        no_anonymize: bool,
    },
    SessionDelete {
        id: String,
    },
    SessionEnd,
    ConfigGet,
    ConfigUpdate(ConfigPatch),
    ConfigReload,
    CalibrationSetSeed(Option<f64>),
    CalibrationSetBoundary {
        effective_ts_ms: Option<i64>,
        seed_sleep_hr: Option<f64>,
        reset_floors: bool,
    },
    CalibrationUndoBoundary,
    DataStats,
    DataPurgeRaw,
    DataHardWipe {
        confirm: String,
        wipe_all: bool,
    },
    EditorHandoff,
}
