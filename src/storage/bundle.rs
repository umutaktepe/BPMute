//! Versioned JSON Ledger Bundle structures, validation, and portability serialization (ADR-0008, ADR-0014).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::calibration::{CalibrationBoundaryRecord, SessionFeedbackRecord};
use crate::storage::{EnrolledWearable, StorageError};

/// Legacy schema version for JSON Ledger Bundle imports (`v = 1`).
pub const LEGACY_BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Schema version for JSON Ledger Bundle exports (`v = 2`).
pub const CURRENT_BUNDLE_SCHEMA_VERSION: u32 = 2;

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
    #[serde(default)]
    pub pre_candidate_mu_hr: Option<f64>,
    #[serde(default)]
    pub pre_candidate_sigma_hr: Option<f64>,
    #[serde(default)]
    pub pre_candidate_ln_rmssd: Option<f64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
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
    if !matches!(
        bundle.schema_version,
        LEGACY_BUNDLE_SCHEMA_VERSION | CURRENT_BUNDLE_SCHEMA_VERSION
    ) {
        return Err(StorageError::Validation(format!(
            "unsupported ledger bundle schema_version {}; expected {} or {}",
            bundle.schema_version, LEGACY_BUNDLE_SCHEMA_VERSION, CURRENT_BUNDLE_SCHEMA_VERSION
        )));
    }

    // Validate top-level UUIDv7 entity identifiers and session references.
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

    validate_child_records(
        &bundle.session_epochs,
        bundle.schema_version,
        "epoch",
        |ep| (&ep.session_id, ep.id.as_deref()),
    )?;
    validate_child_records(
        &bundle.raw_samples,
        bundle.schema_version,
        "raw sample",
        |raw| (&raw.session_id, raw.id.as_deref()),
    )?;

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

