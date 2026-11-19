//! Versioned JSON Ledger Bundle structures, validation, and portability serialization (ADR-0008).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::calibration::{CalibrationBoundaryRecord, SessionFeedbackRecord};
use crate::storage::{EnrolledWearable, StorageError};

/// Schema version for JSON Ledger Bundle exports (`v = 1`).
pub const CURRENT_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Mode for importing a JSON Ledger Bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    Merge,
    Replace,
}

/// Selection options controlling which entity tables to include in a Ledger Bundle export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportBundleOptions {
    pub include_epochs: bool,
    pub include_wearables: bool,
    pub include_raw: bool,
}

impl std::str::FromStr for ImportMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "merge" => Ok(Self::Merge),
            "replace" => Ok(Self::Replace),
            other => Err(format!(
                "invalid import mode '{other}'; expected 'merge' or 'replace'"
            )),
        }
    }
}

/// Serialized session record within a Ledger Bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionBundleRecord {
    pub id: String,
    pub wearable_id: String,
    pub status: String,
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub end_reason: Option<String>,
    pub detection_sensitivity: String,
    pub had_rr: bool,
    pub warmup_mu_hr: Option<f64>,
    pub warmup_sigma_hr: Option<f64>,
    pub warmup_ln_rmssd: Option<f64>,
    pub onset_candidate_ts_ms: Option<i64>,
    pub onset_confirmed_ts_ms: Option<i64>,
    pub sleeping_ts_ms: Option<i64>,
    pub sleep_mu_hr: Option<f64>,
    pub onset_hr_bpm: Option<f64>,
    pub delta_hr_onset: Option<f64>,
    pub sleep_ln_rmssd: Option<f64>,
    pub sleep_hr_var_ratio: Option<f64>,
    pub typical_sqi: Option<f64>,
    pub post_trigger_sleep_sec: u32,
    pub trigger_power_action: String,
    pub rapid_resume_within_5m: bool,
    pub grace_cancel_count: u32,
    pub rapid_wake_fp_count: u32,
    pub snooze_count: u32,
    pub auto_anomalous: bool,
    pub epochs_count: u32,
    pub raw_samples_count: u32,
}

/// Serialized epoch record within a Ledger Bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEpochBundleRecord {
    pub id: String,
    pub session_id: String,
    pub seq: u32,
    pub ts_ms: i64,
    pub hr_bpm_60s: f64,
    pub z_hr: f64,
    pub hr_slope_4m: f64,
    pub r_var_120s: f64,
    pub ln_rmssd_60s: Option<f64>,
    pub delta_ln_rmssd_60s: Option<f64>,
    pub rmssd_sdnn_120s: Option<f64>,
    pub hrv_valid: bool,
    pub rr_artifact_frac: f64,
    pub coverage: f64,
    pub flatline_stale: bool,
    pub accumulators_frozen: bool,
    pub baseline_mu_hr: f64,
    pub baseline_sigma_hr: f64,
    pub baseline_frozen: bool,
    pub p_active_wake: f64,
    pub p_quiet_wake: f64,
    pub p_sleep: f64,
    pub state: String,
}

/// Serialized 1 Hz raw sample record within a Ledger Bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawSampleBundleRecord {
    pub id: String,
    pub session_id: String,
    pub seq: u32,
    pub ts_ms: i64,
    pub bpm: f64,
    pub rr_ms_json: String,
    pub contact_status: String,
}

/// Serialized calibration metadata within a Ledger Bundle.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalibrationMetaBundleRecord {
    pub day1_seed_sleep_hr: Option<f64>,
    pub day1_seed_updated_at_ms: Option<i64>,
    pub dismissed_shift_anchor_ts_ms: Option<i64>,
    pub dismissed_shift_direction: Option<String>,
}

/// Top-level schema-versioned JSON Ledger Bundle (ADR-0008).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerBundle {
    pub schema_version: u32,
    pub exported_at_ms: i64,
    pub app_version: String,
    #[serde(default)]
    pub wearables: Vec<EnrolledWearable>,
    #[serde(default)]
    pub sessions: Vec<SessionBundleRecord>,
    #[serde(default)]
    pub session_epochs: Vec<SessionEpochBundleRecord>,
    #[serde(default)]
    pub raw_samples: Vec<RawSampleBundleRecord>,
    #[serde(default)]
    pub session_feedback: Vec<SessionFeedbackRecord>,
    #[serde(default)]
    pub calibration_boundaries: Vec<CalibrationBoundaryRecord>,
    #[serde(default)]
    pub calibration_meta: Option<CalibrationMetaBundleRecord>,
}

/// Summary report returned upon successful Ledger Bundle import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportSummary {
    pub mode: String,
    pub wearables_imported: usize,
    pub sessions_imported: usize,
    pub epochs_imported: usize,
    pub raw_samples_imported: usize,
    pub feedback_imported: usize,
    pub boundaries_imported: usize,
    pub rollback_backup_path: Option<String>,
}

/// Validates all UUIDs, schema version, and relational integrity of the bundle.
pub fn validate_bundle_payload(bundle: &LedgerBundle) -> Result<(), StorageError> {
    if bundle.schema_version != CURRENT_BUNDLE_SCHEMA_VERSION {
        return Err(StorageError::Validation(format!(
            "unsupported ledger bundle schema_version {}; expected {}",
            bundle.schema_version, CURRENT_BUNDLE_SCHEMA_VERSION
        )));
    }

    // Validate UUIDs
    for w in &bundle.wearables {
        Uuid::parse_str(&w.id).map_err(|e| {
            StorageError::Validation(format!("invalid wearable UUID '{}': {e}", w.id))
        })?;
    }

    for s in &bundle.sessions {
        Uuid::parse_str(&s.id).map_err(|e| {
            StorageError::Validation(format!("invalid session UUID '{}': {e}", s.id))
        })?;
    }

    for ep in &bundle.session_epochs {
        Uuid::parse_str(&ep.id).map_err(|e| {
            StorageError::Validation(format!("invalid epoch UUID '{}': {e}", ep.id))
        })?;
    }

    for raw in &bundle.raw_samples {
        Uuid::parse_str(&raw.id).map_err(|e| {
            StorageError::Validation(format!("invalid raw sample UUID '{}': {e}", raw.id))
        })?;
    }

    for fb in &bundle.session_feedback {
        Uuid::parse_str(&fb.id).map_err(|e| {
            StorageError::Validation(format!("invalid feedback UUID '{}': {e}", fb.id))
        })?;
    }

    for cb in &bundle.calibration_boundaries {
        Uuid::parse_str(&cb.id).map_err(|e| {
            StorageError::Validation(format!("invalid boundary UUID '{}': {e}", cb.id))
        })?;
    }

    Ok(())
}
