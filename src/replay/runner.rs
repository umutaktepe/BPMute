//! Read-only replay runner driving [`SleepEngine`] on parsed canonical traces.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{DetectionConfig, Sensitivity};
use crate::replay::trace::{ReplayError, TraceFormat, detect_trace_format, parse_trace_str};
use crate::sleep_engine::{
    CalibrationPriors, DetectorState, EngineConfig, EngineEvent, EngineOutput, Epoch, GraceOutcome,
    GraceResult, SessionEndReason, SessionSummary, SleepEngine, StreamState, StreamStatus,
    WearableFloor,
};
use crate::storage::LedgerStore;

/// Replay fidelity indicator (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayFidelity {
    RawSamples,
    SessionEpochs,
}

impl ReplayFidelity {
    /// Returns the canonical lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RawSamples => "raw_samples",
            Self::SessionEpochs => "session_epochs",
        }
    }
}

/// Options controlling read-only file replay execution.
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub preset: Option<Sensitivity>,
    pub cold_start: bool,
    pub grace_period_sec: u32,
    pub priors: CalibrationPriors,
    pub wearable_floor: WearableFloor,
}

/// State transition recorded during replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayTransition {
    pub session_index: u64,
    pub ts_ms: i64,
    pub from: DetectorState,
    pub to: DetectorState,
    pub p_sleep: f64,
    pub reason: String,
}

/// Summary of an opened (and possibly closed) `Monitoring Session` during replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplaySessionSummary {
    pub session_index: u64,
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub end_reason: Option<SessionEndReason>,
    pub candidate_ts_ms: Option<i64>,
    pub confirmed_ts_ms: Option<i64>,
    pub sleeping_ts_ms: Option<i64>,
    pub anomalous: bool,
    pub fp_count: u8,
    pub warmup_mu_hr: Option<f64>,
    #[serde(default)]
    pub warmup_sigma_hr: Option<f64>,
    #[serde(default)]
    pub onset_hr_bpm: Option<f64>,
    #[serde(default)]
    pub sleep_mu_hr: Option<f64>,
    #[serde(default)]
    pub observed_hr_var_ratio: Option<f64>,
    #[serde(default)]
    pub post_trigger_sleep_sec: u32,
    #[serde(default)]
    pub grace_cancel_count: u8,
    #[serde(default)]
    pub rapid_wake_fp_count: u8,
    #[serde(default)]
    pub snooze_count: u8,
    pub epochs_count: usize,
}

impl ReplaySessionSummary {
    fn from_engine_summary(s: SessionSummary, end_reason: Option<SessionEndReason>) -> Self {
        Self {
            session_index: s.session_index,
            start_ts_ms: s.start_ts_ms,
            end_ts_ms: s.end_ts_ms,
            end_reason,
            candidate_ts_ms: s.candidate_ts_ms,
            confirmed_ts_ms: s.confirmed_ts_ms,
            sleeping_ts_ms: s.sleeping_ts_ms,
            anomalous: s.anomalous,
            fp_count: s.fp_count,
            warmup_mu_hr: s.warmup_mu_hr,
            warmup_sigma_hr: s.warmup_sigma_hr,
            onset_hr_bpm: s.onset_hr_bpm,
            sleep_mu_hr: s.sleep_mu_hr,
            observed_hr_var_ratio: s.observed_hr_var_ratio,
            post_trigger_sleep_sec: s.post_trigger_sleep_sec,
            grace_cancel_count: s.grace_cancel_count,
            rapid_wake_fp_count: s.rapid_wake_fp_count,
            snooze_count: s.snooze_count,
            epochs_count: s.epochs_count,
        }
    }
}

/// Structured output report emitted by `bpmute replay`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayReport {
    pub v: u32,
    pub source: String,
    pub format: TraceFormat,
    pub fidelity: ReplayFidelity,
    pub read_only: bool,
    pub preset: Sensitivity,
    pub cold_start: bool,
    pub samples_processed: usize,
    pub events_processed: usize,
    pub epochs_emitted: usize,
    pub sessions: Vec<ReplaySessionSummary>,
    pub transitions: Vec<ReplayTransition>,
    pub epochs: Vec<Epoch>,
}

