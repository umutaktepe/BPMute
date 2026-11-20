use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::calibration::{CalibrationState, FeedbackVerdict};
use crate::config::{ClockTime, Config, LogFormat, PatchField, ScheduleConfig};
use crate::replay::TraceFormat;
use crate::sleep_engine::{ContactStatus, DetectorState, Epoch, StreamStatus};
use crate::storage::{EnrolledWearable, ImportMode, ImportSummary, SessionFullDetail};

/// Current NDJSON IPC protocol version (ADR-0002).
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum NDJSON line length in bytes (`1 MiB`).
pub const MAX_FRAME_BYTES: usize = 1_048_576;

/// Canonical daemon alert identifiers shared across daemon, IPC, and TUI.
pub mod alert_ids {
    pub const CONFIG_RELOAD_FAILED: &str = "config_reload_failed";
    pub const PERSISTENT_SHIFT: &str = "calibration_persistent_shift";
    pub const DEGRADED_IDLE: &str = "idle_backend_degraded";
    pub const INHIBITOR_SENSITIVE: &str = "idle_backend_inhibitor_sensitive";
    pub const LOGIND_INHIBITED: &str = "logind_inhibited";
}

/// Standardized IPC error code identifiers (ADR-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IpcErrorCode {
    UnsupportedVersion,
    FrameTooLarge,
    InvalidJson,
    UnknownOp,
    InvalidParams,
    InvalidConfig,
    ValidationError,
    PreconditionFailed,
    NotFound,
    PermissionDenied,
    InternalError,
    Conflict,
    BackendRefused,
    EnrollmentInProgress,
}

impl IpcErrorCode {
    /// Returns the canonical SCREAMING_SNAKE_CASE identifier.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedVersion => "UNSUPPORTED_VERSION",
            Self::FrameTooLarge => "FRAME_TOO_LARGE",
            Self::InvalidJson => "INVALID_JSON",
            Self::UnknownOp => "UNKNOWN_OP",
            Self::InvalidParams => "INVALID_PARAMS",
            Self::InvalidConfig => "INVALID_CONFIG",
            Self::ValidationError => "VALIDATION_ERROR",
            Self::PreconditionFailed => "PRECONDITION_FAILED",
            Self::NotFound => "NOT_FOUND",
            Self::PermissionDenied => "PERMISSION_DENIED",
            Self::InternalError => "INTERNAL_ERROR",
            Self::Conflict => "CONFLICT",
            Self::BackendRefused => "BACKEND_REFUSED",
            Self::EnrollmentInProgress => "ENROLLMENT_IN_PROGRESS",
        }
    }
}

impl fmt::Display for IpcErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for IpcErrorCode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "UNSUPPORTED_VERSION" => Ok(Self::UnsupportedVersion),
            "FRAME_TOO_LARGE" => Ok(Self::FrameTooLarge),
            "INVALID_JSON" => Ok(Self::InvalidJson),
            "UNKNOWN_OP" => Ok(Self::UnknownOp),
            "INVALID_PARAMS" => Ok(Self::InvalidParams),
            "INVALID_CONFIG" => Ok(Self::InvalidConfig),
            "VALIDATION_ERROR" => Ok(Self::ValidationError),
            "PRECONDITION_FAILED" => Ok(Self::PreconditionFailed),
            "NOT_FOUND" => Ok(Self::NotFound),
            "PERMISSION_DENIED" => Ok(Self::PermissionDenied),
            "INTERNAL_ERROR" => Ok(Self::InternalError),
            "CONFLICT" => Ok(Self::Conflict),
            "BACKEND_REFUSED" => Ok(Self::BackendRefused),
            "ENROLLMENT_IN_PROGRESS" => Ok(Self::EnrollmentInProgress),
            other => Err(format!("unknown IPC error code `{other}`")),
        }
    }
}

pub mod error_codes {
    use super::IpcErrorCode;

