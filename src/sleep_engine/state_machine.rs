//! Deterministic outer sleep-onset state machine and `Monitoring Session` lifecycle.

use crate::config::DetectionConfig;
use crate::sleep_engine::baseline::QuietWakeBaseline;
use crate::sleep_engine::hmm::{HmmFilter, STATE_ACTIVE_WAKE, STATE_QUIET_WAKE, STATE_SLEEP_NREM};
use crate::sleep_engine::ppg::{
    MIN_EPOCH_COVERAGE, PpgPipeline, RawEpochMetrics, SIGMA_MIN_BPM, normalize_features, quantile,
};
use crate::sleep_engine::types::{
    CalibrationMaturity, CalibrationPriors, DetectorState, EngineConfig, EngineEvent, EngineOutput,
    Epoch, EpochFeatures, GapTier, GraceAbortReason, GraceResult, HrPacket, PresetParams,
    SessionEndReason, SessionSummary, StreamStatus, WearableFloor,
};

const WARMUP_EPOCHS_COLD_OR_LEARNING: u32 = 32; // 8 min
const WARMUP_EPOCHS_CALIBRATED: u32 = 20; // 5 min
const DEFAULT_EXPECTED_Z_SLEEP: f64 = -2.4;

const IMPLICIT_SILENCE_GAP_MS: i64 = 10_000;
const IMPLICIT_INVALID_GAP_MS: i64 = 15_000;
const SHORT_GAP_MAX_MS: i64 = 120_000; // <= 2 min
const MEDIUM_GAP_MAX_MS: i64 = 900_000; // 2-15 min
const MEDIUM_GAP_STABILIZATION_EPOCHS: u32 = 8; // 2 min
const CANCEL_COOLDOWN_MS: i64 = 900_000; // 15 min
const POST_SLEEP_WAKE_SPLIT_SEC: u32 = 600; // 10 min
const DESKTOP_REBOUND_WINDOW_MS: i64 = 300_000; // 5 min

struct EpochStepInput<'a> {
    ts_ms: i64,
    features: EpochFeatures,
    raw_opt: Option<&'a RawEpochMetrics>,
    hrv_valid: bool,
    rr_artifact_frac: f64,
    coverage: f64,
    flatline_stale: bool,
    accumulators_frozen: bool,
}

/// Pure-synchronous sleep-onset detection engine.
pub struct SleepEngine {
    config: EngineConfig,
    preset: PresetParams,
    state: DetectorState,
    session_index: Option<u64>,
    session_start_ts_ms: Option<i64>,
    candidate_ts_ms: Option<i64>,
    confirmed_ts_ms: Option<i64>,
    sleeping_ts_ms: Option<i64>,
    warmup_mu_hr: Option<f64>,
    warmup_sigma_hr: Option<f64>,
    warmup_ln_rmssd: Option<f64>,
    onset_hr_bpm: Option<f64>,
    epochs_count: usize,
    warmup_clean_epochs: u32,
    stabilization_remaining_epochs: u32,
    anomalous: bool,
    fp_recorded_this_session: bool,
    fp_count: u8,
    grace_cancel_count: u8,
    rapid_wake_fp_count: u8,
    snooze_count: u8,
    had_rr: bool,
    post_trigger_sleep_sec: u32,
    sleep_hr_samples: Vec<f64>,
    sleep_ln_rmssd_samples: Vec<f64>,
    sleep_var_ratio_samples: Vec<f64>,
    sqi_sum: f64,
    sqi_count: usize,

    suppress_candidate_until_ms: Option<i64>,
    gap_started_ms: Option<i64>,
    last_any_packet_ts_ms: Option<i64>,
    last_valid_packet_ts_ms: Option<i64>,

    candidate_epochs: u32,
    candidate_dwell_sec: u32,
    candidate_hrv_valid_epochs: u32,
    candidate_arousal_streak: u32,
    confirmed_arousal_streak: u32,

    post_sleep_wake_sec: u32,
    sleeping_hr_anchor: Option<f64>,
    last_sleeping_desktop_activity_ms: Option<i64>,

