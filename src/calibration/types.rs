//! Pure domain types for the deterministic calibration ledger (ADR-0005, ADR-0008).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::{PowerAction, Sensitivity};
use crate::sleep_engine::{
    CalibrationMaturity, CalibrationPriors, Epoch, SessionEndReason, WearableFloor,
};

/// Current calibration algorithm version persisted alongside cached `calibration_state`.
pub const CALIBRATION_ALGORITHM_VERSION: u32 = 1;

/// User-supplied feedback verdict for a closed `Monitoring Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
#[value(rename_all = "snake_case")]
pub enum FeedbackVerdict {
    #[value(alias = "false-positive")]
    FalsePositive,
    #[value(alias = "missed-sleep")]
    MissedSleep,
    #[value(alias = "anomalous-exclude")]
    AnomalousExclude,
}

impl FeedbackVerdict {
    /// Returns the canonical lowercase snake_case representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FalsePositive => "false_positive",
            Self::MissedSleep => "missed_sleep",
            Self::AnomalousExclude => "anomalous_exclude",
        }
    }
}

impl fmt::Display for FeedbackVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FeedbackVerdict {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "false_positive" | "false-positive" => Ok(Self::FalsePositive),
            "missed_sleep" | "missed-sleep" => Ok(Self::MissedSleep),
            "anomalous_exclude" | "anomalous-exclude" => Ok(Self::AnomalousExclude),
            _ => Err(format!(
                "invalid feedback verdict '{s}': expected false_positive, missed_sleep, or anomalous_exclude"
            )),
        }
    }
}

/// Direction of a persistent multi-session baseline shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShiftDirection {
    Higher,
    Lower,
}

impl ShiftDirection {
    /// Returns the canonical lowercase snake_case representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Higher => "higher",
            Self::Lower => "lower",
        }
    }
}

impl fmt::Display for ShiftDirection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ShiftDirection {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "higher" => Ok(Self::Higher),
            "lower" => Ok(Self::Lower),
            _ => Err(format!(
                "invalid shift direction '{s}': expected higher or lower"
            )),
        }
    }
}

/// Pending 5-session consecutive same-direction persistent-shift prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistentShiftAlert {
    pub direction: ShiftDirection,
    pub session_count: u32,
    pub first_session_id: String,
    pub first_session_start_ts_ms: i64,
    pub baseline_mu_wake: f64,
    pub observed_mean_wake_hr: f64,
}

/// Origin reason for a reversible `Calibration Boundary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryReason {
    PersistentShift,
    ManualRebaseline,
}

impl BoundaryReason {
    /// Returns the canonical lowercase snake_case identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PersistentShift => "persistent_shift",
            Self::ManualRebaseline => "manual_rebaseline",
        }
    }
}

impl fmt::Display for BoundaryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BoundaryReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "persistent_shift" => Ok(Self::PersistentShift),
            "manual_rebaseline" => Ok(Self::ManualRebaseline),
            _ => Err(format!("invalid boundary reason '{s}'")),
        }
    }
}

/// Reversible `Calibration Boundary` marker in the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationBoundaryRecord {
    pub id: String,
    pub effective_ts_ms: i64,
    pub reason: BoundaryReason,
    pub seed_sleep_hr: Option<f64>,
    pub reset_floors: bool,
    pub created_at_ms: i64,
    pub reverted_at_ms: Option<i64>,
}

/// Persisted dismissal anchor so `Keep ignoring` silences the same shift streak.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissedShift {
    pub anchor_ts_ms: i64,
    pub direction: ShiftDirection,
}

/// Editable user feedback associated with one closed `Monitoring Session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionFeedbackRecord {
    pub id: String,
    pub session_id: String,
    pub verdict: Option<FeedbackVerdict>,
    pub manual_onset_ts_ms: Option<i64>,
    pub manual_avg_sleep_hr: Option<f64>,
    pub notes: Option<String>,
    pub updated_at_ms: i64,
}

/// Wearable seed metadata supplied to ledger recomputation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WearableSeedMeta {
    pub wearable_id: String,
    pub supports_rr: bool,
    pub supports_contact: bool,
}

/// Wearable-specific calibration parameters isolated per Wearable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WearableCalibration {
    pub wearable_id: String,
    pub hr_var_floor: f64,
    pub supports_rr: bool,
    pub supports_contact: bool,
    pub typical_sqi: Option<f64>,
    pub sessions_count: u32,
}