    pub const UNSUPPORTED_VERSION: &str = IpcErrorCode::UnsupportedVersion.as_str();
    pub const FRAME_TOO_LARGE: &str = IpcErrorCode::FrameTooLarge.as_str();
    pub const INVALID_JSON: &str = IpcErrorCode::InvalidJson.as_str();
    pub const UNKNOWN_OP: &str = IpcErrorCode::UnknownOp.as_str();
    pub const INVALID_PARAMS: &str = IpcErrorCode::InvalidParams.as_str();
    pub const INVALID_CONFIG: &str = IpcErrorCode::InvalidConfig.as_str();
    pub const VALIDATION_ERROR: &str = IpcErrorCode::ValidationError.as_str();
    pub const PRECONDITION_FAILED: &str = IpcErrorCode::PreconditionFailed.as_str();
    pub const NOT_FOUND: &str = IpcErrorCode::NotFound.as_str();
    pub const PERMISSION_DENIED: &str = IpcErrorCode::PermissionDenied.as_str();
    pub const INTERNAL_ERROR: &str = IpcErrorCode::InternalError.as_str();
    pub const CONFLICT: &str = IpcErrorCode::Conflict.as_str();
    pub const BACKEND_REFUSED: &str = IpcErrorCode::BackendRefused.as_str();
    pub const ENROLLMENT_IN_PROGRESS: &str = IpcErrorCode::EnrollmentInProgress.as_str();
}

/// Incoming client NDJSON request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub v: u32,
    pub id: u64,
    pub op: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

/// Structured error payload returned inside failed responses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: String,
    pub message: String,
}

/// Outgoing server NDJSON response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub v: u32,
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

impl ResponseEnvelope {
    /// Constructs a successful response envelope.
    pub fn ok(id: u64, data: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            data: Some(data),
            error: None,
        }
    }

    /// Constructs a failed response envelope.
    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            data: None,
            error: Some(ErrorPayload {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// Server push event envelope delivered to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushEventEnvelope {
    pub v: u32,
    pub event: String,
    pub seq: u64,
    pub data: Value,
}

/// Active monitoring pause mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseMode {
    Indefinite,
    Duration,
    UntilMorning,
}

/// Parameters accepted by `monitor.pause`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PauseParams {
    pub mode: Option<PauseMode>,
    pub duration_sec: Option<u64>,
}

/// Parameters accepted by `monitor.set_dry_run`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDryRunParams {
    pub dry_run: bool,
}

/// Severity level for daemon alerts surfaced in status and TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertSeverity {
    Info,
    Warn,
    Error,
}

/// Actionable or diagnostic alert held by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonAlert {
    pub id: String,
    pub kind: String,
    pub severity: AlertSeverity,
    pub message: String,
    pub dismissible: bool,
}

/// High-level daemon monitoring state reported in `daemon.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitoringState {
    Idle,
    Warmup,
    Monitoring,
    Candidate,
    Confirmed,
    Sleeping,
    Paused,
}

impl From<DetectorState> for MonitoringState {
    fn from(value: DetectorState) -> Self {
        match value {
            DetectorState::Idle => Self::Idle,
            DetectorState::Warmup => Self::Warmup,
            DetectorState::Monitoring => Self::Monitoring,
            DetectorState::Candidate => Self::Candidate,
            DetectorState::Confirmed => Self::Confirmed,
            DetectorState::Sleeping => Self::Sleeping,
        }
    }
}

impl std::fmt::Display for MonitoringState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Idle => "Idle",
            Self::Warmup => "Warmup",
            Self::Monitoring => "Monitoring",
            Self::Candidate => "Candidate",
            Self::Confirmed => "Confirmed",
            Self::Sleeping => "Sleeping",
            Self::Paused => "Paused",
        };
        f.write_str(label)
    }
}