    last_hr_bpm_60s: Option<f64>,
    last_z_hr: Option<f64>,
    last_local_mad: Option<f64>,
    last_ln_rmssd: Option<f64>,

    ppg: PpgPipeline,
    baseline: QuietWakeBaseline,
    hmm: HmmFilter,
}

impl SleepEngine {
    /// Creates a new sleep-onset engine configured with presets, priors, and Wearable floor.
    pub fn new(config: EngineConfig) -> Self {
        let preset = PresetParams::resolve(&config.detection, &config.priors);
        Self {
            config,
            preset,
            state: DetectorState::Idle,
            session_index: None,
            session_start_ts_ms: None,
            candidate_ts_ms: None,
            confirmed_ts_ms: None,
            sleeping_ts_ms: None,
            warmup_mu_hr: None,
            warmup_sigma_hr: None,
            warmup_ln_rmssd: None,
            onset_hr_bpm: None,
            epochs_count: 0,
            warmup_clean_epochs: 0,
            stabilization_remaining_epochs: 0,
            anomalous: false,
            fp_recorded_this_session: false,
            fp_count: 0,
            grace_cancel_count: 0,
            rapid_wake_fp_count: 0,
            snooze_count: 0,
            had_rr: false,
            post_trigger_sleep_sec: 0,
            sleep_hr_samples: Vec::new(),
            sleep_ln_rmssd_samples: Vec::new(),
            sleep_var_ratio_samples: Vec::new(),
            sqi_sum: 0.0,
            sqi_count: 0,
            suppress_candidate_until_ms: None,
            gap_started_ms: None,
            last_any_packet_ts_ms: None,
            last_valid_packet_ts_ms: None,
            candidate_epochs: 0,
            candidate_dwell_sec: 0,
            candidate_hrv_valid_epochs: 0,
            candidate_arousal_streak: 0,
            confirmed_arousal_streak: 0,
            post_sleep_wake_sec: 0,
            sleeping_hr_anchor: None,
            last_sleeping_desktop_activity_ms: None,
            last_hr_bpm_60s: None,
            last_z_hr: None,
            last_local_mad: None,
            last_ln_rmssd: None,
            ppg: PpgPipeline::new(),
            baseline: QuietWakeBaseline::new(),
            hmm: HmmFilter::new(),
        }
    }

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

