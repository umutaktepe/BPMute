//! Pure domain types for the deterministic calibration ledger (ADR-0005, ADR-0008).

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use chrono::{Duration, Local, LocalResult, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};

use crate::config::{ClockTime, PowerAction, Sensitivity};
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
    pub trigger_power_action: PowerAction,
    pub rapid_resume_within_5m: bool,
    pub grace_cancel_count: u8,
    pub rapid_wake_fp_count: u8,
    pub snooze_count: u8,
    pub auto_anomalous: bool,
    pub epochs_count: u32,
    pub raw_samples_count: u32,
}

impl SessionLedgerRecord {
    /// Resolves the waking reference heart rate, prioritizing pre-candidate quiet wake over warmup.
    pub fn wake_ref(&self) -> Option<f64> {
        self.pre_candidate_mu_hr.or(self.warmup_mu_hr)
    }

    /// Resolves the deceleration of sleep onset relative to the waking baseline.
    pub fn resolve_delta_hr(&self) -> Option<f64> {
        self.delta_hr_onset.filter(|v| v.is_finite()).or_else(|| {
            self.sleep_mu_hr
                .or(self.onset_hr_bpm)
                .zip(self.wake_ref())
                .map(|(sleep_or_onset, wake)| sleep_or_onset - wake)
        })
    }
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

/// Parses an optional sleeping-HR text field, validating within `[30, 140]` BPM.
pub fn parse_optional_sleep_hr_input(raw: &str, label: &str) -> Result<Option<f64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let hr = trimmed
        .parse::<f64>()
        .map_err(|_| format!("{label} must be a valid number"))?;
    validate_sleep_hr_bpm(label, hr)
        .map(Some)
        .map_err(|_| format!("{label} must be between 30 and 140 BPM"))
}

/// Resolves a local `HH:MM` onset clock time onto a timestamp within `[start_ts_ms, end_ts_ms]`.
pub fn resolve_onset_hhmm_ms(
    start_ts_ms: i64,
    end_ts_ms: i64,
    onset: ClockTime,
) -> Result<i64, String> {
    resolve_onset_hhmm_ms_in_tz(&Local, start_ts_ms, end_ts_ms, onset)
}

/// Timezone-parameterized onset resolver handling midnight crossings and DST ambiguity.
pub fn resolve_onset_hhmm_ms_in_tz<Tz: TimeZone>(
    tz: &Tz,
    start_ts_ms: i64,
    end_ts_ms: i64,
    onset: ClockTime,
) -> Result<i64, String> {
    let start_local = match tz.timestamp_millis_opt(start_ts_ms) {
        LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => dt,
        LocalResult::None => {
            return Err("session start timestamp out of local range".to_string());
        }
    };
    let end_local = match tz.timestamp_millis_opt(end_ts_ms) {
        LocalResult::Single(dt) | LocalResult::Ambiguous(dt, _) => dt,
        LocalResult::None => start_local.clone() + Duration::hours(12),
    };

    let target_time = onset.to_naive_time();
    let mut date = start_local
        .date_naive()
        .pred_opt()
        .unwrap_or_else(|| start_local.date_naive());
    let limit_date = end_local
        .date_naive()
        .succ_opt()
        .unwrap_or_else(|| end_local.date_naive());

    while date <= limit_date {
        let naive = NaiveDateTime::new(date, target_time);
        let candidate_dt = match tz.from_local_datetime(&naive) {
            LocalResult::Single(dt) => Some(dt),
            LocalResult::Ambiguous(earliest, latest) => {
                let e_ms = earliest.timestamp_millis();
                if e_ms >= start_ts_ms && e_ms <= end_ts_ms {
                    Some(earliest)
                } else {
                    Some(latest)
                }
            }
            LocalResult::None => None,
        };
        if let Some(dt) = candidate_dt {
            let ts = dt.timestamp_millis();
            if ts >= start_ts_ms && ts <= end_ts_ms {
                return Ok(ts);
            }
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }

    Err(format!(
        "manual onset time {onset} falls outside session bounds"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};

    #[derive(Clone, Copy)]
    struct FallBackDstTz;

    impl TimeZone for FallBackDstTz {
        type Offset = FixedOffset;

        fn from_offset(_offset: &Self::Offset) -> Self {
            Self
        }

        fn offset_from_local_date(&self, _local: &NaiveDate) -> LocalResult<Self::Offset> {
            LocalResult::Single(FixedOffset::east_opt(3600).unwrap())
        }

        fn offset_from_local_datetime(&self, local: &NaiveDateTime) -> LocalResult<Self::Offset> {
            let amb_start = NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(1, 0, 0)
                .unwrap();
            let amb_end = NaiveDate::from_ymd_opt(2026, 11, 1)
                .unwrap()
                .and_hms_opt(2, 0, 0)
                .unwrap();
            let edt = FixedOffset::east_opt(2 * 3600).unwrap();
            let est = FixedOffset::east_opt(3600).unwrap();
            if *local >= amb_start && *local < amb_end {
                LocalResult::Ambiguous(edt, est)
            } else if *local < amb_start {
                LocalResult::Single(edt)
            } else {
                LocalResult::Single(est)
            }
        }

        fn offset_from_utc_date(&self, _utc: &NaiveDate) -> Self::Offset {
            FixedOffset::east_opt(3600).unwrap()
        }

        fn offset_from_utc_datetime(&self, utc: &NaiveDateTime) -> Self::Offset {
            let switch_utc = NaiveDate::from_ymd_opt(2026, 10, 31)
                .unwrap()
                .and_hms_opt(23, 0, 0)
                .unwrap();
            if *utc < switch_utc {
                FixedOffset::east_opt(2 * 3600).unwrap()
            } else {
                FixedOffset::east_opt(3600).unwrap()
            }
        }
    }

    #[test]
    fn resolve_onset_hhmm_handles_ambiguous_fallback_dst_hour() {
        let tz = FallBackDstTz;
        // Session starts during second 01:15 (UTC+1) and ends at 04:00 (UTC+1).
        let start_dt = tz
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2026, 11, 1)
                    .unwrap()
                    .and_hms_opt(1, 15, 0)
                    .unwrap(),
            )
            .latest()
            .unwrap();
        let end_dt = tz
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(2026, 11, 1)
                    .unwrap()
                    .and_hms_opt(4, 0, 0)
                    .unwrap(),
            )
            .single()
            .unwrap();

        // 01:30 occurs twice; earliest is before session start, so latest (second 01:30) is selected.
        let resolved = resolve_onset_hhmm_ms_in_tz(
            &tz,
            start_dt.timestamp_millis(),
            end_dt.timestamp_millis(),
            ClockTime::new(1, 30).unwrap(),
        )
        .expect("second ambiguous 01:30 inside session bounds must resolve");
        assert!(resolved >= start_dt.timestamp_millis() && resolved <= end_dt.timestamp_millis());
    }
}
