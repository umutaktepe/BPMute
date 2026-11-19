//! Local SQLite WAL persistence, `flock` coordination, and dedicated ledger worker (ADR-0005, ADR-0008).

pub mod bundle;
pub mod lock;
pub mod schema;
pub mod store;
pub mod worker;

use std::fmt;
use std::io;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::calibration::{
    CalibrationBoundaryRecord, CalibrationState, FeedbackVerdict, SessionFeedbackRecord,
    SessionLedgerRecord,
};
use crate::config::{ClockTime, PatchField, PowerAction, Sensitivity};
use crate::exit_code::ExitCode;
use crate::ipc::protocol::error_codes;
use crate::sleep_engine::{
    CalibrationMaturity, CalibrationPriors, Epoch, HrPacket, SessionEndReason, SessionSummary,
    WearableFloor,
};

pub use bundle::{
    CURRENT_BUNDLE_SCHEMA_VERSION, CalibrationMetaBundleRecord, ImportMode, ImportSummary,
    LedgerBundle, RawSampleBundleRecord, SessionBundleRecord, SessionEpochBundleRecord,
    validate_bundle_payload,
};
pub use lock::LedgerFileLock;
pub use schema::{backups_dir_in_data_dir, db_exists, db_path_in_data_dir, lock_path_in_data_dir};
pub use store::LedgerStore;
pub use worker::PersistenceWorker;

/// Canonical default address type for Bluetooth LE devices.
pub fn default_address_type() -> String {
    "public".to_string()
}

/// Metadata describing an enrolled Wearable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolledWearable {
    pub id: String,
    pub mac: String,
    pub mac_masked: String,
    pub address_type: String,
    pub alias: Option<String>,
    pub advertised_name: Option<String>,
    pub supports_rr: bool,
    pub supports_contact: bool,
    pub hr_var_floor: f64,
    pub typical_sqi: Option<f64>,
    pub enrolled_at_ms: i64,
    pub last_seen_at_ms: i64,
    pub active: bool,
}

/// Errors returned by SQLite WAL storage, `flock` locking, or the persistence worker.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON serialization error in ledger: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    AmbiguousId(String),
    #[error("validation error: {0}")]
    Validation(String),
    #[error("precondition conflict: {0}")]
    PreconditionFailed(String),
    #[error("persistence worker stopped unexpectedly")]
    WorkerStopped,
}

impl StorageError {
    /// Maps this error to its semantic CLI exit code (`0`–`5`, ADR-0009).
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Io(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                ExitCode::PermissionOrBackendRefusal
            }
            Self::Sqlite(err) if is_sqlite_permission_error(err) => {
                ExitCode::PermissionOrBackendRefusal
            }
            Self::Validation(_) | Self::AmbiguousId(_) => ExitCode::UsageOrConfigError,
            Self::PreconditionFailed(_) => ExitCode::PreconditionConflict,
            Self::NotFound(_)
            | Self::Io(_)
            | Self::Sqlite(_)
            | Self::Serde(_)
            | Self::WorkerStopped => ExitCode::RuntimeError,
        }
    }

    /// Maps this error to the corresponding IPC error code string.
    pub fn ipc_code(&self) -> &'static str {
        match self {
            Self::Io(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                error_codes::PERMISSION_DENIED
            }
            Self::Sqlite(err) if is_sqlite_permission_error(err) => error_codes::PERMISSION_DENIED,
            Self::Validation(_) | Self::AmbiguousId(_) => error_codes::VALIDATION_ERROR,
            Self::PreconditionFailed(_) => error_codes::PRECONDITION_FAILED,
            Self::NotFound(_) => error_codes::NOT_FOUND,
            Self::Io(_) | Self::Sqlite(_) | Self::Serde(_) | Self::WorkerStopped => {
                error_codes::INTERNAL_ERROR
            }
        }
    }
}

fn is_sqlite_permission_error(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(ffi, _)
            if matches!(
                ffi.code,
                rusqlite::ErrorCode::PermissionDenied
                    | rusqlite::ErrorCode::ReadOnly
                    | rusqlite::ErrorCode::CannotOpen
            )
    )
}

/// Persisted lifecycle state of a `Monitoring Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Active,
    Closed,
}