/// Online `daemon.status` / `bpmute status --json` payload (ADR-0002, ADR-0009).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub v: u32,
    pub daemon_running: bool,
    pub version: String,
    pub pid: u32,
    pub uptime_sec: u64,
    pub monitoring_state: MonitoringState,
    pub armed: bool,
    pub paused: bool,
    pub paused_until: Option<String>,
    pub pause_mode: Option<PauseMode>,
    pub dry_run: bool,
    pub schedule: ScheduleConfig,
    pub config_path: String,
    pub config_loaded_from_file: bool,
    pub socket_path: String,
    pub log_biometrics: bool,
    pub alerts: Vec<DaemonAlert>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_backend: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing_players_count: Option<usize>,
}

/// Offline `bpmute status --json` payload (ADR-0002, ADR-0009).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineStatus {
    pub v: u32,
    pub daemon_running: bool,
}

impl Default for OfflineStatus {
    fn default() -> Self {
        Self {
            v: PROTOCOL_VERSION,
            daemon_running: false,
        }
    }
}

/// Effective configuration payload returned by `config.get`, `config.update`, `config.reload`,
/// and `ConfigUpdated` push events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSnapshotPayload {
    pub config: Config,
    pub file_present: bool,
    pub config_path: String,
    pub log_filter: String,
    pub log_format: LogFormat,
    pub log_biometrics: bool,
}

/// Parameters accepted by `session.list`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionListParams {
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Parameters accepted by `session.get`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGetParams {
    pub id: String,
}

/// Parameters accepted by `session.set_feedback`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFeedbackParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "PatchField::is_unchanged")]
    pub verdict: PatchField<FeedbackVerdict>,
    #[serde(default, skip_serializing_if = "PatchField::is_unchanged")]
    pub onset_hhmm: PatchField<ClockTime>,
    #[serde(default, skip_serializing_if = "PatchField::is_unchanged")]
    pub avg_sleep_hr: PatchField<f64>,
    #[serde(default)]
    pub clear: bool,
}

/// Parameters accepted by `calibration.set_seed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSeedParams {
    pub sleep_hr: Option<f64>,
}

/// Parameters accepted by `session.delete`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleteParams {
    pub id: String,
}

/// Parameters accepted by `session.export_trace`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionExportTraceParams {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub format: Option<TraceFormat>,
    #[serde(default)]
    pub no_anonymize: bool,
}

/// Response payload returned by `session.delete`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDeleteResponse {
    pub deleted_session_id: String,
    pub deleted_id: String,
}

/// Response payload returned by `session.export_trace`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionExportTraceResponse {
    pub path: String,
    pub events_count: usize,
}

/// Response payload returned by `data.purge_raw`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataPurgeRawResponse {
    pub reclaimed_rows: usize,
}

/// Response payload returned by `data.export_bundle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataExportBundleResponse {
    pub path: String,
}

/// Response payload returned by `data.import_bundle`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataImportBundleResponse {
    pub summary: ImportSummary,
    pub calibration: CalibrationState,
}

/// Response payload returned by `wearable.list`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearableListResponse {
    pub wearables: Vec<EnrolledWearableStatus>,
}

/// Response payload returned by `wearable.rename` and `wearable.unenroll`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearableMutationResponse {
    pub wearable: EnrolledWearable,
}

/// Response payload returned by `wearable.enroll_start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollStartResponse {
    pub devices: Vec<DiscoveredWearable>,
    pub wearables: Vec<DiscoveredWearable>,
}

/// Response payload returned by `wearable.enroll_select`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollSelectResponse {
    pub wearable: EnrolledWearable,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub hints: Vec<String>,
}

/// Parameters accepted by `calibration.set_boundary`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CalibrationSetBoundaryParams {
    pub effective_ts_ms: Option<i64>,
    pub seed_sleep_hr: Option<f64>,
    pub reset_floors: bool,
}

/// Parameters accepted by `data.hard_wipe`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DataHardWipeParams {
    pub all: bool,
}

/// Parameters accepted by `data.export_bundle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataExportBundleParams {
    pub path: String,
    #[serde(default)]
    pub include_epochs: bool,
    #[serde(default)]
    pub include_wearables: bool,
    #[serde(default)]
    pub include_raw: bool,
}

