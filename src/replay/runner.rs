//! Read-only replay runner driving [`SleepEngine`] on parsed canonical traces.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{DetectionConfig, Sensitivity};
use crate::replay::trace::{ReplayError, TraceFormat, detect_trace_format, parse_trace_str};
use crate::sleep_engine::{
    CalibrationPriors, DetectorState, EngineConfig, EngineEvent, EngineOutput, Epoch, GraceOutcome,
    GraceResult, SessionEndReason, SleepEngine, WearableFloor,
};

/// Replay fidelity indicator (ADR-0007; `#17` will add `SessionEpochs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayFidelity {
    RawSamples,
}

impl ReplayFidelity {
    /// Returns the canonical lowercase identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RawSamples => "raw_samples",
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
    pub epochs_count: usize,
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
                    self.sessions.push(ReplaySessionSummary {
                        session_index: summary.session_index,
                        start_ts_ms: summary.start_ts_ms,
                        end_ts_ms: summary.end_ts_ms,
                        end_reason: Some(summary.end_reason),
                        candidate_ts_ms: summary.candidate_ts_ms,
                        confirmed_ts_ms: summary.confirmed_ts_ms,
                        sleeping_ts_ms: summary.sleeping_ts_ms,
                        anomalous: summary.anomalous,
                        fp_count: summary.fp_count,
                        warmup_mu_hr: summary.warmup_mu_hr,
                        epochs_count: summary.epochs_count,
                    });
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
        options.priors
    };

    let mut engine = SleepEngine::new(EngineConfig {
        detection,
        priors,
        wearable_floor: options.wearable_floor,
    });

    let grace_period_ms = i64::from(options.grace_period_sec) * 1000;
    let mut samples_processed = 0_usize;
    let mut acc = ReplayAccumulator::default();

    for ev in events {
        if let Some(conf_ts) = acc.pending_confirmed_since_ms {
            if matches!(ev, EngineEvent::GraceOutcome(_)) {
                acc.pending_confirmed_since_ms = None;
            } else if ev.ts_ms() - conf_ts >= grace_period_ms {
                acc.pending_confirmed_since_ms = None;
                let exp_ts = conf_ts + grace_period_ms;
                let out = engine.handle_event(EngineEvent::GraceOutcome(GraceOutcome {
                    ts_ms: exp_ts,
                    outcome: GraceResult::Expired,
                }));
                acc.record(out, &engine);
            }
        }

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

    if let Some(act) = acc.active_session.take() {
        acc.sessions.push(ReplaySessionSummary {
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
            epochs_count: act.epochs_count,
        });
    }

    ReplayReport {
        v: 1,
        source: source.to_string(),
        format,
        fidelity: ReplayFidelity::RawSamples,
        read_only: true,
        preset: effective_preset,
        cold_start: options.cold_start,
        samples_processed,
        events_processed: events.len(),
        epochs_emitted: acc.epochs.len(),
        sessions: acc.sessions,
        transitions: acc.transitions,
        epochs: acc.epochs,
    }
}
