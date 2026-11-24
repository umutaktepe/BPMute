//! Live `Monitoring Session` execution and background sleep-onset loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::config::{DetectionConfig, RawSamplesRetention};
use crate::daemon::EngineControlEvent;
use crate::daemon::state::{DaemonCore, ipc_err, storage_to_ipc_err};
use crate::daemon::wearable::{WearableDescriptor, WearableEvent};
use crate::ipc::protocol::{
    EpochUpdateEvent, ErrorPayload, HrSampleEvent, SessionStartedEvent, error_codes,
};
use crate::logging::BiometricLogContext;
use crate::sleep_engine::{
    DetectorState, EngineConfig, EngineEvent, EngineOutput, GraceAbortReason, HrPacket,
    ManualCloseError, SessionEndReason, SleepEngine, StreamDisconnected, StreamStatus,
};
use crate::storage::{CloseSessionRequest, OpenSessionRequest, SessionFullDetail, StorageError};

struct MonitoringContext {
    core: Arc<DaemonCore>,
    engine: SleepEngine,
    detection_cfg: DetectionConfig,
    raw_samples_enabled: bool,
    wearable_cache: HashMap<String, (String, WearableDescriptor)>,
    active_wearable_id: Option<String>,
    active_session_id: Option<String>,
    epoch_seq: u32,
    raw_seq: u32,
    pending_raw: Vec<(u32, HrPacket)>,
    last_event_ts_ms: Option<i64>,
}

impl MonitoringContext {
    fn new(
        core: Arc<DaemonCore>,
        detection_cfg: DetectionConfig,
        raw_retention: RawSamplesRetention,
    ) -> Self {
        let cal = core.calibration_state();
        let within_active = core.is_within_active_hours();
        let mut engine = SleepEngine::new(EngineConfig {
            detection: detection_cfg.clone(),
            priors: cal.to_priors(),
            wearable_floor: Default::default(),
        });
        engine.set_active_hours(within_active);
        Self {
            core,
            engine,
            detection_cfg,
            raw_samples_enabled: !matches!(raw_retention, RawSamplesRetention::Disabled),
            wearable_cache: HashMap::new(),
            active_wearable_id: None,
            active_session_id: None,
            epoch_seq: 0,
            raw_seq: 0,
            pending_raw: Vec::new(),
            last_event_ts_ms: None,
        }
    }

    async fn resolve_wearable(&mut self, desc: &WearableDescriptor, ts_ms: i64) -> Option<String> {
        let key = desc.mac.trim().to_ascii_uppercase();
        if let Some((id, cached_desc)) = self.wearable_cache.get(&key)
            && cached_desc == desc
        {
            return Some(id.clone());
        }

        let now_ms = self.core.clock.now_utc().timestamp_millis().max(ts_ms);
        match self
            .core
            .persistence
            .upsert_wearable(desc.clone(), now_ms)
            .await
        {
            Ok(id) => {
                self.wearable_cache.insert(key, (id.clone(), desc.clone()));
                Some(id)
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to persist wearable descriptor");
                None
            }
        }
    }

    async fn switch_wearable_if_needed(&mut self, wearable_id: &str, ts_ms: i64) {
        if self.active_wearable_id.as_deref() == Some(wearable_id) {
            return;
        }
        if let Some(prev_wearable_id) = self.active_wearable_id.take()
            && self.engine.state() != DetectorState::Idle
        {
            let outputs = self
                .engine
                .force_close_session(ts_ms, SessionEndReason::StreamEnded);
            self.process_outputs(&prev_wearable_id, outputs).await;
        }

        self.active_wearable_id = Some(wearable_id.to_string());
        let cal = self.core.calibration_state();
        self.engine
            .update_calibration(cal.to_priors(), cal.wearable_floor(wearable_id));
    }

    async fn process_stream_transition(
        &mut self,
        wearable: &WearableDescriptor,
        status: StreamStatus,
        ts_ms: i64,
        engine_event: EngineEvent,
    ) {
        self.core.set_wearable_status(&wearable.mac, status);
        self.core.publish_wearables_updated().await;
        if status == StreamStatus::PassiveStandby {
            return;
        }
        self.last_event_ts_ms = Some(ts_ms);
        let Some(wearable_id) = self.resolve_wearable(wearable, ts_ms).await else {
            return;
        };
        self.switch_wearable_if_needed(&wearable_id, ts_ms).await;
        if status == StreamStatus::SignalLost || status == StreamStatus::Disconnected {
            self.core.signal_signal_lost().await;
        }
        let outputs = self.engine.handle_event(engine_event);
        self.process_outputs(&wearable_id, outputs).await;
    }