impl SessionStatus {
    /// Returns the canonical lowercase session status stored in SQLite.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Closed => "closed",
        }
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SessionStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "active" => Ok(Self::Active),
            "closed" => Ok(Self::Closed),
            other => Err(format!("unknown session status `{other}`")),
        }
    }
}

/// Metadata describing an observed or enrolled Wearable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WearableDescriptor {
    pub mac: String,
    #[serde(default = "default_address_type")]
    pub address_type: String,
    pub alias: Option<String>,
    pub advertised_name: Option<String>,
    pub supports_rr: bool,
    pub supports_contact: bool,
}

/// Persisted Wearable summary exposed in session/calibration queries (MAC always masked).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearableInfo {
    pub id: String,
    pub mac_masked: String,
    pub alias: Option<String>,
    pub advertised_name: Option<String>,
    pub supports_rr: bool,
    pub supports_contact: bool,
    pub hr_var_floor: f64,
    pub typical_sqi: Option<f64>,
}

/// Parameters for persisting a newly opened active `Monitoring Session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSessionRequest {
    pub wearable_id: String,
    pub start_ts_ms: i64,
    pub detection_sensitivity: Sensitivity,
}

/// Parameters for finalizing a closed `Monitoring Session` and recomputing calibration.
#[derive(Debug, Clone, PartialEq)]
pub struct CloseSessionRequest {
    pub session_id: String,
    pub summary: SessionSummary,
    pub trigger_power_action: PowerAction,
    pub rapid_resume_within_5m: bool,
}

/// Compact session row returned by `sessions list` / `session.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub id: String,
    pub wearable_id: String,
    pub wearable_alias: Option<String>,
    pub wearable_mac_masked: String,
    pub status: SessionStatus,
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub end_reason: Option<SessionEndReason>,
    pub detection_sensitivity: Sensitivity,
    pub had_rr: bool,
    pub warmup_mu_hr: Option<f64>,
    pub sleep_mu_hr: Option<f64>,
    pub onset_confirmed_ts_ms: Option<i64>,
    pub sleeping_ts_ms: Option<i64>,
    pub auto_anomalous: bool,
    pub fp_count: u8,
    pub epochs_count: u32,
    pub raw_samples_count: u32,
    pub feedback: Option<SessionFeedbackRecord>,
}

/// Paginated `sessions list` / `session.list` response payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListPage {
    pub sessions: Vec<SessionListEntry>,
    pub total_count: u64,
    pub limit: u32,
    pub offset: u32,
}

/// Detailed session inspection payload returned by `sessions show` / `session.get`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFullDetail {
    pub session: SessionLedgerRecord,
    pub status: SessionStatus,
    pub wearable: WearableInfo,
    pub feedback: Option<SessionFeedbackRecord>,
    pub epochs: Vec<Epoch>,
    pub has_raw_samples: bool,
}

/// Read-only stored session payload loaded for `bpmute replay --session <id>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionReplayBundle {
    pub session: SessionLedgerRecord,
    pub wearable_floor: WearableFloor,
    pub priors: CalibrationPriors,
    pub raw_samples: Vec<HrPacket>,
    pub epochs: Vec<Epoch>,
}

/// Editable feedback mutation request for a closed `Monitoring Session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetFeedbackRequest {
    pub session_id_or_prefix: String,
    pub verdict: PatchField<FeedbackVerdict>,
    pub onset_hhmm: PatchField<ClockTime>,
    pub avg_sleep_hr: PatchField<f64>,
    pub clear_all: bool,
}

/// Result returned when `accept-shift` creates a `Calibration Boundary`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcceptShiftResult {
    pub boundary: CalibrationBoundaryRecord,
    pub calibration: CalibrationState,
}

/// Retention pruning summary report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PruneSummary {
    pub raw_samples_pruned: usize,
    pub session_epochs_pruned: usize,
}

/// Detailed storage statistics reported by `bpmute data stats [--json]` and `data.stats`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageStats {
    pub db_size_bytes: u64,
    pub db_size_formatted: String,
    pub sessions_count: usize,
    pub session_epochs_count: usize,
    pub raw_samples_count: usize,
    pub session_feedback_count: usize,
    pub calibration_boundaries_count: usize,
    pub wearables_count: usize,
    pub raw_samples_retention: String,
    pub session_epochs_retention: String,
    pub calibration_maturity: CalibrationMaturity,
    pub active_calibration_boundary: Option<CalibrationBoundaryRecord>,
}