struct ActiveSessionTracker {
    session_index: u64,
    start_ts_ms: i64,
    last_ts_ms: i64,
    candidate_ts_ms: Option<i64>,
    confirmed_ts_ms: Option<i64>,
    sleeping_ts_ms: Option<i64>,
    anomalous: bool,
    fp_count: u8,
    warmup_mu_hr: Option<f64>,
    epochs_count: usize,
}

#[derive(Default)]
struct ReplayAccumulator {
    pending_confirmed_since_ms: Option<i64>,
    epochs: Vec<Epoch>,
    transitions: Vec<ReplayTransition>,
    sessions: Vec<ReplaySessionSummary>,
    active_session: Option<ActiveSessionTracker>,
}

impl ReplayAccumulator {
    fn record(&mut self, outputs: Vec<EngineOutput>, engine_ref: &SleepEngine) {
        for output in outputs {
            match output {
                EngineOutput::SessionStarted {
                    session_index,
                    ts_ms,
                } => {
                    self.active_session = Some(ActiveSessionTracker {
                        session_index,
                        start_ts_ms: ts_ms,
                        last_ts_ms: ts_ms,
                        candidate_ts_ms: None,
                        confirmed_ts_ms: None,
                        sleeping_ts_ms: None,
                        anomalous: false,
                        fp_count: 0,
                        warmup_mu_hr: None,
                        epochs_count: 0,
                    });
                }
                EngineOutput::EpochEmitted(ep) => {
                    if let Some(ref mut act) = self.active_session {
                        act.last_ts_ms = ep.ts_ms;
                        act.epochs_count += 1;
                        act.anomalous = engine_ref.is_session_anomalous();
                        if act.warmup_mu_hr.is_none() || ep.state == DetectorState::Warmup {
                            act.warmup_mu_hr = Some(ep.baseline_mu_hr);
                        }
                    }
                    self.epochs.push(ep);
                }
                EngineOutput::StateChanged {
                    session_index,
                    ts_ms,
                    from,
                    to,
                    p_sleep,
                    reason,
                } => {
                    if let Some(ref mut act) = self.active_session {
                        act.last_ts_ms = ts_ms;
                        match to {
                            DetectorState::Candidate if act.candidate_ts_ms.is_none() => {
                                act.candidate_ts_ms = Some(ts_ms);
                            }
                            DetectorState::Confirmed => {
                                if act.confirmed_ts_ms.is_none() {
                                    act.confirmed_ts_ms = Some(ts_ms);
                                }
                                self.pending_confirmed_since_ms = Some(ts_ms);
                            }
                            DetectorState::Sleeping => {
                                if act.sleeping_ts_ms.is_none() {
                                    act.sleeping_ts_ms = Some(ts_ms);
                                }
                                self.pending_confirmed_since_ms = None;
                            }
                            DetectorState::Monitoring
                            | DetectorState::Idle
                            | DetectorState::Warmup => {
                                if from == DetectorState::Confirmed {
                                    self.pending_confirmed_since_ms = None;
                                }
                                if to == DetectorState::Monitoring
                                    && matches!(
                                        from,
                                        DetectorState::Candidate
                                            | DetectorState::Confirmed
                                            | DetectorState::Sleeping
                                    )
                                {
                                    act.candidate_ts_ms = None;
                                    act.confirmed_ts_ms = None;
                                    if from == DetectorState::Sleeping {
                                        act.sleeping_ts_ms = None;
                                    }
                                }
                            }
                            DetectorState::Candidate => {}
                        }
                    }
                    self.transitions.push(ReplayTransition {
                        session_index,
                        ts_ms,
                        from,
                        to,
                        p_sleep,
                        reason,
                    });
                }
                EngineOutput::FalsePositiveRecorded { counted, .. } => {
                    if counted && let Some(ref mut act) = self.active_session {
                        act.fp_count = act.fp_count.saturating_add(1);
                    }
                }
                EngineOutput::SessionEnded(summary) => {
                    self.pending_confirmed_since_ms = None;
                    self.active_session = None;
                    let end_reason = Some(summary.end_reason);
                    self.sessions
                        .push(ReplaySessionSummary::from_engine_summary(
                            summary, end_reason,
                        ));
                }
                EngineOutput::GraceAbortRequested { .. } | EngineOutput::GapResolved { .. } => {}
            }
        }
    }
}