    async fn transition_active_hours_entered(&mut self, ts_ms: i64) {
        self.last_event_ts_ms = Some(ts_ms);
        self.engine.set_active_hours(true);
        if self.engine.state() == DetectorState::Standby
            && let Some(wearable_id) = self.active_wearable_id.clone()
        {
            let outputs = self
                .engine
                .open_session_from_standby(ts_ms, "active hours started");
            self.process_outputs(&wearable_id, outputs).await;
        }
    }

    async fn transition_active_hours_expired(&mut self, ts_ms: i64) {
        self.last_event_ts_ms = Some(ts_ms);
        self.engine.set_active_hours(false);
        if (self.engine.state() == DetectorState::Monitoring
            || self.engine.state() == DetectorState::Warmup
            || self.engine.state() == DetectorState::Candidate)
            && let Some(wearable_id) = self.active_wearable_id.clone()
        {
            let outputs = self.engine.close_active_hours_expired(ts_ms);
            self.process_outputs(&wearable_id, outputs).await;
        }
    }

    async fn evaluate_active_hours(&mut self, ts_ms: i64) {
        let within_active = self.core.is_within_active_hours();
        let prev_within = self.engine.within_active_hours();
        if prev_within != within_active {
            if !prev_within && within_active {
                self.transition_active_hours_entered(ts_ms).await;
            } else if prev_within && !within_active {
                self.transition_active_hours_expired(ts_ms).await;
            }
        }
    }

    async fn handle_wearable_event(&mut self, ev: WearableEvent) {
        let event_ts = match &ev {
            WearableEvent::StreamState { state, .. } => Some(state.ts_ms),
            WearableEvent::StreamDisconnected { ts_ms, .. } => Some(*ts_ms),
            WearableEvent::HrPacket { packet, .. } => Some(packet.ts_ms),
            WearableEvent::Metadata { .. } => None,
        };
        if let Some(ts_ms) = event_ts {
            self.evaluate_active_hours(ts_ms).await;
        }

        match ev {
            WearableEvent::StreamState { wearable, state } => {
                self.process_stream_transition(
                    &wearable,
                    state.status,
                    state.ts_ms,
                    EngineEvent::StreamState(state),
                )
                .await;
            }
            WearableEvent::StreamDisconnected {
                wearable,
                ts_ms,
                reason,
            } => {
                self.process_stream_transition(
                    &wearable,
                    StreamStatus::Disconnected,
                    ts_ms,
                    EngineEvent::StreamDisconnected(StreamDisconnected { ts_ms, reason }),
                )
                .await;
            }
            WearableEvent::Metadata {
                wearable,
                battery,
                body_location,
            } => {
                self.core
                    .set_wearable_metadata(&wearable.mac, battery, body_location);
                self.core.publish_wearables_updated().await;
            }
            WearableEvent::HrPacket { wearable, packet } => {
                self.last_event_ts_ms = Some(packet.ts_ms);
                let Some(wearable_id) = self.resolve_wearable(&wearable, packet.ts_ms).await else {
                    return;
                };
                self.switch_wearable_if_needed(&wearable_id, packet.ts_ms)
                    .await;

                let log_ctx = BiometricLogContext {
                    log_biometrics: self.core.log_biometrics(),
                };
                let rr_u16: Vec<u16> = packet
                    .rr_ms
                    .iter()
                    .map(|v| v.clamp(0.0, f64::from(u16::MAX)).round() as u16)
                    .collect();
                log_ctx.log_telemetry_sample(
                    wearable.alias.as_deref(),
                    &wearable.mac,
                    packet.bpm,
                    &rr_u16,
                );

                let outputs = self
                    .engine
                    .handle_event(EngineEvent::HrPacket(packet.clone()));
                self.process_outputs(&wearable_id, outputs).await;

                if self.raw_samples_enabled && self.active_session_id.is_some() {
                    let seq = self.raw_seq;
                    self.raw_seq = self.raw_seq.saturating_add(1);
                    self.pending_raw.push((seq, packet.clone()));
                }

                self.core.record_hr_sample(HrSampleEvent {
                    ts_ms: packet.ts_ms,
                    bpm: packet.bpm,
                    rr_ms: packet.rr_ms,
                    contact_status: packet.contact_status,
                    wearable_id,
                    session_id: self.active_session_id.clone(),
                });
            }
        }
    }

