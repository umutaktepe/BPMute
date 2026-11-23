//! Chronological Empirical Bayes -> capped-EWMA calibration recomputation (ADR-0005).

use std::collections::{BTreeMap, VecDeque};

use crate::calibration::types::{
    CALIBRATION_ALGORITHM_VERSION, CalibrationBoundaryRecord, CalibrationState, DismissedShift,
    FeedbackVerdict, LedgerRecomputeInput, PersistentShiftAlert, SessionLedgerRecord,
    SessionReplayInput, ShiftDirection, WearableCalibration, WearableSeedMeta,
};
use crate::config::PowerAction;
use crate::sleep_engine::ppg::{SIGMA_MIN_BPM, quantile, scaled_mad};
use crate::sleep_engine::{CalibrationMaturity, Epoch, WearableFloor};

pub const DAY1_SEED_WEIGHT: f64 = 1.5;
pub const NADIR_OFFSET_BPM: f64 = 2.5;
pub const LEARNING_MIN_SESSIONS: u32 = 3;
pub const CALIBRATED_MIN_SESSIONS: u32 = 7;
pub const MAD_QUARANTINE_MIN_SESSIONS: usize = 5;
pub const MAD_ROLLING_WINDOW: usize = 30;
pub const ANOMALY_SIGMA_THRESHOLD: f64 = 2.5;
pub const PERSISTENT_SHIFT_STREAK: u32 = 5;

pub const EB_KAPPA_0: f64 = 4.0;
pub const EWMA_ALPHA: f64 = 0.08;
pub const MANUAL_EWMA_ALPHA_CAP: f64 = 0.25;
pub const HUBER_DELTA_MULT: f64 = 2.0;

pub const WEIGHT_IMPLICIT_SLEEP: f64 = 0.5;
pub const WEIGHT_SUSPEND_POWEROFF: f64 = 0.35;
pub const WEIGHT_MANUAL_ONSET: f64 = 1.75;
pub const WEIGHT_MANUAL_AVG_SLEEP_HR: f64 = 2.0;
pub const MIN_IMPLICIT_SLEEP_SEC: u32 = 900;

pub const ETA_FP_TAU: f64 = 0.015;
pub const ETA_FP_DWELL_SEC: i32 = 30;
pub const ETA_RAPID_WAKE_TAU: f64 = 0.01125;
pub const ETA_RAPID_WAKE_DWELL_SEC: i32 = 20;
pub const ETA_TP_TAU: f64 = -0.003;
pub const ETA_MISSED_TAU: f64 = -0.010;
pub const ETA_MISSED_DWELL_SEC: i32 = -15;

const FLOOR_EB_KAPPA: f64 = 2.0;
const FLOOR_EWMA_ALPHA: f64 = 0.15;

const TAU_MIN: f64 = -0.03;
const TAU_MAX: f64 = 0.10;
const DWELL_MIN: i32 = -60;
const DWELL_MAX: i32 = 300;
const MU_WAKE_RANGE: (f64, f64) = (35.0, 140.0);
const MU_SLEEP_RANGE: (f64, f64) = (30.0, 130.0);
const SIGMA_WAKE_RANGE: (f64, f64) = (2.0, 15.0);
const DELTA_HR_RANGE: (f64, f64) = (-20.0, -1.5);
const LN_RMSSD_RANGE: (f64, f64) = (1.0, 6.0);

#[derive(Debug, Clone)]
struct ParamAccumulator {
    prior_kappa: f64,
    w_sum: f64,
    wx_sum: f64,
    current: Option<f64>,
    bounds: (f64, f64),
}

impl ParamAccumulator {
    fn new(prior_kappa: f64, bounds: (f64, f64)) -> Self {
        Self {
            prior_kappa,
            w_sum: 0.0,
            wx_sum: 0.0,
            current: None,
            bounds,
        }
    }

    fn seed(&mut self, value: f64, weight: f64) {
        let clamped = value.clamp(self.bounds.0, self.bounds.1);
        self.w_sum = weight;
        self.wx_sum = weight * clamped;
        self.current = Some(clamped);
    }