impl DataExportBundleParams {
    pub fn options(&self) -> crate::storage::ExportBundleOptions {
        crate::storage::ExportBundleOptions {
            include_epochs: self.include_epochs,
            include_wearables: self.include_wearables,
            include_raw: self.include_raw,
        }
    }
}

/// Parameters accepted by `data.import_bundle`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataImportBundleParams {
    pub path: String,
    #[serde(default)]
    pub mode: Option<ImportMode>,
    #[serde(default)]
    pub force: bool,
}

/// Parameters accepted by `data.backup_sqlite`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataBackupSqliteParams {
    pub path: String,
}

/// Stream event payload pushed as `HrSample` (and buffered for `Snapshot`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HrSampleEvent {
    pub ts_ms: i64,
    pub bpm: f64,
    #[serde(default)]
    pub rr_ms: Vec<f64>,
    pub contact_status: ContactStatus,
    pub wearable_id: String,
    pub session_id: Option<String>,
}

/// Control event payload pushed as `GraceTick` during active `Grace Period`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraceTickEvent {
    pub session_id: String,
    pub remaining_sec: u32,
    pub total_sec: u32,
    pub power_action: crate::config::PowerAction,
    pub dry_run: bool,
    #[serde(default)]
    pub planned_steps: Vec<String>,
}

/// Control event payload pushed as `EpochUpdate` every 15-second `Epoch`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpochUpdateEvent {
    pub session_id: String,
    pub wearable_id: String,
    pub epoch: Epoch,
}

/// Control event payload pushed as `SessionClosed` when a `Monitoring Session` finalizes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionClosedEvent {
    pub session: SessionFullDetail,
}

/// Control event payload pushed as `CalibrationUpdated` after any ledger recompute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationUpdatedEvent {
    pub calibration: CalibrationState,
}

/// Control event payload pushed as `WearablesUpdated` when wearable status or allowlist changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearablesUpdatedEvent {
    pub wearables: Vec<EnrolledWearableStatus>,
}

/// Discovered Bluetooth LE wearable observed during enrollment discovery scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredWearable {
    pub address: String,
    pub address_type: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
    pub paired: bool,
    pub already_enrolled: bool,
    #[serde(default)]
    pub uuids: Vec<String>,
}

/// Enrolled wearable status including live connectivity, battery percentage, and body location.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrolledWearableStatus {
    #[serde(flatten)]
    pub wearable: EnrolledWearable,
    pub status: StreamStatus,
    pub battery: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_location: Option<String>,
}

/// Parameters for `wearable.rename`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WearableRenameParams {
    pub id: String,
    pub alias: Option<String>,
}

/// Parameters for `wearable.connect`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WearableConnectParams {
    pub id: String,
}

/// Parameters for `wearable.unenroll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WearableUnenrollParams {
    pub id: String,
    #[serde(default)]
    pub forget_bluez: bool,
    #[serde(default)]
    pub delete_sessions: bool,
}

/// Parameters for `wearable.enroll_start`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnrollStartParams {
    pub timeout_sec: Option<u32>,
}

/// Parameters for `wearable.enroll_select`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollSelectParams {
    pub address: String,
    pub alias: Option<String>,
}

/// Parameters for `wearable.enroll_auth_response`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnrollAuthResponseParams {
    pub confirmed: Option<bool>,
    pub passkey: Option<u32>,
    pub pincode: Option<String>,
}

/// Interactive Agent1 pairing prompt bridged over IPC (ADR-0002, ADR-0003).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PairingPrompt {
    Confirmation { passkey: u32, wearable: String },
    Passkey { wearable: String },
    PinCode { wearable: String },
}

/// Control event payload pushed as `EnrollmentUpdate` during active wearable discovery.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollmentUpdateEvent {
    #[serde(default, alias = "devices")]
    pub wearables: Vec<DiscoveredWearable>,
    #[serde(default)]
    pub pairing_prompt: Option<PairingPrompt>,
    #[serde(default)]
    pub remaining_sec: Option<u32>,
}
