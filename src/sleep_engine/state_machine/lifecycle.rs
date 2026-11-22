//! Session lifecycle, gap resolution, warmup calibration, and grace-period handling.

use crate::config::DetectionConfig;
use crate::sleep_engine::baseline::QuietWakeBaseline;
use crate::sleep_engine::hmm::STATE_SLEEP_NREM;
use crate::sleep_engine::ppg::{PpgPipeline, SIGMA_MIN_BPM};
use crate::sleep_engine::state_machine::{
    ActiveGap, CANCEL_COOLDOWN_MS, DEFAULT_EXPECTED_Z_SLEEP, MEDIUM_GAP_STABILIZATION_EPOCHS,
    SHORT_GAP_MAX_MS, SleepEngine, WARMUP_EPOCHS_CALIBRATED, WARMUP_EPOCHS_COLD_OR_LEARNING,
};
use crate::sleep_engine::types::{
    CalibrationMaturity, CalibrationPriors, DetectorState, EngineOutput, GapTier, GraceResult,
    PresetParams, SessionEndReason, SessionSummary, WearableFloor,
};

impl SleepEngine {
    /// Updates detection preset/advanced config live while preserving active session state.
    pub fn update_detection_config(&mut self, detection: DetectionConfig) {
        self.config.detection = detection;
        self.preset = PresetParams::resolve(&self.config.detection, &self.config.priors);
    }

    /// Updates personal calibration priors and per-Wearable variance floor live.
    pub fn update_calibration(&mut self, priors: CalibrationPriors, wearable_floor: WearableFloor) {
        self.config.priors = priors;
        self.config.wearable_floor = wearable_floor;
        self.preset = PresetParams::resolve(&self.config.detection, &self.config.priors);
    }

