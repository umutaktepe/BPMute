use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, LogFormat, ScheduleConfig};

/// Current NDJSON IPC protocol version (ADR-0002).
pub const PROTOCOL_VERSION: u32 = 1;

/// Maximum NDJSON line length in bytes (`1 MiB`).
pub const MAX_FRAME_BYTES: usize = 1_048_576;

pub mod error_codes {
    pub const UNSUPPORTED_VERSION: &str = "UNSUPPORTED_VERSION";
    pub const FRAME_TOO_LARGE: &str = "FRAME_TOO_LARGE";
    pub const INVALID_JSON: &str = "INVALID_JSON";
    pub const UNKNOWN_OP: &str = "UNKNOWN_OP";
    pub const INVALID_PARAMS: &str = "INVALID_PARAMS";
    pub const INVALID_CONFIG: &str = "INVALID_CONFIG";
    pub const PRECONDITION_FAILED: &str = "PRECONDITION_FAILED";
    pub const INTERNAL_ERROR: &str = "INTERNAL_ERROR";
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
#[derive(Debug, Clone, Default, Deserialize)]
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
    Paused,
}

impl std::fmt::Display for MonitoringState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => f.write_str("Idle"),
            Self::Paused => f.write_str("Paused"),
        }
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