    async fn handle_control_event(&mut self, ctrl: EngineControlEvent) {
        match ctrl {
            EngineControlEvent::GraceOutcome(gr) => {
                let Some(wearable_id) = self.active_wearable_id.clone() else {
                    return;
                };
                self.last_event_ts_ms = Some(gr.ts_ms);
                let outputs = self.engine.handle_event(EngineEvent::GraceOutcome(gr));
                self.process_outputs(&wearable_id, outputs).await;
            }
            EngineControlEvent::HardWipe { wipe_all, reply } => {
                self.abort_active_session(wipe_all);
                let _ = reply.send(());
            }
            EngineControlEvent::ManualCloseSession { reply } => {
                let res = self.manual_close_active_session().await;
                let _ = reply.send(res);
            }
            EngineControlEvent::ActiveHoursEntered { ts_ms } => {
                self.transition_active_hours_entered(ts_ms).await;
            }
            EngineControlEvent::ActiveHoursExpired { ts_ms } => {
                self.transition_active_hours_expired(ts_ms).await;
            }
        }
    }

    async fn manual_close_active_session(&mut self) -> Result<SessionFullDetail, ErrorPayload> {
        if self.active_session_id.is_none() {
            return Err(ipc_err(
                error_codes::PRECONDITION_FAILED,
                "no active monitoring session to close",
            ));
        }
        let wearable_id = self.active_wearable_id.clone().unwrap_or_default();
        let outputs = match self.engine.manual_close_session() {
            Ok(outputs) => outputs,
            Err(ManualCloseError::NoActiveSession) => {
                return Err(ipc_err(
                    error_codes::PRECONDITION_FAILED,
                    "no active monitoring session to close",
                ));
            }
            Err(ManualCloseError::StreamingActive) => {
                return Err(ipc_err(
                    error_codes::PRECONDITION_FAILED,
                    "cannot manually close session while wearable is streaming; disconnect wearable or stop heart rate broadcast first",
                ));
            }
        };

        self.core.signal_arousal_abort().await;

        match self.process_outputs(&wearable_id, outputs).await {
            Some(Ok(detail)) => Ok(detail),
            Some(Err(err)) => Err(storage_to_ipc_err(err)),
            None => Err(ipc_err(
                error_codes::INTERNAL_ERROR,
                "session closure did not produce detail",
            )),
        }
    }

    fn abort_active_session(&mut self, wipe_all: bool) {
        self.active_session_id = None;
        self.epoch_seq = 0;
        self.raw_seq = 0;
        self.pending_raw.clear();
        self.last_event_ts_ms = None;
        if wipe_all {
            self.wearable_cache.clear();
            self.active_wearable_id = None;
        }
        let cal = self.core.calibration_state();
        let floor = self
            .active_wearable_id
            .as_deref()
            .map(|id| cal.wearable_floor(id))
            .unwrap_or_default();
        let within_active = self.core.is_within_active_hours();
        let mut engine = SleepEngine::new(EngineConfig {
            detection: self.detection_cfg.clone(),
            priors: cal.to_priors(),
            wearable_floor: floor,
        });
        engine.set_active_hours(within_active);
        self.engine = engine;
        self.core.sync_detector_and_deadline(
            DetectorState::Idle,
            None,
            self.active_wearable_id.clone(),
            None,
        );
    }

    async fn flush_close_latest(&mut self, reason: SessionEndReason) {
        if self.engine.state() == DetectorState::Idle {
            return;
        }
        let close_ts = self
            .last_event_ts_ms
            .unwrap_or_else(|| self.core.clock.now_utc().timestamp_millis());
        let wearable_id = self.active_wearable_id.clone().unwrap_or_default();
        let outputs = self.engine.force_close_session(close_ts, reason);
        self.process_outputs(&wearable_id, outputs).await;
    }

    fn gap_deadline_instant(&self) -> Option<tokio::time::Instant> {
        let deadline_ms = self.engine.gap_deadline_ms()?;
        let now_ms = self.core.clock.now_utc().timestamp_millis();
        let remaining_ms = deadline_ms.saturating_sub(now_ms);
        Some(self.core.clock.now_instant() + Duration::from_millis(remaining_ms.max(0) as u64))
    }