/// Immutable per-session summary persisted in the SQLite `sessions` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionLedgerRecord {
    pub id: String,
    pub wearable_id: String,
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub end_reason: SessionEndReason,
    pub detection_sensitivity: Sensitivity,
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
    pub trigger_power_action: PowerAction,
    pub rapid_resume_within_5m: bool,
    pub grace_cancel_count: u8,
    pub rapid_wake_fp_count: u8,
    pub snooze_count: u8,
    pub auto_anomalous: bool,
    pub epochs_count: u32,
    pub raw_samples_count: u32,
}

/// Single session bundle passed to pure-sync calibration recomputation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReplayInput {
    pub session: SessionLedgerRecord,
    pub feedback: Option<SessionFeedbackRecord>,
    pub epochs: Vec<Epoch>,
    pub wearable_supports_rr: bool,
    pub wearable_supports_contact: bool,
}

/// Complete ledger snapshot used to recompute [`CalibrationState`] chronologically from scratch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LedgerRecomputeInput {
    pub day1_seed_sleep_hr: Option<f64>,
    pub boundaries: Vec<CalibrationBoundaryRecord>,
    pub dismissed_shift: Option<DismissedShift>,
    pub wearables: Vec<WearableSeedMeta>,
    pub sessions: Vec<SessionReplayInput>,
}

/// Deterministic, timestamp-free calibration snapshot recomputed from the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationState {
    pub algorithm_version: u32,
    pub maturity: CalibrationMaturity,
    pub valid_sessions: u32,
    pub quarantined_sessions: u32,
    pub mu_wake: Option<f64>,
    pub sigma_wake: Option<f64>,
    pub wake_median: Option<f64>,
    pub wake_mad_s: Option<f64>,
    pub mu_sleep: Option<f64>,
    pub delta_hr_onset: Option<f64>,
    pub mu_ln_rmssd_sleep: Option<f64>,
    pub tau_p_sleep_offset: f64,
    pub dwell_adjust_sec: i32,
    pub day1_seed_sleep_hr: Option<f64>,
    pub active_boundary_id: Option<String>,
    pub active_boundary_ts_ms: Option<i64>,
    pub consecutive_anomaly_count: u32,
    pub consecutive_anomaly_direction: Option<ShiftDirection>,
    pub pending_shift_alert: Option<PersistentShiftAlert>,
    pub wearables: BTreeMap<String, WearableCalibration>,
}

impl Default for CalibrationState {
    fn default() -> Self {
        Self {
            algorithm_version: CALIBRATION_ALGORITHM_VERSION,
            maturity: CalibrationMaturity::ColdStart,
            valid_sessions: 0,
            quarantined_sessions: 0,
            mu_wake: None,
            sigma_wake: None,
            wake_median: None,
            wake_mad_s: None,
            mu_sleep: None,
            delta_hr_onset: None,
            mu_ln_rmssd_sleep: None,
            tau_p_sleep_offset: 0.0,
            dwell_adjust_sec: 0,
            day1_seed_sleep_hr: None,
            active_boundary_id: None,
            active_boundary_ts_ms: None,
            consecutive_anomaly_count: 0,
            consecutive_anomaly_direction: None,
            pending_shift_alert: None,
            wearables: BTreeMap::new(),
        }
    }
}

impl CalibrationState {
    /// Converts this calibration snapshot into [`CalibrationPriors`] consumed by [`crate::sleep_engine::SleepEngine`].
    pub fn to_priors(&self) -> CalibrationPriors {
        CalibrationPriors {
            maturity: self.maturity,
            mu_wake: self.mu_wake,
            wake_median: self.wake_median,
            sigma_wake: self.sigma_wake,
            mu_sleep: self.mu_sleep,
            delta_hr_onset: self.delta_hr_onset,
            mu_ln_rmssd_sleep: self.mu_ln_rmssd_sleep,
            tau_p_sleep_offset: self.tau_p_sleep_offset,
            dwell_adjust_sec: self.dwell_adjust_sec,
            wake_mad_s: self.wake_mad_s,
        }
    }

    /// Returns the isolated [`WearableFloor`] for `wearable_id`, falling back to the default floor.
    pub fn wearable_floor(&self, wearable_id: &str) -> WearableFloor {
        self.wearables
            .get(wearable_id)
            .map(|w| WearableFloor {
                hr_var_floor: w.hr_var_floor,
            })
            .unwrap_or_default()
    }
}

/// Validates a sleeping-HR override or Day-1 seed within physiological bounds `[30, 140]` bpm.
pub fn validate_sleep_hr_bpm(label: &str, hr: f64) -> Result<f64, String> {
    if !hr.is_finite() || !(30.0..=140.0).contains(&hr) {
        return Err(format!(
            "{label} must be finite and within [30, 140] bpm (got {hr})"
        ));
    }
    Ok(hr)
}