    /// Processes one explicit timestamped input event synchronously.
    pub fn handle_event(&mut self, event: EngineEvent) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        match event {
            EngineEvent::StreamState(st) => {
                self.handle_stream_state(st.ts_ms, st.status, &mut outputs);
            }
            EngineEvent::HrPacket(pkt) => self.handle_hr_packet(pkt, &mut outputs),
            EngineEvent::DesktopActivity(act) => {
                self.handle_desktop_activity(act.ts_ms, &mut outputs);
            }
            EngineEvent::GraceOutcome(gr) => {
                self.handle_grace_outcome(gr.ts_ms, gr.outcome, &mut outputs);
            }
        }
        outputs
    }

    fn handle_stream_state(
        &mut self,
        ts_ms: i64,
        status: StreamStatus,
        outputs: &mut Vec<EngineOutput>,
    ) {
        match status {
            StreamStatus::Streaming => {
                if self.state == DetectorState::Idle {
                    self.open_session(ts_ms, "stream started", outputs);
                } else if self.gap_started_ms.is_some() {
                    self.resolve_active_gap(ts_ms, true, outputs);
                }
            }
            StreamStatus::SignalLost | StreamStatus::Disconnected => {
                if self.state != DetectorState::Idle && self.gap_started_ms.is_none() {
                    self.gap_started_ms = Some(ts_ms);
                }
            }
            StreamStatus::Connecting | StreamStatus::Acquiring | StreamStatus::PassiveStandby => {}
        }
    }

    fn handle_hr_packet(&mut self, pkt: HrPacket, outputs: &mut Vec<EngineOutput>) {
        let pkt_valid = PpgPipeline::is_packet_valid_stage1(&pkt);

        if let Some(prev_any) = self.last_any_packet_ts_ms
            && pkt.ts_ms < prev_any
        {
            return;
        }

        if self.state == DetectorState::Idle {
            if !pkt_valid {
                return;
            }
            self.open_session(pkt.ts_ms, "hr packet stream started", outputs);
        } else if pkt_valid {
            if self.gap_started_ms.is_some() {
                self.resolve_active_gap(pkt.ts_ms, true, outputs);
            } else if let Some(prev_any) = self.last_any_packet_ts_ms
                && pkt.ts_ms - prev_any > IMPLICIT_SILENCE_GAP_MS
            {
                self.gap_started_ms = Some(prev_any);
                self.resolve_active_gap(pkt.ts_ms, true, outputs);
            } else if let Some(prev_valid) = self.last_valid_packet_ts_ms
                && pkt.ts_ms - prev_valid > IMPLICIT_INVALID_GAP_MS
            {
                self.gap_started_ms = Some(prev_valid);
                self.resolve_active_gap(pkt.ts_ms, true, outputs);
            }
        }

        self.last_any_packet_ts_ms = Some(pkt.ts_ms);
        if pkt_valid {
            self.last_valid_packet_ts_ms = Some(pkt.ts_ms);
            if !pkt.rr_ms.is_empty() {
                self.had_rr = true;
            }
        }

        if let Some(raw) = self.ppg.ingest_packet(&pkt) {
            self.process_raw_epoch(&raw, outputs);
        }
    }

    fn resolve_active_gap(
        &mut self,
        ts_ms: i64,
        reopen_on_close: bool,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let Some(gap_start) = self.gap_started_ms.take() else {
            return;
        };
        let duration_ms = (ts_ms - gap_start).max(0);
        let session_idx = self.session_index.unwrap_or(1);

        if duration_ms <= SHORT_GAP_MAX_MS {
            self.ppg.on_short_gap_resume(ts_ms);
            self.last_any_packet_ts_ms = Some(ts_ms);
            self.last_valid_packet_ts_ms = Some(ts_ms);
            outputs.push(EngineOutput::GapResolved {
                session_index: session_idx,
                ts_ms,
                tier: GapTier::ShortPause,
                duration_ms,
            });
        } else if duration_ms <= MEDIUM_GAP_MAX_MS {
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
        } else {
            outputs.push(EngineOutput::GapResolved {
                session_index: session_idx,
                ts_ms,
                tier: GapTier::LongSessionClose,
                duration_ms,
            });
            self.close_session(gap_start, SessionEndReason::GapExceeded, outputs);
            if reopen_on_close {
                self.open_session(ts_ms, "stream resumed after >15 min gap", outputs);
            }
        }
    }

    fn process_raw_epoch(&mut self, raw: &RawEpochMetrics, outputs: &mut Vec<EngineOutput>) {
        if self.state == DetectorState::Warmup {
            self.baseline.accept_warmup(raw);
        }
        let mu_hr = self.baseline.mu_hr_or(raw.hr_bpm_60s);
        let sigma_hr = self.baseline.sigma_hr();
        let features = normalize_features(
            raw,
            mu_hr,
            sigma_hr,
            self.baseline.mu_ln_rmssd(),
            self.config.wearable_floor,
        );
        self.process_features_step(
            EpochStepInput {
                ts_ms: raw.ts_ms,
                features,
                raw_opt: Some(raw),
                hrv_valid: raw.hrv_valid,
                rr_artifact_frac: raw.rr_artifact_frac,
                coverage: raw.coverage,
                flatline_stale: raw.flatline_stale,
                accumulators_frozen: raw.accumulators_frozen,
            },
            outputs,
        );
    }

    fn process_features_step(
        &mut self,
        input: EpochStepInput<'_>,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let EpochStepInput {
            ts_ms,
            features,
            raw_opt,
            hrv_valid,
            rr_artifact_frac,
            coverage,
            flatline_stale,
            accumulators_frozen,
        } = input;

        let mu_hr = self.baseline.mu_hr_or(features.hr_bpm_60s);
        let sigma_hr = self.baseline.sigma_hr();

        let probs = if accumulators_frozen || self.state == DetectorState::Warmup {
            self.hmm.probs()
        } else {
            let expected_z = self.expected_z_sleep(mu_hr, sigma_hr);
            self.hmm.step(&features, hrv_valid, expected_z)
        };
        let p_sleep = probs[STATE_SLEEP_NREM];

        self.last_hr_bpm_60s = Some(features.hr_bpm_60s);
        self.last_z_hr = Some(features.z_hr);
        self.last_local_mad = Some(features.r_var_120s * sigma_hr);
        if let Some(ln_r) = features.ln_rmssd_60s {
            self.last_ln_rmssd = Some(ln_r);
        }
        if hrv_valid || features.ln_rmssd_60s.is_some() {
            self.had_rr = true;
        }
        let epoch_sqi = (coverage * (1.0 - rr_artifact_frac)).clamp(0.0, 1.0);
        if epoch_sqi.is_finite() {
            self.sqi_sum += epoch_sqi;
            self.sqi_count += 1;
        }
        let unclamped_var_ratio = raw_opt
            .map(|r| r.local_mad_120s / sigma_hr.max(SIGMA_MIN_BPM))
            .unwrap_or(features.r_var_120s);

        let mut split_reason: Option<SessionEndReason> = None;

        let synth_raw;
        let raw_ref = if let Some(raw) = raw_opt {
            raw
        } else {
            synth_raw = RawEpochMetrics {
                ts_ms,
                hr_bpm_60s: features.hr_bpm_60s,
                hr_slope_4m: features.hr_slope_4m,
                local_mad_120s: features.r_var_120s * sigma_hr,
                ln_rmssd_60s: features.ln_rmssd_60s,
                rmssd_sdnn_120s: features.rmssd_sdnn_120s,
                hrv_valid,
                rr_artifact_frac,
                coverage,
                flatline_stale,
                accumulators_frozen,
            };
            &synth_raw
        };

        if !accumulators_frozen {
            match self.state {
                DetectorState::Idle => {}
                DetectorState::Warmup => {
                    if raw_opt.is_none() {
                        self.baseline.accept_warmup(raw_ref);
                    }
                    self.warmup_clean_epochs += 1;
                    if self.warmup_clean_epochs >= self.required_warmup_epochs() {
                        self.snapshot_warmup_baseline();
                        self.evaluate_warmup_anomaly();
                        self.transition_to(
                            ts_ms,
                            DetectorState::Monitoring,
                            p_sleep,
                            "warmup completed",
                            outputs,
                        );
                    }
                }
                DetectorState::Monitoring => {
                    self.baseline.update_post_warmup(raw_ref, p_sleep, false);
                    if self.stabilization_remaining_epochs > 0 {
                        self.stabilization_remaining_epochs -= 1;
                    } else {
                        let cooldown_clear = self
                            .suppress_candidate_until_ms
                            .is_none_or(|until| ts_ms >= until);
                        if cooldown_clear && p_sleep >= self.preset.p_sleep_threshold {
                            self.enter_candidate(ts_ms, hrv_valid, p_sleep, outputs);
                        }
                    }
                }
                DetectorState::Candidate => {
                    self.baseline.set_frozen(true);
                    self.step_candidate(ts_ms, &features, probs, hrv_valid, outputs);
                }
                DetectorState::Confirmed => {
                    self.baseline.set_frozen(true);
                    if p_sleep < self.preset.reset_threshold {
                        self.confirmed_arousal_streak += 1;
                    } else {
                        self.confirmed_arousal_streak = 0;
                    }
                    if self.confirmed_arousal_streak >= 2 {
                        outputs.push(EngineOutput::GraceAbortRequested {
                            session_index: self.session_index.unwrap_or(1),
                            ts_ms,
                            reason: GraceAbortReason::PhysiologicalArousal,
                        });
                        self.baseline.on_desktop_wake_hint();
                        self.return_to_monitoring(
                            ts_ms,
                            true,
                            p_sleep,
                            "physiological arousal during grace period",
                            outputs,
                        );
                    }
                }
                DetectorState::Sleeping => {
                    self.baseline.set_frozen(true);
                    self.sleep_hr_samples.push(features.hr_bpm_60s);
                    if hrv_valid && let Some(ln_r) = features.ln_rmssd_60s {
                        self.sleep_ln_rmssd_samples.push(ln_r);
                    }
                    if unclamped_var_ratio.is_finite() {
                        self.sleep_var_ratio_samples.push(unclamped_var_ratio);
                    }
                    if p_sleep < 0.30 {
                        self.post_sleep_wake_sec += 15;
                    } else {
                        self.post_sleep_wake_sec = 0;
                        self.post_trigger_sleep_sec =
                            self.post_trigger_sleep_sec.saturating_add(15);
                    }

                    let recent_desktop_wake = self
                        .last_sleeping_desktop_activity_ms
                        .is_some_and(|act_ts| ts_ms - act_ts <= DESKTOP_REBOUND_WINDOW_MS)
                        && self.has_wake_rebound(features.hr_bpm_60s, features.z_hr, p_sleep);

                    if recent_desktop_wake {
                        split_reason = Some(SessionEndReason::DesktopWakeRebound);
                    } else if self.post_sleep_wake_sec >= POST_SLEEP_WAKE_SPLIT_SEC {
                        split_reason = Some(SessionEndReason::SustainedWakefulness);
                    }
                }
            }
        }

        self.epochs_count += 1;
        outputs.push(EngineOutput::EpochEmitted(Epoch {
            session_index: self.session_index.unwrap_or(1),
            ts_ms,
            features,
            hrv_valid,
            rr_artifact_frac,
            coverage,
            flatline_stale,
            accumulators_frozen,
            baseline_mu_hr: self.baseline.mu_hr_or(mu_hr),
            baseline_sigma_hr: self.baseline.sigma_hr(),
            baseline_frozen: self.baseline.is_frozen(),
            p_active_wake: probs[STATE_ACTIVE_WAKE],
            p_quiet_wake: probs[STATE_QUIET_WAKE],
            p_sleep,
            state: self.state,
        }));

        if let Some(reason) = split_reason {
            self.close_session(ts_ms, reason, outputs);
            self.open_session(ts_ms, "post-sleep wakefulness session split", outputs);
        }
    }

    fn enter_candidate(
        &mut self,
        ts_ms: i64,
        hrv_valid: bool,
        p_sleep: f64,
        outputs: &mut Vec<EngineOutput>,
    ) {
        self.candidate_ts_ms = Some(ts_ms);
        self.candidate_epochs = 1;
        self.candidate_dwell_sec = 15;
        self.candidate_hrv_valid_epochs = u32::from(hrv_valid);
        self.candidate_arousal_streak = 0;
        self.baseline.set_frozen(true);
        self.transition_to(
            ts_ms,
            DetectorState::Candidate,
            p_sleep,
            "p_sleep threshold crossed",
            outputs,
        );
    }

    fn step_candidate(
        &mut self,
        ts_ms: i64,
        features: &EpochFeatures,
        probs: [f64; 3],
        hrv_valid: bool,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let p_active_wake = probs[STATE_ACTIVE_WAKE];
        let p_sleep = probs[STATE_SLEEP_NREM];

        let arousal = p_active_wake > 0.50 || (features.z_hr > -0.5 && features.r_var_120s > 1.1);
        if arousal {
            self.candidate_arousal_streak += 1;
        } else {
            self.candidate_arousal_streak = 0;
        }

        if p_sleep < self.preset.reset_threshold || self.candidate_arousal_streak >= 2 {
            self.baseline.set_frozen(false);
            self.return_to_monitoring(
                ts_ms,
                true,
                p_sleep,
                "candidate reset by arousal or low p_sleep",
                outputs,
            );
            return;
        }

        self.candidate_epochs += 1;
        self.candidate_dwell_sec += 15;
        if hrv_valid {
            self.candidate_hrv_valid_epochs += 1;
        }

        let hrv_frac = self.candidate_hrv_valid_epochs as f64 / self.candidate_epochs as f64;
        let required_dwell = if hrv_frac >= 0.80 {
            self.preset.dwell_bpm_rr_sec
        } else {
            self.preset.dwell_bpm_only_sec
        };

        if self.candidate_dwell_sec >= required_dwell {
            self.confirmed_ts_ms = Some(ts_ms);
            self.onset_hr_bpm = Some(features.hr_bpm_60s);
            self.confirmed_arousal_streak = 0;
            self.transition_to(
                ts_ms,
                DetectorState::Confirmed,
                p_sleep,
                "confirmation dwell satisfied",
                outputs,
            );
        }
    }

    fn handle_desktop_activity(&mut self, ts_ms: i64, outputs: &mut Vec<EngineOutput>) {
        match self.state {
            DetectorState::Idle | DetectorState::Warmup | DetectorState::Confirmed => {}
            DetectorState::Monitoring => {
                self.baseline.on_desktop_wake_hint();
            }
            DetectorState::Candidate => {
                self.baseline.on_desktop_wake_hint();
                let p_sleep = self.hmm.probs()[STATE_SLEEP_NREM];
                self.return_to_monitoring(
                    ts_ms,
                    true,
                    p_sleep,
                    "desktop activity cancelled candidate",
                    outputs,
                );
            }
            DetectorState::Sleeping => {
                self.last_sleeping_desktop_activity_ms = Some(ts_ms);
                if let (Some(hr), Some(z_hr)) = (self.last_hr_bpm_60s, self.last_z_hr)
                    && self.has_wake_rebound(hr, z_hr, self.hmm.probs()[STATE_SLEEP_NREM])
                {
                    self.close_session(ts_ms, SessionEndReason::DesktopWakeRebound, outputs);
                    self.open_session(ts_ms, "desktop wake rebound session split", outputs);
                }
            }
        }
    }

    fn handle_grace_outcome(
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

    fn return_to_monitoring(
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

    fn apply_false_positive_cooldown(&mut self, ts_ms: i64, outputs: &mut Vec<EngineOutput>) {
        self.suppress_candidate_until_ms = Some(ts_ms.saturating_add(CANCEL_COOLDOWN_MS));
        self.baseline.on_cancel_cooldown_start(
            ts_ms,
            self.last_hr_bpm_60s,
            self.last_local_mad,
            self.last_ln_rmssd,
        );
        let counted = !self.fp_recorded_this_session;
        self.fp_recorded_this_session = true;
        if counted {
            self.fp_count = self.fp_count.saturating_add(1);
        }
        outputs.push(EngineOutput::FalsePositiveRecorded {
            session_index: self.session_index.unwrap_or(1),
            ts_ms,
            counted,
        });
    }

    fn has_wake_rebound(&self, hr_bpm: f64, z_hr: f64, p_sleep: f64) -> bool {
        let anchor_rebound = self
            .sleeping_hr_anchor
            .is_some_and(|anchor| hr_bpm >= anchor + 4.0);
        anchor_rebound || z_hr >= -0.8 || p_sleep < 0.30
    }

    fn reset_candidate_counters(&mut self) {
        self.candidate_epochs = 0;
        self.candidate_dwell_sec = 0;
        self.candidate_hrv_valid_epochs = 0;
        self.candidate_arousal_streak = 0;
    }

    fn required_warmup_epochs(&self) -> u32 {
        match self.config.priors.maturity {
            CalibrationMaturity::Calibrated => WARMUP_EPOCHS_CALIBRATED,
            CalibrationMaturity::ColdStart | CalibrationMaturity::Learning => {
                WARMUP_EPOCHS_COLD_OR_LEARNING
            }
        }
    }

    fn evaluate_warmup_anomaly(&mut self) {
        if let (Some(wake_ref), Some(wake_mad), Some(session_mu)) = (
            self.config
                .priors
                .wake_median
                .or(self.config.priors.mu_wake),
            self.config.priors.wake_mad_s,
            self.baseline.mu_hr(),
        ) {
            let scale = wake_mad.max(SIGMA_MIN_BPM);
            if (session_mu - wake_ref).abs() > 2.5 * scale {
                self.anomalous = true;
            }
        }
    }

    fn expected_z_sleep(&self, baseline_mu_hr: f64, baseline_sigma_hr: f64) -> f64 {
        if self.anomalous {
            return DEFAULT_EXPECTED_Z_SLEEP;
        }
        if self.config.priors.maturity == CalibrationMaturity::ColdStart
            && self.config.priors.mu_sleep.is_none()
        {
            return DEFAULT_EXPECTED_Z_SLEEP;
        }
        let denom = baseline_sigma_hr.max(SIGMA_MIN_BPM);
        let from_delta = self
            .config
            .priors
            .delta_hr_onset
            .map(|d| (-d.abs()) / denom);
        let from_sleep_mu = self
            .config
            .priors
            .mu_sleep
            .map(|ms| (ms - baseline_mu_hr) / denom);

        let personal = match (from_delta, from_sleep_mu) {
            (Some(a), Some(b)) => 0.5 * (a + b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return DEFAULT_EXPECTED_Z_SLEEP,
        };

        let weight = match self.config.priors.maturity {
            CalibrationMaturity::ColdStart => 0.25,
            CalibrationMaturity::Learning => 0.5,
            CalibrationMaturity::Calibrated => 0.8,
        };
        let blended = (1.0 - weight) * DEFAULT_EXPECTED_Z_SLEEP + weight * personal;
        blended.clamp(-3.5, -1.6)
    }

    fn snapshot_warmup_baseline(&mut self) {
        self.warmup_mu_hr = self.baseline.mu_hr();
        self.warmup_sigma_hr = Some(self.baseline.sigma_hr());
        self.warmup_ln_rmssd = self.baseline.mu_ln_rmssd();
    }

    fn clear_sleeping_accumulators(&mut self) {
        self.post_sleep_wake_sec = 0;
        self.sleeping_ts_ms = None;
        self.sleeping_hr_anchor = None;
        self.last_sleeping_desktop_activity_ms = None;
        self.post_trigger_sleep_sec = 0;
        self.sleep_hr_samples.clear();
        self.sleep_ln_rmssd_samples.clear();
        self.sleep_var_ratio_samples.clear();
    }

    fn reset_session_accumulators(&mut self, ts_ms: i64) {
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
        self.gap_started_ms = None;
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

    fn transition_to(
        &mut self,
        ts_ms: i64,
        to: DetectorState,
        p_sleep: f64,
        reason: &str,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let from = self.state;
        if from == to {
            return;
        }
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

    fn open_session(&mut self, ts_ms: i64, reason: &str, outputs: &mut Vec<EngineOutput>) {
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

    fn close_session(
        &mut self,
        end_ts_ms: i64,
        end_reason: SessionEndReason,
        outputs: &mut Vec<EngineOutput>,
    ) {
        let Some(session_index) = self.session_index else {
            return;
        };
        let sleep_mu_hr = quantile(&self.sleep_hr_samples, 0.5);
        let sleep_ln_rmssd = quantile(&self.sleep_ln_rmssd_samples, 0.5);
        let observed_hr_var_ratio = quantile(&self.sleep_var_ratio_samples, 0.5);
        let typical_sqi = (self.sqi_count > 0).then(|| self.sqi_sum / self.sqi_count as f64);
        let summary = SessionSummary {
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
        };
        let prev = self.state;
        self.state = DetectorState::Idle;
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

    /// Feeds pre-extracted 15-second `Epoch` features directly (for post-filter replay).
    pub fn feed_epoch_features(
        &mut self,
        ts_ms: i64,
        features: EpochFeatures,
        hrv_valid: bool,
        coverage: f64,
        flatline_stale: bool,
    ) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        if self.state == DetectorState::Idle {
            self.open_session(ts_ms, "epoch feature replay started", &mut outputs);
        } else if let Some(prev_ts) = self.last_any_packet_ts_ms
            && ts_ms - prev_ts > SHORT_GAP_MAX_MS
        {
            self.gap_started_ms = Some(prev_ts);
            self.resolve_active_gap(ts_ms, true, &mut outputs);
        }
        let frozen = flatline_stale || coverage < MIN_EPOCH_COVERAGE;
        self.last_any_packet_ts_ms = Some(ts_ms);
        if !frozen {
            self.last_valid_packet_ts_ms = Some(ts_ms);
        }
        self.process_features_step(
            EpochStepInput {
                ts_ms,
                features,
                raw_opt: None,
                hrv_valid,
                rr_artifact_frac: 0.0,
                coverage,
                flatline_stale,
                accumulators_frozen: frozen,
            },
            &mut outputs,
        );
        outputs
    }
}