/// Reads and replays a canonical `CSV` or `JSONL` trace file strictly read-only.
pub fn replay_file(
    path: &Path,
    base_detection: DetectionConfig,
    options: ReplayOptions,
) -> Result<ReplayReport, ReplayError> {
    let content = fs::read_to_string(path)?;
    let format = detect_trace_format(path, &content);
    let events = parse_trace_str(&content, format)?;
    Ok(replay_events(
        &path.display().to_string(),
        format,
        &events,
        base_detection,
        options,
    ))
}

/// Replays parsed [`EngineEvent`] items strictly read-only through [`SleepEngine`].
pub fn replay_events(
    source: &str,
    format: TraceFormat,
    events: &[EngineEvent],
    base_detection: DetectionConfig,
    options: ReplayOptions,
) -> ReplayReport {
    let (mut engine, effective_preset, cold_start, grace_period_ms) =
        build_replay_engine(base_detection, &options);
    let mut samples_processed = 0_usize;
    let mut acc = ReplayAccumulator::default();

    for ev in events {
        expire_pending_grace_if_due(
            &mut engine,
            &mut acc,
            ev.ts_ms(),
            grace_period_ms,
            matches!(ev, EngineEvent::GraceOutcome(_)),
        );

        if matches!(ev, EngineEvent::HrPacket(_)) {
            samples_processed += 1;
        }
        let is_desktop_during_grace = matches!(ev, EngineEvent::DesktopActivity(_))
            && engine.state() == DetectorState::Confirmed;

        let out = engine.handle_event(ev.clone());
        acc.record(out, &engine);

        if is_desktop_during_grace && engine.state() == DetectorState::Confirmed {
            acc.pending_confirmed_since_ms = None;
            let cancel_out = engine.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms: ev.ts_ms(),
                outcome: GraceResult::Cancelled,
            }));
            acc.record(cancel_out, &engine);
        }
    }

    finalize_replay_report(
        ReplayReportMeta {
            source,
            format,
            fidelity: ReplayFidelity::RawSamples,
            preset: effective_preset,
            cold_start,
            samples_processed,
            events_processed: events.len(),
        },
        &engine,
        acc,
    )
}

/// Replays stored 15-second `Epoch` snapshots strictly read-only when `raw_samples` are absent.
pub fn replay_session_epochs(
    source: &str,
    epochs: &[Epoch],
    base_detection: DetectionConfig,
    options: ReplayOptions,
) -> ReplayReport {
    let (mut engine, effective_preset, cold_start, grace_period_ms) =
        build_replay_engine(base_detection, &options);
    let mut acc = ReplayAccumulator::default();

    for ep in epochs {
        expire_pending_grace_if_due(&mut engine, &mut acc, ep.ts_ms, grace_period_ms, false);
        let out = engine.feed_epoch_features(
            ep.ts_ms,
            ep.features.clone(),
            ep.hrv_valid,
            ep.coverage,
            ep.flatline_stale,
        );
        acc.record(out, &engine);
    }

    finalize_replay_report(
        ReplayReportMeta {
            source,
            format: TraceFormat::SqliteLedger,
            fidelity: ReplayFidelity::SessionEpochs,
            preset: effective_preset,
            cold_start,
            samples_processed: 0,
            events_processed: epochs.len(),
        },
        &engine,
        acc,
    )
}

/// Replays a stored `Monitoring Session` strictly read-only from `db_path`, preferring retained
/// 1 Hz `raw_samples` and transparently falling back to permanent 15-second `session_epochs`.
pub fn replay_stored_session(
    db_path: &Path,
    session_id: &str,
    base_detection: DetectionConfig,
    preset: Option<Sensitivity>,
    cold_start: bool,
    grace_period_sec: u32,
) -> Result<ReplayReport, ReplayError> {
    let store = LedgerStore::open_read_only(db_path)?.ok_or_else(|| {
        ReplayError::NotFound(format!(
            "ledger database not found at {}; record a Monitoring Session first",
            db_path.display()
        ))
    })?;
    let bundle = store.load_session_for_replay(session_id)?;
    let source = format!("session:{}", bundle.session.id);
    let options = ReplayOptions {
        preset,
        cold_start,
        grace_period_sec,
        priors: bundle.priors,
        wearable_floor: bundle.wearable_floor,
    };

    if !bundle.raw_samples.is_empty() {
        let mut events = Vec::with_capacity(bundle.raw_samples.len() + 1);
        if bundle.raw_samples[0].ts_ms > bundle.session.start_ts_ms {
            events.push(EngineEvent::StreamState(StreamState {
                ts_ms: bundle.session.start_ts_ms,
                status: StreamStatus::Streaming,
            }));
        }
        events.extend(bundle.raw_samples.into_iter().map(EngineEvent::HrPacket));
        Ok(replay_events(
            &source,
            TraceFormat::SqliteLedger,
            &events,
            base_detection,
            options,
        ))
    } else if !bundle.epochs.is_empty() {
        Ok(replay_session_epochs(
            &source,
            &bundle.epochs,
            base_detection,
            options,
        ))
    } else {
        Err(ReplayError::NoReplayData(format!(
            "session '{}' contains neither raw_samples nor session_epochs to replay",
            bundle.session.id
        )))
    }
}