    /// Force-closes any active `Monitoring Session` (for daemon shutdown or Wearable switches).
    pub fn force_close_session(
        &mut self,
        ts_ms: i64,
        reason: SessionEndReason,
    ) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        if self.state != DetectorState::Idle {
            self.close_session(ts_ms, reason, &mut outputs);
        }
        outputs
    }

    pub(super) fn begin_or_tighten_gap(&mut self, ts_ms: i64, window_ms: i64) {
        if self.state == DetectorState::Idle {
            return;
        }
        let candidate_deadline = ts_ms.saturating_add(window_ms).saturating_add(1);
        match &mut self.active_gap {
            Some(gap) => {
                gap.deadline_ms = gap.deadline_ms.min(candidate_deadline);
            }
            None => {
                self.active_gap = Some(ActiveGap {
                    started_ms: ts_ms,
                    deadline_ms: candidate_deadline,
                });
            }
        }
    }

    pub(super) fn resolve_active_gap(
        &mut self,
        ts_ms: i64,
        reopen_on_close: bool,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let Some(gap) = self.active_gap.take() else {
            return;
        };
        let duration_ms = (ts_ms - gap.started_ms).max(0);
        let session_idx = self.session_index.unwrap_or(1);

        if ts_ms >= gap.deadline_ms {
            outputs.push(EngineOutput::GapResolved {
                session_index: session_idx,
                ts_ms,
                tier: GapTier::LongSessionClose,
                duration_ms,
            });
            self.close_session(gap.started_ms, SessionEndReason::GapExceeded, outputs);
            if reopen_on_close {
                self.open_session(ts_ms, "stream resumed after gap exceeded", outputs);
            }
        } else if duration_ms <= SHORT_GAP_MAX_MS {
            self.ppg.on_short_gap_resume(ts_ms);
            self.last_any_packet_ts_ms = Some(ts_ms);
            self.last_valid_packet_ts_ms = Some(ts_ms);
            outputs.push(EngineOutput::GapResolved {
                session_index: session_idx,
                ts_ms,
                tier: GapTier::ShortPause,
                duration_ms,
            });
        } else {
            self.ppg.clear_rolling_windows(ts_ms);
            self.last_any_packet_ts_ms = Some(ts_ms);
            self.last_valid_packet_ts_ms = Some(ts_ms);
            if self.state == DetectorState::Candidate {
                self.baseline.set_frozen(false);
                let p_sleep = self.hmm.probs()[STATE_SLEEP_NREM];
                self.return_to_monitoring(
                    ts_ms,
                    true,
                    p_sleep,
                    "2-15 min gap candidate reset",
                    outputs,
                );
            }
            self.stabilization_remaining_epochs = MEDIUM_GAP_STABILIZATION_EPOCHS;
            outputs.push(EngineOutput::GapResolved {
                session_index: session_idx,
                ts_ms,
                tier: GapTier::MediumStabilize,
                duration_ms,
            });
        }
    }

    pub(super) fn handle_grace_outcome(
        &mut self,
        ts_ms: i64,
        outcome: GraceResult,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let p_sleep = self.hmm.probs()[STATE_SLEEP_NREM];
        match outcome {
            GraceResult::Expired => {
                if self.state == DetectorState::Confirmed {
                    self.sleeping_ts_ms = Some(ts_ms);
                    self.sleeping_hr_anchor = self.last_hr_bpm_60s;
                    self.post_sleep_wake_sec = 0;
                    self.last_sleeping_desktop_activity_ms = None;
                    self.transition_to(
                        ts_ms,
                        DetectorState::Sleeping,
                        p_sleep,
                        "grace period expired",
                        outputs,
                    );
                }
            }
            GraceResult::Cancelled => {
                if self.state == DetectorState::Confirmed {
                    self.grace_cancel_count = self.grace_cancel_count.saturating_add(1);
                    self.apply_false_positive_cooldown(ts_ms, outputs);
                    self.return_to_monitoring(
                        ts_ms,
                        true,
                        p_sleep,
                        "grace period cancelled",
                        outputs,
                    );
                }
            }
            GraceResult::Snoozed { duration_sec } => {
                if self.state == DetectorState::Confirmed {
                    self.snooze_count = self.snooze_count.saturating_add(1);
                    let until = ts_ms.saturating_add(i64::from(duration_sec).saturating_mul(1000));
                    self.suppress_candidate_until_ms = Some(until);
                    self.baseline.on_desktop_wake_hint();
                    self.return_to_monitoring(
                        ts_ms,
                        true,
                        p_sleep,
                        "grace period snoozed",
                        outputs,
                    );
                }
            }
            GraceResult::AbortedNoPenalty => {
                if self.state == DetectorState::Confirmed {
                    self.baseline.on_desktop_wake_hint();
                    self.return_to_monitoring(
                        ts_ms,
                        true,
                        p_sleep,
                        "grace period aborted without penalty",
                        outputs,
                    );
                }
            }
            GraceResult::PostTriggerRapidWake => {
                if matches!(
                    self.state,
                    DetectorState::Confirmed | DetectorState::Sleeping
                ) {
                    self.rapid_wake_fp_count = self.rapid_wake_fp_count.saturating_add(1);
                    self.apply_false_positive_cooldown(ts_ms, outputs);
                    self.clear_sleeping_accumulators();
                    self.return_to_monitoring(
                        ts_ms,
                        true,
                        p_sleep,
                        "post-trigger rapid wake false positive",
                        outputs,
                    );
                }
            }
        }
    }

    pub(super) fn return_to_monitoring(
        &mut self,
        ts_ms: i64,
        clear_onset_marks: bool,
        p_sleep: f64,
        reason: &str,
        outputs: &mut Vec<EngineOutput>,
    ) {
        self.reset_candidate_counters();
        self.confirmed_arousal_streak = 0;
        if clear_onset_marks {
            self.candidate_ts_ms = None;
            self.confirmed_ts_ms = None;
            self.onset_hr_bpm = None;
        }
        self.transition_to(ts_ms, DetectorState::Monitoring, p_sleep, reason, outputs);
    }

    pub(super) fn apply_false_positive_cooldown(
        &mut self,
        ts_ms: i64,
        outputs: &mut Vec<EngineOutput>,
    ) {
        self.suppress_candidate_until_ms = Some(ts_ms.saturating_add(CANCEL_COOLDOWN_MS));
        self.baseline.on_cancel_cooldown_start(
            ts_ms,
            self.last_hr_bpm_60s,
            self.last_local_mad,
            self.last_ln_rmssd,
        );
        let counted = !std::mem::replace(&mut self.fp_recorded_this_session, true);
        if counted {
            self.fp_count = self.fp_count.saturating_add(1);
        }
        outputs.push(EngineOutput::FalsePositiveRecorded {
            session_index: self.session_index.unwrap_or(1),
            ts_ms,
            counted,
        });
    }

    pub(super) fn has_wake_rebound(&self, hr_bpm: f64, z_hr: f64, p_sleep: f64) -> bool {
        self.sleeping_hr_anchor.is_some_and(|a| hr_bpm >= a + 4.0) || z_hr >= -0.8 || p_sleep < 0.30
    }

    pub(super) fn reset_candidate_counters(&mut self) {
        self.candidate_epochs = 0;
        self.candidate_dwell_sec = 0;
        self.candidate_hrv_valid_epochs = 0;
        self.candidate_arousal_streak = 0;
    }

    pub(super) fn required_warmup_epochs(&self) -> u32 {
        match self.config.priors.maturity {
            CalibrationMaturity::Calibrated => WARMUP_EPOCHS_CALIBRATED,
            _ => WARMUP_EPOCHS_COLD_OR_LEARNING,
        }
    }

    pub(super) fn evaluate_warmup_anomaly(&mut self) {
        let p = &self.config.priors;
        if let (Some(ref_hr), Some(mad), Some(mu)) = (
            p.wake_median.or(p.mu_wake),
            p.wake_mad_s,
            self.baseline.mu_hr(),
        ) && (mu - ref_hr).abs() > 2.5 * mad.max(SIGMA_MIN_BPM)
        {
            self.anomalous = true;
        }
    }

    pub(super) fn expected_z_sleep(&self, baseline_mu_hr: f64, baseline_sigma_hr: f64) -> f64 {
        let p = &self.config.priors;
        if self.anomalous || (p.maturity == CalibrationMaturity::ColdStart && p.mu_sleep.is_none())
        {
            return DEFAULT_EXPECTED_Z_SLEEP;
        }
        let denom = baseline_sigma_hr.max(SIGMA_MIN_BPM);
        let from_delta = p.delta_hr_onset.map(|d| (-d.abs()) / denom);
        let from_sleep_mu = p.mu_sleep.map(|ms| (ms - baseline_mu_hr) / denom);
        let personal = match (from_delta, from_sleep_mu) {
            (Some(a), Some(b)) => 0.5 * (a + b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return DEFAULT_EXPECTED_Z_SLEEP,
        };
        let weight = match p.maturity {
            CalibrationMaturity::ColdStart => 0.25,
            CalibrationMaturity::Learning => 0.5,
            CalibrationMaturity::Calibrated => 0.8,
        };
        ((1.0 - weight) * DEFAULT_EXPECTED_Z_SLEEP + weight * personal).clamp(-3.5, -1.6)
    }

    pub(super) fn snapshot_warmup_baseline(&mut self) {
        self.warmup_mu_hr = self.baseline.mu_hr();
        self.warmup_sigma_hr = Some(self.baseline.sigma_hr());
        self.warmup_ln_rmssd = self.baseline.mu_ln_rmssd();
    }

    pub(super) fn clear_sleeping_accumulators(&mut self) {
        self.post_sleep_wake_sec = 0;
        self.sleeping_ts_ms = None;
        self.sleeping_hr_anchor = None;
        self.last_sleeping_desktop_activity_ms = None;
        self.post_trigger_sleep_sec = 0;
        self.sleep_samples.clear();
    }

    pub(super) fn reset_session_accumulators(&mut self, ts_ms: i64) {
        self.session_start_ts_ms = Some(ts_ms);
        self.candidate_ts_ms = None;
        self.confirmed_ts_ms = None;
        self.warmup_mu_hr = None;
        self.warmup_sigma_hr = None;
        self.warmup_ln_rmssd = None;
        self.onset_hr_bpm = None;
        self.epochs_count = 0;
        self.warmup_clean_epochs = 0;
        self.stabilization_remaining_epochs = 0;
        self.anomalous = false;
        self.fp_recorded_this_session = false;
        self.fp_count = 0;
        self.grace_cancel_count = 0;
        self.rapid_wake_fp_count = 0;
        self.snooze_count = 0;
        self.had_rr = false;
        self.sqi_sum = 0.0;
        self.sqi_count = 0;
        self.suppress_candidate_until_ms = None;
        self.active_gap = None;
        self.last_any_packet_ts_ms = None;
        self.last_valid_packet_ts_ms = None;
        self.reset_candidate_counters();
        self.confirmed_arousal_streak = 0;
        self.clear_sleeping_accumulators();
        self.last_hr_bpm_60s = None;
        self.last_z_hr = None;
        self.last_local_mad = None;
        self.last_ln_rmssd = None;

        self.ppg = PpgPipeline::new();
        self.ppg.set_epoch_anchor(ts_ms);
        self.baseline = QuietWakeBaseline::new();
        self.hmm.reset();
    }

    pub(super) fn transition_to(
        &mut self,
        ts_ms: i64,
        to: DetectorState,
        p_sleep: f64,
        reason: &str,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let from = self.state;
        if from != to {
            self.state = to;
            outputs.push(EngineOutput::StateChanged {
                session_index: self.session_index.unwrap_or(1),
                ts_ms,
                from,
                to,
                p_sleep,
                reason: reason.to_string(),
            });
        }
    }

    pub(super) fn open_session(
        &mut self,
        ts_ms: i64,
        reason: &str,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let next_idx = self.session_index.unwrap_or(0) + 1;
        self.session_index = Some(next_idx);
        self.reset_session_accumulators(ts_ms);

        let prev = self.state;
        self.state = DetectorState::Warmup;
        outputs.push(EngineOutput::SessionStarted {
            session_index: next_idx,
            ts_ms,
        });
        outputs.push(EngineOutput::StateChanged {
            session_index: next_idx,
            ts_ms,
            from: prev,
            to: DetectorState::Warmup,
            p_sleep: self.hmm.probs()[STATE_SLEEP_NREM],
            reason: reason.to_string(),
        });
    }

    fn build_summary(
        &self,
        session_index: u64,
        end_ts_ms: i64,
        end_reason: SessionEndReason,
        medians: (Option<f64>, Option<f64>, Option<f64>),
    ) -> SessionSummary {
        let (sleep_mu_hr, sleep_ln_rmssd, observed_hr_var_ratio) = medians;
        let typical_sqi = (self.sqi_count > 0).then(|| self.sqi_sum / self.sqi_count as f64);
        SessionSummary {
            session_index,
            start_ts_ms: self.session_start_ts_ms.unwrap_or(end_ts_ms),
            end_ts_ms,
            end_reason,
            candidate_ts_ms: self.candidate_ts_ms,
            confirmed_ts_ms: self.confirmed_ts_ms,
            sleeping_ts_ms: self.sleeping_ts_ms,
            anomalous: self.anomalous,
            fp_count: self.fp_count,
            warmup_mu_hr: self.warmup_mu_hr.or_else(|| self.baseline.mu_hr()),
            warmup_sigma_hr: self
                .warmup_sigma_hr
                .or_else(|| (self.epochs_count > 0).then(|| self.baseline.sigma_hr())),
            warmup_ln_rmssd: self.warmup_ln_rmssd.or_else(|| self.baseline.mu_ln_rmssd()),
            onset_hr_bpm: self.onset_hr_bpm,
            sleep_mu_hr,
            sleep_ln_rmssd,
            observed_hr_var_ratio,
            typical_sqi,
            post_trigger_sleep_sec: self.post_trigger_sleep_sec,
            had_rr: self.had_rr,
            grace_cancel_count: self.grace_cancel_count,
            rapid_wake_fp_count: self.rapid_wake_fp_count,
            snooze_count: self.snooze_count,
            epochs_count: self.epochs_count,
        }
    }

    /// Snapshots summary metrics for the currently active `Monitoring Session` without mutating state.
    pub(crate) fn snapshot_active_summary(
        &self,
        end_ts_ms: i64,
        end_reason: SessionEndReason,
    ) -> Option<SessionSummary> {
        let session_index = self.session_index?;
        Some(self.build_summary(
            session_index,
            end_ts_ms,
            end_reason,
            self.sleep_samples.peek_medians(),
        ))
    }

    pub(super) fn close_session(
        &mut self,
        end_ts_ms: i64,
        end_reason: SessionEndReason,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let Some(session_index) = self.session_index else {
            return;
        };
        let medians = self.sleep_samples.finalize_medians();
        let summary = self.build_summary(session_index, end_ts_ms, end_reason, medians);
        let prev = self.state;
        self.state = DetectorState::Idle;
        self.active_gap = None;
        if prev != DetectorState::Idle {
            outputs.push(EngineOutput::StateChanged {
                session_index,
                ts_ms: end_ts_ms,
                from: prev,
                to: DetectorState::Idle,
                p_sleep: self.hmm.probs()[STATE_SLEEP_NREM],
                reason: "monitoring session closed".to_string(),
            });
        }
        outputs.push(EngineOutput::SessionEnded(summary));
    }

    /// Returns the current state-machine phase.
    pub fn state(&self) -> DetectorState {
        self.state
    }

    /// Returns the active 1-based `Monitoring Session` index, if a session is open.
    pub fn current_session_index(&self) -> Option<u64> {
        self.session_index
    }

    /// Returns whether the active `Monitoring Session` is flagged as an `Anomalous Session`.
    pub fn is_session_anomalous(&self) -> bool {
        self.anomalous
    }

    /// Returns the active gap start timestamp in milliseconds, if the engine is currently in a gap.
    pub fn gap_started_ms(&self) -> Option<i64> {
        if self.state == DetectorState::Idle {
            return None;
        }
        self.active_gap.map(|gap| gap.started_ms)
    }

    /// Returns the millisecond timestamp when the active gap will exceed its disconnect-reason-aware closure window.
    pub fn gap_deadline_ms(&self) -> Option<i64> {
        if self.state == DetectorState::Idle {
            return None;
        }
        self.active_gap.map(|gap| gap.deadline_ms)
    }

    /// Checks whether an active gap has exceeded its active closure deadline, closing the session if so.
    pub fn check_gap_timeout(&mut self, now_ms: i64) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        if self.state != DetectorState::Idle
            && let Some(deadline_ms) = self.gap_deadline_ms()
            && now_ms >= deadline_ms
        {
            self.resolve_active_gap(now_ms, false, &mut outputs);
        }
        outputs
    }
}