    fn update_eb(&mut self, value: f64, weight: f64, prior_center: f64) {
        if !value.is_finite() || weight <= 0.0 {
            return;
        }
        let clamped_val = value.clamp(self.bounds.0, self.bounds.1);
        self.w_sum += weight;
        self.wx_sum += weight * clamped_val;
        let denom = self.prior_kappa + self.w_sum;
        if denom > 0.0 {
            let est = (self.prior_kappa * prior_center + self.wx_sum) / denom;
            self.current = Some(est.clamp(self.bounds.0, self.bounds.1));
        }
    }

    fn update_ewma(&mut self, value: f64, weight: f64, huber_delta: f64) {
        if !value.is_finite() || weight <= 0.0 {
            return;
        }
        let obs = value.clamp(self.bounds.0, self.bounds.1);
        let Some(prev) = self.current else {
            self.seed(obs, weight);
            return;
        };
        let delta = huber_delta.max(1e-3);
        let innov = (obs - prev).clamp(-delta, delta);
        let clipped_obs = prev + innov;
        let ratio = weight / WEIGHT_IMPLICIT_SLEEP;
        let alpha = if weight <= WEIGHT_IMPLICIT_SLEEP {
            (EWMA_ALPHA * ratio).min(EWMA_ALPHA)
        } else {
            (EWMA_ALPHA * ratio).min(MANUAL_EWMA_ALPHA_CAP)
        };
        let next = prev + alpha * (clipped_obs - prev);
        self.current = Some(next.clamp(self.bounds.0, self.bounds.1));
    }
}

#[derive(Debug, Clone)]
struct WearableAccumulator {
    wearable_id: String,
    supports_rr: bool,
    supports_contact: bool,
    sessions_count: u32,
    floor_count: u32,
    floor_w_sum: f64,
    floor_wx_sum: f64,
    hr_var_floor: f64,
    sqi_count: u32,
    sqi_sum: f64,
    typical_sqi: Option<f64>,
}

impl WearableAccumulator {
    fn new(wearable_id: String, supports_rr: bool, supports_contact: bool) -> Self {
        Self {
            wearable_id,
            supports_rr,
            supports_contact,
            sessions_count: 0,
            floor_count: 0,
            floor_w_sum: 0.0,
            floor_wx_sum: 0.0,
            hr_var_floor: WearableFloor::DEFAULT_HR_VAR_FLOOR,
            sqi_count: 0,
            sqi_sum: 0.0,
            typical_sqi: None,
        }
    }

    fn observe(
        &mut self,
        session: &SessionLedgerRecord,
        supports_rr: bool,
        supports_contact: bool,
        manually_excluded: bool,
    ) {
        self.sessions_count += 1;
        self.supports_rr = self.supports_rr || supports_rr || session.had_rr;
        self.supports_contact = self.supports_contact || supports_contact;

        if let Some(sqi) = session.typical_sqi
            && sqi.is_finite()
        {
            let clamped = sqi.clamp(0.0, 1.0);
            self.sqi_count += 1;
            self.sqi_sum += clamped;
            self.typical_sqi = Some(self.sqi_sum / f64::from(self.sqi_count));
        }

        if !manually_excluded
            && let Some(obs_ratio) = session.sleep_hr_var_ratio
            && obs_ratio.is_finite()
        {
            let obs = obs_ratio.clamp(0.05, 0.80);
            self.floor_count += 1;
            if self.floor_count <= 6 {
                self.floor_w_sum += 1.0;
                self.floor_wx_sum += obs;
                let est = (FLOOR_EB_KAPPA * WearableFloor::DEFAULT_HR_VAR_FLOOR
                    + self.floor_wx_sum)
                    / (FLOOR_EB_KAPPA + self.floor_w_sum);
                self.hr_var_floor = est.clamp(0.05, 0.80);
            } else {
                let next = self.hr_var_floor + FLOOR_EWMA_ALPHA * (obs - self.hr_var_floor);
                self.hr_var_floor = next.clamp(0.05, 0.80);
            }
        }
    }