fn build_replay_engine(
    base_detection: DetectionConfig,
    options: &ReplayOptions,
) -> (SleepEngine, Sensitivity, bool, i64) {
    let detection = match options.preset {
        Some(preset) => DetectionConfig {
            sensitivity: preset,
            advanced: None,
        },
        None => base_detection,
    };
    let effective_preset = detection.sensitivity;
    let priors = if options.cold_start {
        CalibrationPriors::cold_start()
    } else {
        options.priors.clone()
    };

    let engine = SleepEngine::new(EngineConfig {
        detection,
        priors,
        wearable_floor: options.wearable_floor,
    });
    let grace_period_ms = i64::from(options.grace_period_sec) * 1000;
    (
        engine,
        effective_preset,
        options.cold_start,
        grace_period_ms,
    )
}

fn expire_pending_grace_if_due(
    engine: &mut SleepEngine,
    acc: &mut ReplayAccumulator,
    event_ts_ms: i64,
    grace_period_ms: i64,
    is_explicit_grace: bool,
) {
    if let Some(conf_ts) = acc.pending_confirmed_since_ms {
        if is_explicit_grace {
            acc.pending_confirmed_since_ms = None;
        } else if event_ts_ms - conf_ts >= grace_period_ms {
            acc.pending_confirmed_since_ms = None;
            let exp_ts = conf_ts + grace_period_ms;
            let out = engine.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms: exp_ts,
                outcome: GraceResult::Expired,
            }));
            acc.record(out, engine);
        }
    }
}

struct ReplayReportMeta<'a> {
    source: &'a str,
    format: TraceFormat,
    fidelity: ReplayFidelity,
    preset: Sensitivity,
    cold_start: bool,
    samples_processed: usize,
    events_processed: usize,
}

fn finalize_replay_report(
    meta: ReplayReportMeta<'_>,
    engine: &SleepEngine,
    mut acc: ReplayAccumulator,
) -> ReplayReport {
    if let Some(act) = acc.active_session.take() {
        let summary = if let Some(snapshot) =
            engine.snapshot_active_summary(act.last_ts_ms, SessionEndReason::StreamEnded)
        {
            ReplaySessionSummary::from_engine_summary(snapshot, None)
        } else {
            ReplaySessionSummary {
                session_index: act.session_index,
                start_ts_ms: act.start_ts_ms,
                end_ts_ms: act.last_ts_ms,
                end_reason: None,
                candidate_ts_ms: act.candidate_ts_ms,
                confirmed_ts_ms: act.confirmed_ts_ms,
                sleeping_ts_ms: act.sleeping_ts_ms,
                anomalous: act.anomalous,
                fp_count: act.fp_count,
                warmup_mu_hr: act.warmup_mu_hr,
                warmup_sigma_hr: None,
                onset_hr_bpm: None,
                sleep_mu_hr: None,
                observed_hr_var_ratio: None,
                post_trigger_sleep_sec: 0,
                grace_cancel_count: 0,
                rapid_wake_fp_count: 0,
                snooze_count: 0,
                epochs_count: act.epochs_count,
            }
        };
        acc.sessions.push(summary);
    }

    ReplayReport {
        v: 1,
        source: meta.source.to_string(),
        format: meta.format,
        fidelity: meta.fidelity,
        read_only: true,
        preset: meta.preset,
        cold_start: meta.cold_start,
        samples_processed: meta.samples_processed,
        events_processed: meta.events_processed,
        epochs_emitted: acc.epochs.len(),
        sessions: acc.sessions,
        transitions: acc.transitions,
        epochs: acc.epochs,
    }
}