    async fn check_gap_timeout(&mut self, now_ms: i64) {
        if self.engine.state() == DetectorState::Idle {
            return;
        }
        let wearable_id = self.active_wearable_id.clone().unwrap_or_default();
        let outputs = self.engine.check_gap_timeout(now_ms);
        self.process_outputs(&wearable_id, outputs).await;
    }

    async fn process_outputs(
        &mut self,
        wearable_id: &str,
        outputs: Vec<EngineOutput>,
    ) -> Option<Result<SessionFullDetail, StorageError>> {
        let mut closed_result = None;
        for output in outputs {
            match output {
                EngineOutput::SessionStarted { ts_ms, .. } => {
                    match self
                        .core
                        .persistence
                        .open_session(OpenSessionRequest {
                            wearable_id: wearable_id.to_string(),
                            start_ts_ms: ts_ms,
                            detection_sensitivity: self.detection_cfg.sensitivity,
                        })
                        .await
                    {
                        Ok(sid) => {
                            self.active_session_id = Some(sid.clone());
                            self.epoch_seq = 0;
                            self.raw_seq = 0;
                            self.pending_raw.clear();
                            self.core.sync_detector_and_deadline(
                                self.engine.state(),
                                self.active_session_id.clone(),
                                self.active_wearable_id.clone(),
                                self.engine.gap_deadline_ms(),
                            );
                            self.core.publish_session_started(SessionStartedEvent {
                                session_id: sid,
                                wearable_id: wearable_id.to_string(),
                                start_ts_ms: ts_ms,
                            });
                        }
                        Err(err) => {
                            tracing::error!(error = %err, "failed to open monitoring session in SQLite");
                        }
                    }
                }
                EngineOutput::EpochEmitted(ep) => {
                    if let Some(ref sid) = self.active_session_id {
                        let seq = self.epoch_seq;
                        self.epoch_seq = self.epoch_seq.saturating_add(1);
                        let raw_batch = std::mem::take(&mut self.pending_raw);
                        if let Err(err) = self
                            .core
                            .persistence
                            .append_epoch_and_raw(sid.clone(), Some((seq, ep.clone())), raw_batch)
                            .await
                        {
                            tracing::error!(error = %err, "failed to persist session epoch");
                        }
                        self.core.publish_epoch_update(EpochUpdateEvent {
                            session_id: sid.clone(),
                            wearable_id: wearable_id.to_string(),
                            epoch: ep,
                        });
                    }
                }
                EngineOutput::StateChanged { to, ts_ms, .. } => {
                    self.core.sync_detector_and_deadline(
                        to,
                        self.active_session_id.clone(),
                        self.active_wearable_id.clone(),
                        self.engine.gap_deadline_ms(),
                    );
                    if to == DetectorState::Confirmed {
                        self.core.handle_confirmed_sleep_onset(ts_ms).await;
                    }
                }
                EngineOutput::SessionEnded(summary) => {
                    if let Some(sid) = self.active_session_id.take() {
                        let remaining_raw = std::mem::take(&mut self.pending_raw);
                        if !remaining_raw.is_empty()
                            && let Err(err) = self
                                .core
                                .persistence
                                .append_epoch_and_raw(sid.clone(), None, remaining_raw)
                                .await
                        {
                            tracing::error!(error = %err, "failed to flush trailing raw samples");
                        }

                        let close_now_ms = self
                            .core
                            .clock
                            .now_utc()
                            .timestamp_millis()
                            .max(summary.end_ts_ms);
                        let trigger_power = self.core.take_trigger_power_action();
                        let is_rapid = self.core.take_rapid_resume();
                        match self
                            .core
                            .persistence
                            .close_session_slim_and_recompute(
                                CloseSessionRequest {
                                    session_id: sid,
                                    summary,
                                    trigger_power_action: trigger_power,
                                    rapid_resume_within_5m: is_rapid,
                                },
                                close_now_ms,
                            )
                            .await
                        {
                            Ok((detail, cal_state)) => {
                                let floor = cal_state.wearable_floor(wearable_id);
                                self.engine.update_calibration(cal_state.to_priors(), floor);
                                self.core.on_session_closed(detail.clone(), cal_state);
                                closed_result = Some(Ok(detail));
                            }
                            Err(err) => {
                                tracing::error!(error = %err, "failed to close session and recompute calibration");
                                closed_result = Some(Err(err));
                            }
                        }
                    }
                    self.epoch_seq = 0;
                    self.raw_seq = 0;
                    self.core.sync_detector_and_deadline(
                        self.engine.state(),
                        None,
                        self.active_wearable_id.clone(),
                        self.engine.gap_deadline_ms(),
                    );
                }
                EngineOutput::GraceAbortRequested { reason, .. } => {
                    if reason == GraceAbortReason::PhysiologicalArousal {
                        self.core.signal_arousal_abort().await;
                    }
                }
                EngineOutput::FalsePositiveRecorded { .. } | EngineOutput::GapResolved { .. } => {}
            }
        }
        self.core
            .set_session_close_deadline(self.engine.gap_deadline_ms());
        closed_result
    }
}