    fn to_calibration(&self) -> WearableCalibration {
        WearableCalibration {
            wearable_id: self.wearable_id.clone(),
            hr_var_floor: self.hr_var_floor,
            supports_rr: self.supports_rr,
            supports_contact: self.supports_contact,
            typical_sqi: self.typical_sqi,
            sessions_count: self.sessions_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ExtractedOnsetEvidence {
    pub(crate) sleep_hr: f64,
    pub(crate) delta_hr: Option<f64>,
    pub(crate) ln_rmssd: Option<f64>,
}

/// Incremental deterministic folder that matches scratch [`recompute_calibration`] bit-for-bit.
#[derive(Debug, Clone)]
pub struct CalibrationFolder {
    active_boundary_id: Option<String>,
    active_boundary_ts_ms: Option<i64>,
    floor_reset_ts_ms: Option<i64>,
    effective_seed_sleep_hr: Option<f64>,
    dismissed_shift: Option<DismissedShift>,

    valid_sessions: u32,
    quarantined_sessions: u32,

    mu_wake: ParamAccumulator,
    sigma_wake: ParamAccumulator,
    mu_sleep: ParamAccumulator,
    delta_hr_onset: ParamAccumulator,
    mu_ln_rmssd_sleep: ParamAccumulator,

    tau_p_sleep_offset: f64,
    dwell_adjust_sec: i32,

    clean_wake_hrs: VecDeque<f64>,
    consecutive_anomaly_count: u32,
    consecutive_anomaly_direction: Option<ShiftDirection>,
    streak_first_session_id: Option<String>,
    streak_first_start_ts_ms: Option<i64>,
    streak_wake_hrs: Vec<f64>,

    wearables: BTreeMap<String, WearableAccumulator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryFilter {
    AnyActive,
    ResetFloorsOnly,
}

impl CalibrationFolder {
    /// Initializes a fresh folder from optional Day-1 seed, boundaries, shift dismissal, and Wearables.
    pub fn new(
        day1_seed_sleep_hr: Option<f64>,
        boundaries: &[CalibrationBoundaryRecord],
        dismissed_shift: Option<DismissedShift>,
        wearables: &[WearableSeedMeta],
    ) -> Self {
        let active_boundary = select_latest_active_boundary(boundaries, BoundaryFilter::AnyActive);
        let floor_boundary =
            select_latest_active_boundary(boundaries, BoundaryFilter::ResetFloorsOnly);

        let effective_seed_sleep_hr = active_boundary
            .map(|b| b.seed_sleep_hr)
            .unwrap_or(day1_seed_sleep_hr);

        let mut mu_sleep = ParamAccumulator::new(0.0, MU_SLEEP_RANGE);
        if let Some(seed) = effective_seed_sleep_hr
            && seed.is_finite()
        {
            mu_sleep.seed(seed + NADIR_OFFSET_BPM, DAY1_SEED_WEIGHT);
        }

        let mut wearable_map = BTreeMap::new();
        for w in wearables {
            wearable_map.insert(
                w.wearable_id.clone(),
                WearableAccumulator::new(w.wearable_id.clone(), w.supports_rr, w.supports_contact),
            );
        }

        Self {
            active_boundary_id: active_boundary.map(|b| b.id.clone()),
            active_boundary_ts_ms: active_boundary.map(|b| b.effective_ts_ms),
            floor_reset_ts_ms: floor_boundary.map(|b| b.effective_ts_ms),
            effective_seed_sleep_hr,
            dismissed_shift,
            valid_sessions: 0,
            quarantined_sessions: 0,
            mu_wake: ParamAccumulator::new(0.0, MU_WAKE_RANGE),
            sigma_wake: ParamAccumulator::new(0.0, SIGMA_WAKE_RANGE),
            mu_sleep,
            delta_hr_onset: ParamAccumulator::new(EB_KAPPA_0, DELTA_HR_RANGE),
            mu_ln_rmssd_sleep: ParamAccumulator::new(0.0, LN_RMSSD_RANGE),
            tau_p_sleep_offset: 0.0,
            dwell_adjust_sec: 0,
            clean_wake_hrs: VecDeque::with_capacity(MAD_ROLLING_WINDOW),
            consecutive_anomaly_count: 0,
            consecutive_anomaly_direction: None,
            streak_first_session_id: None,
            streak_first_start_ts_ms: None,
            streak_wake_hrs: Vec::new(),
            wearables: wearable_map,
        }
    }

    /// Folds one chronological `Monitoring Session` into the calibration state.
    pub fn fold_session(&mut self, input: &SessionReplayInput) {
        let session = &input.session;
        let feedback = input.feedback.as_ref();
        let verdict = feedback.and_then(|fb| fb.verdict);
        let manually_excluded = verdict == Some(FeedbackVerdict::AnomalousExclude);

        // 1. Per-Wearable calibration stays isolated per Wearable and survives normal boundaries.
        let include_for_wearable = self
            .floor_reset_ts_ms
            .is_none_or(|reset_ts| session.start_ts_ms >= reset_ts);
        if include_for_wearable {
            let accumulator = self
                .wearables
                .entry(session.wearable_id.clone())
                .or_insert_with(|| {
                    WearableAccumulator::new(
                        session.wearable_id.clone(),
                        input.wearable_supports_rr,
                        input.wearable_supports_contact,
                    )
                });
            accumulator.observe(
                session,
                input.wearable_supports_rr,
                input.wearable_supports_contact,
                manually_excluded,
            );
        }

        // 2. Personal physiological parameters fold only sessions at or after the active boundary.
        if let Some(boundary_ts) = self.active_boundary_ts_ms
            && session.start_ts_ms < boundary_ts
        {
            return;
        }

        // 3. Manual AnomalousExclude quarantines the session with zero drift.
        if manually_excluded {
            self.quarantined_sessions += 1;
            self.reset_shift_streak();
            return;
        }

        // 4. Automatic MAD anomaly quarantine activates at 5 clean sessions.
        let wake_ref = session.wake_ref();
        if let Some((median, mad_s)) = self.current_wake_mad()
            && let Some(wake_mu) = wake_ref
            && wake_mu.is_finite()
        {
            let diff = wake_mu - median;
            if diff.abs() > ANOMALY_SIGMA_THRESHOLD * mad_s {
                self.quarantined_sessions += 1;
                let dir = if diff > 0.0 {
                    ShiftDirection::Higher
                } else {
                    ShiftDirection::Lower
                };
                self.record_shift_streak(session, dir, wake_mu);
                return;
            }
        }

        self.reset_shift_streak();

        // 5. False-positive and missed-sleep threshold adjustments (capped at 1 FP per session).
        let is_fp = verdict == Some(FeedbackVerdict::FalsePositive)
            || session.grace_cancel_count > 0
            || session.rapid_wake_fp_count > 0
            || session.rapid_resume_within_5m;

        if is_fp {
            let strong_fp =
                verdict == Some(FeedbackVerdict::FalsePositive) || session.grace_cancel_count > 0;
            let (d_tau, d_dwell) = if strong_fp {
                (ETA_FP_TAU, ETA_FP_DWELL_SEC)
            } else {
                (ETA_RAPID_WAKE_TAU, ETA_RAPID_WAKE_DWELL_SEC)
            };
            self.adjust_threshold(d_tau, d_dwell);
        } else if verdict == Some(FeedbackVerdict::MissedSleep) {
            self.adjust_threshold(ETA_MISSED_TAU, ETA_MISSED_DWELL_SEC);
        }

        // 6. Combine manual ground truth and implicit physiological observations.
        let manual_avg_sleep_hr = feedback
            .and_then(|fb| fb.manual_avg_sleep_hr)
            .filter(|v| v.is_finite());
        let manual_onset_ts_ms = feedback.and_then(|fb| fb.manual_onset_ts_ms);
        let onset_evidence = manual_onset_ts_ms.and_then(|ts| {
            extract_onset_evidence(
                ts,
                session.pre_candidate_mu_hr,
                session.warmup_mu_hr,
                &input.epochs,
            )
        });

        if let Some(onset_ts) = manual_onset_ts_ms {
            apply_manual_onset_threshold_nudge(
                &mut self.tau_p_sleep_offset,
                onset_ts,
                session.onset_confirmed_ts_ms,
            );
        }

        let has_implicit_sleep = !is_fp && session.post_trigger_sleep_sec >= MIN_IMPLICIT_SLEEP_SEC;
        let has_suspend_poweroff = !is_fp
            && !has_implicit_sleep
            && matches!(
                session.trigger_power_action,
                PowerAction::Suspend | PowerAction::Poweroff
            )
            && session.sleeping_ts_ms.is_some();
        let has_manual_sleep = manual_avg_sleep_hr.is_some() || manual_onset_ts_ms.is_some();

        if !(has_implicit_sleep || has_suspend_poweroff || has_manual_sleep) {
            return;
        }

        let wake_weight = if has_implicit_sleep || has_manual_sleep {
            WEIGHT_IMPLICIT_SLEEP
        } else {
            WEIGHT_SUSPEND_POWEROFF
        };

        let sleep_mu_obs = if let Some(avg_hr) = manual_avg_sleep_hr {
            Some((avg_hr + NADIR_OFFSET_BPM, WEIGHT_MANUAL_AVG_SLEEP_HR))
        } else if let Some(ref ev) = onset_evidence {
            Some((ev.sleep_hr, WEIGHT_MANUAL_ONSET))
        } else if has_implicit_sleep {
            session
                .sleep_mu_hr
                .filter(|v| v.is_finite())
                .map(|s| (s, WEIGHT_IMPLICIT_SLEEP))
        } else {
            None
        };

        let delta_hr_obs = if let Some(ref ev) = onset_evidence {
            ev.delta_hr.map(|d| (d, WEIGHT_MANUAL_ONSET))
        } else if let (Some(avg_hr), Some(wake_hr)) = (manual_avg_sleep_hr, wake_ref) {
            Some((
                (avg_hr + NADIR_OFFSET_BPM) - wake_hr,
                WEIGHT_MANUAL_AVG_SLEEP_HR,
            ))
        } else if has_implicit_sleep {
            resolve_implicit_delta_hr(session).map(|d| (d, WEIGHT_IMPLICIT_SLEEP))
        } else if has_suspend_poweroff {
            resolve_implicit_delta_hr(session).map(|d| (d, WEIGHT_SUSPEND_POWEROFF))
        } else {
            None
        };

        let ln_rmssd_obs = if let Some(ref ev) = onset_evidence {
            ev.ln_rmssd.map(|r| (r, WEIGHT_MANUAL_ONSET))
        } else if has_implicit_sleep {
            session
                .sleep_ln_rmssd
                .filter(|v| v.is_finite())
                .map(|r| (r, WEIGHT_IMPLICIT_SLEEP))
        } else {
            None
        };

        if has_implicit_sleep && !has_manual_sleep && self.tau_p_sleep_offset > 0.0 {
            self.tau_p_sleep_offset = (self.tau_p_sleep_offset + ETA_TP_TAU).max(0.0);
        }

        let use_ewma = self.valid_sessions >= CALIBRATED_MIN_SESSIONS - 1;
        let scale_s = self
            .current_wake_mad()
            .map(|(_, s)| s)
            .unwrap_or(SIGMA_MIN_BPM);
        let huber_hr_delta = HUBER_DELTA_MULT * scale_s;
        let wake_sigma_ref = session.pre_candidate_sigma_hr.or(session.warmup_sigma_hr);

        if use_ewma {
            if let Some(w_hr) = wake_ref.filter(|v| v.is_finite()) {
                self.mu_wake.update_ewma(w_hr, wake_weight, huber_hr_delta);
            }
            if let Some(s_hr) = wake_sigma_ref.filter(|v| v.is_finite()) {
                self.sigma_wake.update_ewma(s_hr, wake_weight, 3.0);
            }
            if let Some((s_mu, w)) = sleep_mu_obs {
                self.mu_sleep.update_ewma(s_mu, w, huber_hr_delta);
            }
            if let Some((d_hr, w)) = delta_hr_obs {
                self.delta_hr_onset.update_ewma(d_hr, w, huber_hr_delta);
            }
            if let Some((r_val, w)) = ln_rmssd_obs {
                self.mu_ln_rmssd_sleep.update_ewma(r_val, w, 0.70);
            }
        } else {
            if let Some(w_hr) = wake_ref.filter(|v| v.is_finite()) {
                self.mu_wake.update_eb(w_hr, wake_weight, w_hr);
            }
            if let Some(s_hr) = wake_sigma_ref.filter(|v| v.is_finite()) {
                self.sigma_wake.update_eb(s_hr, wake_weight, s_hr);
            }
            if let Some((s_mu, w)) = sleep_mu_obs {
                self.mu_sleep.update_eb(s_mu, w, s_mu);
            }
            if let Some((d_hr, w)) = delta_hr_obs {
                let prior_delta = self
                    .mu_wake
                    .current
                    .map(|wake| (-0.08 * wake).clamp(DELTA_HR_RANGE.0, DELTA_HR_RANGE.1))
                    .unwrap_or(-5.5);
                self.delta_hr_onset.update_eb(d_hr, w, prior_delta);
            }
            if let Some((r_val, w)) = ln_rmssd_obs {
                self.mu_ln_rmssd_sleep.update_eb(r_val, w, r_val);
            }
        }

        if let Some(w_hr) = wake_ref.filter(|v| v.is_finite()) {
            if self.clean_wake_hrs.len() == MAD_ROLLING_WINDOW {
                self.clean_wake_hrs.pop_front();
            }
            self.clean_wake_hrs.push_back(w_hr);
        }

        self.valid_sessions += 1;
    }

    /// Returns the resulting deterministic [`CalibrationState`].
    pub fn finish(&self) -> CalibrationState {
        let maturity = maturity_for_valid_sessions(self.valid_sessions);
        let (wake_median, wake_mad_s) = match self.current_wake_mad() {
            Some((med, mad_s)) => (Some(med), Some(mad_s)),
            None => (None, None),
        };

        let pending_shift_alert = self.build_pending_shift_alert();

        let wearables = self
            .wearables
            .iter()
            .map(|(k, acc)| (k.clone(), acc.to_calibration()))
            .collect();

        CalibrationState {
            algorithm_version: CALIBRATION_ALGORITHM_VERSION,
            maturity,
            valid_sessions: self.valid_sessions,
            quarantined_sessions: self.quarantined_sessions,
            mu_wake: self.mu_wake.current,
            sigma_wake: self.sigma_wake.current,
            wake_median,
            wake_mad_s,
            mu_sleep: self.mu_sleep.current,
            delta_hr_onset: self.delta_hr_onset.current,
            mu_ln_rmssd_sleep: self.mu_ln_rmssd_sleep.current,
            tau_p_sleep_offset: self.tau_p_sleep_offset,
            dwell_adjust_sec: self.dwell_adjust_sec,
            day1_seed_sleep_hr: self.effective_seed_sleep_hr,
            active_boundary_id: self.active_boundary_id.clone(),
            active_boundary_ts_ms: self.active_boundary_ts_ms,
            consecutive_anomaly_count: self.consecutive_anomaly_count,
            consecutive_anomaly_direction: self.consecutive_anomaly_direction,
            pending_shift_alert,
            wearables,
        }
    }

    fn current_wake_mad(&self) -> Option<(f64, f64)> {
        if self.clean_wake_hrs.len() < MAD_QUARANTINE_MIN_SESSIONS {
            return None;
        }
        let values: Vec<f64> = self.clean_wake_hrs.iter().copied().collect();
        let median = quantile(&values, 0.5)?;
        let mad_s = scaled_mad(&values)
            .unwrap_or(SIGMA_MIN_BPM)
            .max(SIGMA_MIN_BPM);
        Some((median, mad_s))
    }

    fn adjust_threshold(&mut self, delta_tau: f64, delta_dwell: i32) {
        self.tau_p_sleep_offset = (self.tau_p_sleep_offset + delta_tau).clamp(TAU_MIN, TAU_MAX);
        self.dwell_adjust_sec = (self.dwell_adjust_sec + delta_dwell).clamp(DWELL_MIN, DWELL_MAX);
    }

    fn reset_shift_streak(&mut self) {
        self.consecutive_anomaly_count = 0;
        self.consecutive_anomaly_direction = None;
        self.streak_first_session_id = None;
        self.streak_first_start_ts_ms = None;
        self.streak_wake_hrs.clear();
    }

    fn record_shift_streak(
        &mut self,
        session: &SessionLedgerRecord,
        dir: ShiftDirection,
        warmup_mu: f64,
    ) {
        if self.consecutive_anomaly_direction == Some(dir) {
            self.consecutive_anomaly_count += 1;
            self.streak_wake_hrs.push(warmup_mu);
        } else {
            self.consecutive_anomaly_direction = Some(dir);
            self.consecutive_anomaly_count = 1;
            self.streak_first_session_id = Some(session.id.clone());
            self.streak_first_start_ts_ms = Some(session.start_ts_ms);
            self.streak_wake_hrs.clear();
            self.streak_wake_hrs.push(warmup_mu);
        }
    }

    fn build_pending_shift_alert(&self) -> Option<PersistentShiftAlert> {
        if self.consecutive_anomaly_count < PERSISTENT_SHIFT_STREAK {
            return None;
        }
        let direction = self.consecutive_anomaly_direction?;
        let first_session_id = self.streak_first_session_id.clone()?;
        let first_session_start_ts_ms = self.streak_first_start_ts_ms?;
        let baseline_mu_wake = self.mu_wake.current?;
        if self.dismissed_shift.is_some_and(|d| {
            d.anchor_ts_ms == first_session_start_ts_ms && d.direction == direction
        }) {
            return None;
        }
        let observed_mean_wake_hr = if self.streak_wake_hrs.is_empty() {
            baseline_mu_wake
        } else {
            self.streak_wake_hrs.iter().sum::<f64>() / self.streak_wake_hrs.len() as f64
        };
        Some(PersistentShiftAlert {
            direction,
            session_count: self.consecutive_anomaly_count,
            first_session_id,
            first_session_start_ts_ms,
            baseline_mu_wake,
            observed_mean_wake_hr,
        })
    }
}

/// Recomputes [`CalibrationState`] deterministically from scratch across the entire ledger.
pub fn recompute_calibration(input: &LedgerRecomputeInput) -> CalibrationState {
    let mut sorted_sessions: Vec<&SessionReplayInput> = input.sessions.iter().collect();
    sorted_sessions.sort_by(|a, b| {
        a.session
            .start_ts_ms
            .cmp(&b.session.start_ts_ms)
            .then_with(|| a.session.id.cmp(&b.session.id))
    });

    let mut folder = CalibrationFolder::new(
        input.day1_seed_sleep_hr,
        &input.boundaries,
        input.dismissed_shift,
        &input.wearables,
    );
    for session in sorted_sessions {
        folder.fold_session(session);
    }
    folder.finish()
}

fn maturity_for_valid_sessions(valid_sessions: u32) -> CalibrationMaturity {
    if valid_sessions >= CALIBRATED_MIN_SESSIONS {
        CalibrationMaturity::Calibrated
    } else if valid_sessions >= LEARNING_MIN_SESSIONS {
        CalibrationMaturity::Learning
    } else {
        CalibrationMaturity::ColdStart
    }
}

fn select_latest_active_boundary(
    boundaries: &[CalibrationBoundaryRecord],
    filter: BoundaryFilter,
) -> Option<&CalibrationBoundaryRecord> {
    boundaries
        .iter()
        .filter(|b| {
            b.reverted_at_ms.is_none()
                && match filter {
                    BoundaryFilter::AnyActive => true,
                    BoundaryFilter::ResetFloorsOnly => b.reset_floors,
                }
        })
        .max_by(|a, b| {
            a.effective_ts_ms
                .cmp(&b.effective_ts_ms)
                .then_with(|| a.created_at_ms.cmp(&b.created_at_ms))
                .then_with(|| a.id.cmp(&b.id))
        })
}

fn resolve_implicit_delta_hr(session: &SessionLedgerRecord) -> Option<f64> {
    session.resolve_delta_hr()
}

pub(crate) fn extract_onset_evidence(
    onset_ts_ms: i64,
    pre_candidate_mu_hr: Option<f64>,
    warmup_mu_hr: Option<f64>,
    epochs: &[Epoch],
) -> Option<ExtractedOnsetEvidence> {
    if epochs.is_empty() {
        return None;
    }

    let mut window: Vec<&Epoch> = epochs
        .iter()
        .filter(|ep| {
            !ep.accumulators_frozen && ep.ts_ms >= onset_ts_ms && ep.ts_ms <= onset_ts_ms + 900_000
        })
        .collect();
    if window.is_empty() {
        window = epochs
            .iter()
            .filter(|ep| {
                !ep.accumulators_frozen
                    && ep.ts_ms >= onset_ts_ms - 120_000
                    && ep.ts_ms <= onset_ts_ms + 900_000
            })
            .collect();
    }
    if window.is_empty() {
        return None;
    }

    let hrs: Vec<f64> = window
        .iter()
        .map(|ep| ep.features.hr_bpm_60s)
        .filter(|v| v.is_finite())
        .collect();
    let sleep_hr = quantile(&hrs, 0.5)?;

    let wake_ref = pre_candidate_mu_hr.or(warmup_mu_hr).or_else(|| {
        let pre_baselines: Vec<f64> = epochs
            .iter()
            .filter(|ep| ep.ts_ms <= onset_ts_ms)
            .map(|ep| ep.baseline_mu_hr)
            .filter(|v| v.is_finite())
            .collect();
        quantile(&pre_baselines, 0.5)
    });
    let delta_hr = wake_ref.map(|wake| sleep_hr - wake);

    let rmssds: Vec<f64> = window
        .iter()
        .filter(|ep| ep.hrv_valid)
        .filter_map(|ep| ep.features.ln_rmssd_60s)
        .filter(|v| v.is_finite())
        .collect();
    let ln_rmssd = quantile(&rmssds, 0.5);

    Some(ExtractedOnsetEvidence {
        sleep_hr,
        delta_hr,
        ln_rmssd,
    })
}

fn apply_manual_onset_threshold_nudge(
    tau_p_sleep_offset: &mut f64,
    manual_onset_ts_ms: i64,
    detected_confirmed_ts_ms: Option<i64>,
) {
    const TEN_MIN_MS: i64 = 600_000;
    let nudge = match detected_confirmed_ts_ms {
        None => -0.005,
        Some(detected) if detected - manual_onset_ts_ms > TEN_MIN_MS => -0.005,
        Some(detected) if manual_onset_ts_ms - detected > TEN_MIN_MS => 0.005,
        Some(_) => 0.0,
    };
    *tau_p_sleep_offset = (*tau_p_sleep_offset + nudge).clamp(TAU_MIN, TAU_MAX);
}

#[cfg(test)]
mod tests {
    use crate::calibration::types::BoundaryReason;

    #[test]
    fn boundary_reason_round_trip() {
        for r in [
            BoundaryReason::PersistentShift,
            BoundaryReason::ManualRebaseline,
        ] {
            assert_eq!(r.as_str().parse::<BoundaryReason>().unwrap(), r);
        }
    }

    #[test]
    fn extract_onset_evidence_prioritizes_pre_candidate_mu_hr() {
        use crate::sleep_engine::{DetectorState, Epoch, EpochFeatures};

        let onset_ts = 1_000_000;
        let ep = Epoch {
            session_index: 1,
            ts_ms: onset_ts + 60_000,
            features: EpochFeatures {
                hr_bpm_60s: 56.0,
                z_hr: -1.5,
                hr_slope_4m: -0.2,
                r_var_120s: 0.4,
                ln_rmssd_60s: Some(3.9),
                delta_ln_rmssd_60s: Some(0.2),
                rmssd_sdnn_120s: Some(1.0),
            },
            baseline_mu_hr: 70.0,
            baseline_sigma_hr: 2.2,
            baseline_frozen: true,
            coverage: 1.0,
            hrv_valid: true,
            rr_artifact_frac: 0.01,
            flatline_stale: false,
            accumulators_frozen: false,
            p_active_wake: 0.05,
            p_quiet_wake: 0.25,
            p_sleep: 0.70,
            state: DetectorState::Monitoring,
        };

        let evidence = super::extract_onset_evidence(onset_ts, Some(64.0), Some(80.0), &[ep])
            .expect("evidence must be extracted");

        assert_eq!(evidence.sleep_hr, 56.0);
        assert_eq!(evidence.delta_hr, Some(-8.0));
    }
}
