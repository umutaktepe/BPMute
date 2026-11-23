//! Deterministic outer sleep-onset state machine and `Monitoring Session` lifecycle.

use crate::sleep_engine::baseline::QuietWakeBaseline;
use crate::sleep_engine::hmm::{HmmFilter, STATE_ACTIVE_WAKE, STATE_QUIET_WAKE, STATE_SLEEP_NREM};
use crate::sleep_engine::ppg::{
    MIN_EPOCH_COVERAGE, PpgPipeline, RawEpochMetrics, SIGMA_MIN_BPM, normalize_features,
};
use crate::sleep_engine::types::{
    DetectorState, EngineConfig, EngineEvent, EngineOutput, Epoch, EpochFeatures, GraceAbortReason,
    HrPacket, PresetParams, SessionEndReason, StreamDisconnectReason, StreamStatus,
};

mod lifecycle;

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
const POST_SLEEP_ACTIVE_WAKE_THRESHOLD: f64 = 0.50;
const POST_SLEEP_WAKE_Z_THRESHOLD: f64 = 2.0;
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

#[derive(Debug, Default)]
struct SleepSampleVecs {
    hr: Vec<f64>,
    ln_rmssd: Vec<f64>,
    var_ratio: Vec<f64>,
}

impl SleepSampleVecs {
    fn push(&mut self, hr: f64, ln_rmssd: Option<f64>, var_ratio: f64) {
        self.hr.push(hr);
        if let Some(ln_r) = ln_rmssd {
            self.ln_rmssd.push(ln_r);
        }
        if var_ratio.is_finite() {
            self.var_ratio.push(var_ratio);
        }
    }

    fn append_from(&mut self, other: &mut Self) {
        self.hr.append(&mut other.hr);
        self.ln_rmssd.append(&mut other.ln_rmssd);
        self.var_ratio.append(&mut other.var_ratio);
    }

    fn clear(&mut self) {
        self.hr.clear();
        self.ln_rmssd.clear();
        self.var_ratio.clear();
    }
}

#[derive(Debug, Default)]
struct SleepSampleBuffers {
    committed: SleepSampleVecs,
    wake_tail: SleepSampleVecs,
}

impl SleepSampleBuffers {
    fn record_epoch(
        &mut self,
        is_wake_epoch: bool,
        hr: f64,
        ln_rmssd: Option<f64>,
        var_ratio: f64,
    ) {
        if is_wake_epoch {
            self.wake_tail.push(hr, ln_rmssd, var_ratio);
        } else {
            // Rejoin buffered brief wake/REM intervals when consolidated sleep resumes.
            self.committed.append_from(&mut self.wake_tail);
            self.committed.push(hr, ln_rmssd, var_ratio);
        }
    }

    fn medians_for(vecs: &SleepSampleVecs) -> (Option<f64>, Option<f64>, Option<f64>) {
        (
            crate::sleep_engine::ppg::quantile(&vecs.hr, 0.5),
            crate::sleep_engine::ppg::quantile(&vecs.ln_rmssd, 0.5),
            crate::sleep_engine::ppg::quantile(&vecs.var_ratio, 0.5),
        )
    }

    pub(super) fn peek_medians(&self) -> (Option<f64>, Option<f64>, Option<f64>) {
        let src = if self.committed.hr.is_empty() {
            &self.wake_tail
        } else {
            &self.committed
        };
        Self::medians_for(src)
    }

    fn finalize_medians(&mut self) -> (Option<f64>, Option<f64>, Option<f64>) {
        if self.committed.hr.is_empty() {
            self.committed.append_from(&mut self.wake_tail);
        } else {
            self.wake_tail.clear();
        }
        Self::medians_for(&self.committed)
    }