async fn drain_control_and_close(
    ctx: &mut MonitoringContext,
    ctrl_rx: &mut mpsc::Receiver<EngineControlEvent>,
) {
    while let Ok(ctrl) = ctrl_rx.try_recv() {
        ctx.handle_control_event(ctrl).await;
    }
    ctx.flush_close_latest(SessionEndReason::StreamEnded).await;
}

async fn sleep_or_pending(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn run_monitoring_loop(
    core: Arc<DaemonCore>,
    mut wearable_rx: mpsc::Receiver<WearableEvent>,
    mut ctrl_rx: mpsc::Receiver<EngineControlEvent>,
    mut shutdown_rx: watch::Receiver<bool>,
    initial_detection: DetectionConfig,
    initial_retention: RawSamplesRetention,
) {
    let mut cal_rx = core.subscribe_calibration();
    let mut cfg_rx = core.subscribe_config();
    let mut ctx = MonitoringContext::new(core, initial_detection, initial_retention);
    let mut wearable_closed = false;

    loop {
        if *shutdown_rx.borrow() {
            break;
        }
        let gap_deadline = ctx.gap_deadline_instant();
        tokio::select! {
            biased;
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            Some(ctrl) = ctrl_rx.recv() => {
                ctx.handle_control_event(ctrl).await;
            }
            Ok(()) = cal_rx.changed() => {
                let cal = cal_rx.borrow_and_update().clone();
                let floor = ctx
                    .active_wearable_id
                    .as_deref()
                    .map(|id| cal.wearable_floor(id))
                    .unwrap_or_default();
                ctx.engine.update_calibration(cal.to_priors(), floor);
            }
            Ok(()) = cfg_rx.changed() => {
                let (det, ret) = cfg_rx.borrow_and_update().clone();
                ctx.detection_cfg = det.clone();
                ctx.raw_samples_enabled = !matches!(ret, RawSamplesRetention::Disabled);
                ctx.engine.update_detection_config(det);
                let within_active = ctx.core.is_within_active_hours();
                ctx.engine.set_active_hours(within_active);
            }
            _ = sleep_or_pending(gap_deadline) => {
                let now_ms = ctx.core.clock.now_utc().timestamp_millis();
                ctx.check_gap_timeout(now_ms).await;
            }
            ev_opt = wearable_rx.recv(), if !wearable_closed => {
                match ev_opt {
                    Some(ev) => ctx.handle_wearable_event(ev).await,
                    None => {
                        wearable_closed = true;
                        ctx.flush_close_latest(SessionEndReason::StreamEnded).await;
                    }
                }
            }
        }
    }

    drain_control_and_close(&mut ctx, &mut ctrl_rx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use tempfile::TempDir;

    use crate::actions::fake::{FakeActionExecutor, FakeNotifier};
    use crate::config::{CliOverrides, LoadedConfig};
    use crate::daemon::state::DaemonCoreInit;
    use crate::daemon::wearable::{NullWearableSource, WearableDescriptor};
    use crate::daemon::{EventBroker, NoopEnrollmentDriver, VirtualClock};
    use crate::idle::fake::FakeIdleProbe;
    use crate::paths::{MapEnv, ResolvedPaths};
    use crate::sleep_engine::{ContactStatus, GraceOutcome, GraceResult, StreamState};
    use crate::storage::{
        LedgerStore, PersistenceWorker, db_path_in_data_dir, lock_path_in_data_dir,
    };

    #[tokio::test(start_paused = true)]
    async fn shutdown_drains_queued_grace_outcome_before_closing_session() {
        let temp = TempDir::new().unwrap();
        let paths = ResolvedPaths {
            config_file: temp.path().join("cfg/bpmute/config.toml"),
            data_dir: temp.path().join("data/bpmute"),
            socket_path: temp.path().join("run/bpmute/daemon.sock"),
        };
        let env = Arc::new(MapEnv::new());
        let cli = CliOverrides::default();
        let mut loaded = LoadedConfig::load(&paths.config_file, env.as_ref(), &cli).unwrap();
        loaded.config.actions.pause_media = false;
        loaded.config.actions.lock_screen = true;
        loaded.config.actions.grace_period_sec = Some(300);
        let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
        let base_ts = start_utc.timestamp_millis();
        let clock = Arc::new(VirtualClock::starting_at(start_utc));
        let db_path = db_path_in_data_dir(&paths.data_dir);
        let lock_path = lock_path_in_data_dir(&paths.data_dir);
        let (worker, persistence, initial_calibration) =
            PersistenceWorker::spawn(&db_path, &lock_path, base_ts).unwrap();

        let (ctrl_tx, mut ctrl_rx) = mpsc::channel(32);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let core = Arc::new(DaemonCore::new(DaemonCoreInit {
            paths: paths.clone(),
            loaded_config: loaded,
            env,
            cli_overrides: cli,
            clock,
            broker: Arc::new(EventBroker::default()),
            shutdown_rx,
            persistence,
            initial_calibration,
            action_executor: Arc::new(FakeActionExecutor::new()),
            notifier: Arc::new(FakeNotifier::new()),
            idle_probe: Arc::new(FakeIdleProbe::default()),
            enrollment_driver: Arc::new(NoopEnrollmentDriver),
            wearable_source: Arc::new(NullWearableSource),
            engine_ctrl_tx: ctrl_tx.clone(),
        }));

        let mut ctx = MonitoringContext::new(
            Arc::clone(&core),
            DetectionConfig::default(),
            RawSamplesRetention::Disabled,
        );
        let wearable = WearableDescriptor {
            mac: "AA:BB:CC:DD:EE:11".to_string(),
            address_type: "public".to_string(),
            alias: Some("Polar".to_string()),
            advertised_name: Some("Polar H10".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        ctx.handle_wearable_event(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                ts_ms: base_ts,
                status: StreamStatus::Streaming,
            },
        })
        .await;

        // Drive SleepEngine through warmup (>= 8 min) and sleep onset until Confirmed.
        for sec in 0..(10 * 60) {
            let wave = 2.6 * (sec as f64 * 0.11).sin() + 1.1 * (sec as f64 * 0.31).cos();
            let bpm = 68.0 + wave;
            let rr = 60_000.0 / bpm + 14.0 * (sec as f64 * 1.5).sin();
            ctx.handle_wearable_event(WearableEvent::HrPacket {
                wearable: wearable.clone(),
                packet: HrPacket {
                    ts_ms: base_ts + sec * 1_000,
                    bpm,
                    rr_ms: vec![rr],
                    contact_status: ContactStatus::Contact,
                },
            })
            .await;
        }
        for sec in (10 * 60)..(30 * 60) {
            let mins = (sec - 10 * 60) as f64 / 60.0;
            let target = (68.0 - 0.8 * mins).max(56.0);
            let bpm = target + 0.35 * (sec as f64 * 0.4).sin();
            let rr = 60_000.0 / bpm + 26.0 * (sec as f64 * 1.5).sin();
            ctx.handle_wearable_event(WearableEvent::HrPacket {
                wearable: wearable.clone(),
                packet: HrPacket {
                    ts_ms: base_ts + sec * 1_000,
                    bpm,
                    rr_ms: vec![rr],
                    contact_status: ContactStatus::Contact,
                },
            })
            .await;
            if ctx.engine.state() == DetectorState::Confirmed {
                break;
            }
        }
        assert_eq!(ctx.engine.state(), DetectorState::Confirmed);

        let expired_ts = base_ts + 30 * 60 * 1_000;
        ctrl_tx
            .send(EngineControlEvent::GraceOutcome(GraceOutcome {
                ts_ms: expired_ts,
                outcome: GraceResult::Expired,
            }))
            .await
            .unwrap();

        drain_control_and_close(&mut ctx, &mut ctrl_rx).await;
        worker.shutdown_async().await;

        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        let page = store.list_sessions(10, 0).unwrap();
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.sessions[0].sleeping_ts_ms, Some(expired_ts));
    }
}