fn validate_child_records<'a, T, F>(
    records: &'a [T],
    schema_version: u32,
    kind: &str,
    extract: F,
) -> Result<(), StorageError>
where
    F: Fn(&'a T) -> (&'a str, Option<&'a str>),
{
    for rec in records {
        let (session_id, child_id) = extract(rec);
        Uuid::parse_str(session_id).map_err(|e| {
            StorageError::Validation(format!(
                "invalid {kind} session_id UUID '{session_id}': {e}"
            ))
        })?;
        if schema_version == LEGACY_BUNDLE_SCHEMA_VERSION
            && let Some(id) = child_id
        {
            Uuid::parse_str(id).map_err(|e| {
                StorageError::Validation(format!("invalid {kind} UUID '{id}': {e}"))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_session_json(sid: &str, wid: &str) -> serde_json::Value {
        json!({
            "id": sid,
            "wearable_id": wid,
            "status": "closed",
            "start_ts_ms": 1_000_000,
            "end_ts_ms": 1_060_000,
            "end_reason": "stream_ended",
            "detection_sensitivity": "balanced",
            "had_rr": true,
            "warmup_mu_hr": 65.0,
            "warmup_sigma_hr": 2.0,
            "warmup_ln_rmssd": 3.5,
            "onset_candidate_ts_ms": null,
            "onset_confirmed_ts_ms": null,
            "sleeping_ts_ms": null,
            "sleep_mu_hr": null,
            "onset_hr_bpm": null,
            "delta_hr_onset": null,
            "sleep_ln_rmssd": null,
            "sleep_hr_var_ratio": null,
            "typical_sqi": null,
            "post_trigger_sleep_sec": 0,
            "trigger_power_action": "none",
            "rapid_resume_within_5m": false,
            "grace_cancel_count": 0,
            "rapid_wake_fp_count": 0,
            "snooze_count": 0,
            "auto_anomalous": false,
            "epochs_count": 1,
            "raw_samples_count": 1
        })
    }

    fn with_optional_id(mut obj: serde_json::Value, id: Option<&str>) -> serde_json::Value {
        if let Some(child_id) = id {
            obj.as_object_mut()
                .unwrap()
                .insert("id".to_string(), json!(child_id));
        }
        obj
    }

    fn sample_epoch_json(id: Option<&str>, sid: &str, seq: u32) -> serde_json::Value {
        with_optional_id(
            json!({
                "session_id": sid,
                "seq": seq,
                "ts_ms": 1_015_000,
                "hr_bpm_60s": 60.0,
                "z_hr": -1.2,
                "hr_slope_4m": -0.2,
                "r_var_120s": 0.4,
                "ln_rmssd_60s": 3.6,
                "delta_ln_rmssd_60s": 0.1,
                "rmssd_sdnn_120s": 0.9,
                "hrv_valid": true,
                "rr_artifact_frac": 0.0,
                "coverage": 1.0,
                "flatline_stale": false,
                "accumulators_frozen": false,
                "baseline_mu_hr": 65.0,
                "baseline_sigma_hr": 2.0,
                "baseline_frozen": false,
                "p_active_wake": 0.1,
                "p_quiet_wake": 0.3,
                "p_sleep": 0.6,
                "state": "monitoring"
            }),
            id,
        )
    }

    fn sample_raw_json(id: Option<&str>, sid: &str, seq: u32) -> serde_json::Value {
        with_optional_id(
            json!({
                "session_id": sid,
                "seq": seq,
                "ts_ms": 1_001_000,
                "bpm": 60.0,
                "rr_ms_json": "[1000.0]",
                "contact_status": "contact"
            }),
            id,
        )
    }

    #[test]
    fn v2_is_current_schema_and_omits_child_ids() {
        assert_eq!(CURRENT_BUNDLE_SCHEMA_VERSION, 2);

        let sid = "01950000-0000-7000-8000-000000000001";
        let wid = "01950000-0000-7000-8000-000000000002";
        let bundle_val = json!({
            "schema_version": 2,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "wearables": [],
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [sample_epoch_json(None, sid, 0)],
            "raw_samples": [sample_raw_json(None, sid, 0)],
            "session_feedback": [],
            "calibration_boundaries": [],
            "calibration_meta": null
        });

        let bundle: LedgerBundle = serde_json::from_value(bundle_val).expect("v2 deserializes");
        validate_bundle_payload(&bundle).expect("v2 validates without child id");

        let serialized = serde_json::to_value(&bundle).unwrap();
        assert!(serialized["session_epochs"][0].get("id").is_none());
        assert!(serialized["raw_samples"][0].get("id").is_none());
        assert_eq!(serialized["session_epochs"][0]["session_id"], sid);
        assert_eq!(serialized["raw_samples"][0]["session_id"], sid);
    }

    #[test]
    fn v1_validation_checks_present_child_uuids_and_rejects_unsupported_versions() {
        let sid = "01950000-0000-7000-8000-000000000001";
        let wid = "01950000-0000-7000-8000-000000000002";
        let child_uuid = "01950000-0000-7000-8000-000000000010";
        let raw_uuid = "01950000-0000-7000-8000-000000000020";

        let valid_v1: LedgerBundle = serde_json::from_value(json!({
            "schema_version": 1,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [sample_epoch_json(Some(child_uuid), sid, 0)],
            "raw_samples": [sample_raw_json(Some(raw_uuid), sid, 0)],
        }))
        .unwrap();
        validate_bundle_payload(&valid_v1).expect("valid v1 must pass");

        let v1_no_child_id: LedgerBundle = serde_json::from_value(json!({
            "schema_version": 1,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [sample_epoch_json(None, sid, 0)],
            "raw_samples": [sample_raw_json(None, sid, 0)],
        }))
        .unwrap();
        validate_bundle_payload(&v1_no_child_id)
            .expect("v1 without optional child id should still pass");

        let bad_epoch_v1: LedgerBundle = serde_json::from_value(json!({
            "schema_version": 1,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [sample_epoch_json(Some("not-a-uuid"), sid, 0)],
            "raw_samples": [],
        }))
        .unwrap();
        assert!(validate_bundle_payload(&bad_epoch_v1).is_err());

        let bad_raw_v1: LedgerBundle = serde_json::from_value(json!({
            "schema_version": 1,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [],
            "raw_samples": [sample_raw_json(Some("bad-raw-uuid"), sid, 0)],
        }))
        .unwrap();
        assert!(validate_bundle_payload(&bad_raw_v1).is_err());

        let bad_v2_sid: LedgerBundle = serde_json::from_value(json!({
            "schema_version": 2,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [sample_epoch_json(None, "bad-sid", 0)],
            "raw_samples": [],
        }))
        .unwrap();
        assert!(validate_bundle_payload(&bad_v2_sid).is_err());

        let v2_with_ignored_child_id: LedgerBundle = serde_json::from_value(json!({
            "schema_version": 2,
            "exported_at_ms": 2_000_000,
            "app_version": "0.1.0",
            "sessions": [sample_session_json(sid, wid)],
            "session_epochs": [sample_epoch_json(Some("ignored-on-v2"), sid, 0)],
            "raw_samples": [sample_raw_json(Some("ignored-on-v2"), sid, 0)],
        }))
        .unwrap();
        validate_bundle_payload(&v2_with_ignored_child_id)
            .expect("v2 should not enforce child id UUID validation");

        for bad_ver in [0_u32, 3_u32] {
            let unsupported: LedgerBundle = serde_json::from_value(json!({
                "schema_version": bad_ver,
                "exported_at_ms": 2_000_000,
                "app_version": "0.1.0",
            }))
            .unwrap();
            assert!(validate_bundle_payload(&unsupported).is_err());
        }
    }
}