    fn clear(&mut self) {
        self.committed.clear();
        self.wake_tail.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveGap {
    started_ms: i64,
    deadline_ms: i64,
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
    pre_candidate_mu_hr: Option<f64>,
    pre_candidate_sigma_hr: Option<f64>,
    pre_candidate_ln_rmssd: Option<f64>,
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
    sleep_samples: SleepSampleBuffers,
    sqi_sum: f64,
    sqi_count: usize,

    suppress_candidate_until_ms: Option<i64>,
    active_gap: Option<ActiveGap>,
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
            pre_candidate_mu_hr: None,
            pre_candidate_sigma_hr: None,
            pre_candidate_ln_rmssd: None,
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
            sleep_samples: SleepSampleBuffers::default(),
            sqi_sum: 0.0,
            sqi_count: 0,
            suppress_candidate_until_ms: None,
            active_gap: None,
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

    /// Processes one explicit timestamped input event synchronously.
    pub fn handle_event(&mut self, event: EngineEvent) -> Vec<EngineOutput> {
        let mut outputs = Vec::new();
        match event {
            EngineEvent::StreamState(st) => {
                self.handle_stream_state(st.ts_ms, st.status, &mut outputs);
            }
            EngineEvent::StreamDisconnected(dc) => {
                self.handle_stream_disconnected(dc.ts_ms, dc.reason);
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
                } else if self.active_gap.is_some() {
                    self.resolve_active_gap(ts_ms, true, outputs);
                }
            }
            StreamStatus::SignalLost | StreamStatus::Disconnected => {
                self.begin_or_tighten_gap(ts_ms, MEDIUM_GAP_MAX_MS);
            }
            StreamStatus::Connecting | StreamStatus::Acquiring | StreamStatus::PassiveStandby => {}
        }
    }

    fn handle_stream_disconnected(&mut self, ts_ms: i64, reason: StreamDisconnectReason) {
        let window_ms = match reason {
            StreamDisconnectReason::Remote => SHORT_GAP_MAX_MS,
            StreamDisconnectReason::Timeout => MEDIUM_GAP_MAX_MS,
        };
        self.begin_or_tighten_gap(ts_ms, window_ms);
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
            if self.active_gap.is_some() {
                self.resolve_active_gap(pkt.ts_ms, true, outputs);
            } else if let Some(prev_any) = self.last_any_packet_ts_ms
                && pkt.ts_ms - prev_any > IMPLICIT_SILENCE_GAP_MS
            {
                self.begin_or_tighten_gap(prev_any, MEDIUM_GAP_MAX_MS);
                self.resolve_active_gap(pkt.ts_ms, true, outputs);
            } else if let Some(prev_valid) = self.last_valid_packet_ts_ms
                && pkt.ts_ms - prev_valid > IMPLICIT_INVALID_GAP_MS
            {
                self.begin_or_tighten_gap(prev_valid, MEDIUM_GAP_MAX_MS);
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
                    let is_non_sleep_epoch = p_sleep < 0.30;
                    let is_sustained_wake_epoch = probs[STATE_ACTIVE_WAKE]
                        >= POST_SLEEP_ACTIVE_WAKE_THRESHOLD
                        || (is_non_sleep_epoch && features.z_hr >= POST_SLEEP_WAKE_Z_THRESHOLD);
                    let ln_r = hrv_valid.then_some(features.ln_rmssd_60s).flatten();
                    self.sleep_samples.record_epoch(
                        is_non_sleep_epoch,
                        features.hr_bpm_60s,
                        ln_r,
                        unclamped_var_ratio,
                    );
                    if !is_non_sleep_epoch {
                        self.post_trigger_sleep_sec =
                            self.post_trigger_sleep_sec.saturating_add(15);
                    }
                    if is_sustained_wake_epoch {
                        self.post_sleep_wake_sec += 15;
                    } else {
                        self.post_sleep_wake_sec = 0;
                    }

                    if self
                        .last_sleeping_desktop_activity_ms
                        .is_some_and(|act| ts_ms - act <= DESKTOP_REBOUND_WINDOW_MS)
                        && self.has_wake_rebound(features.hr_bpm_60s, features.z_hr, p_sleep)
                    {
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
        self.pre_candidate_mu_hr = self.baseline.mu_hr();
        self.pre_candidate_sigma_hr = Some(self.baseline.sigma_hr());
        self.pre_candidate_ln_rmssd = self.baseline.mu_ln_rmssd();
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
            self.begin_or_tighten_gap(prev_ts, MEDIUM_GAP_MAX_MS);
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
