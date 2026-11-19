pub mod clock;
pub mod state;
pub mod wearable;

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use thiserror::Error;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::{
    CliOverrides, DetectionConfig, LoadedConfig, PowerAction, RawSamplesRetention,
};
use crate::ipc::protocol::{EpochUpdateEvent, HrSampleEvent};
use crate::ipc::pubsub::EventBroker;
use crate::ipc::server::handle_connection;
use crate::logging::BiometricLogContext;
use crate::paths::{EnvSource, ResolvedPaths, ensure_private_dir};
use crate::sleep_engine::{
    DetectorState, EngineConfig, EngineEvent, EngineOutput, GraceOutcome, HrPacket,
    SessionEndReason, SleepEngine,
};
use crate::storage::{
    CloseSessionRequest, OpenSessionRequest, PersistenceWorker, StorageError, db_path_in_data_dir,
    lock_path_in_data_dir,
};

pub use clock::{DaemonClock, SystemClock, VirtualClock};
pub use state::{DaemonCore, DaemonCoreInit};
pub use wearable::{NullWearableSource, WearableDescriptor, WearableEvent, WearableSource};

/// Errors that prevent `bpmute daemon` from starting or binding its socket.
#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon is already running on socket {0}")]
    AlreadyRunning(PathBuf),
    #[error("socket path {0} exists and is not a Unix domain socket")]
    NonSocketConflict(PathBuf),
    #[error("daemon I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("ledger storage error: {0}")]
    Storage(#[from] StorageError),
}

enum EngineControlEvent {
    GraceOutcome(GraceOutcome),
}

/// Startup parameters for [`DaemonHandle::spawn`].
pub struct DaemonOptions {
    pub paths: ResolvedPaths,
    pub loaded_config: LoadedConfig,
    pub env: Arc<dyn EnvSource>,
    pub cli_overrides: CliOverrides,
    pub clock: Arc<dyn DaemonClock>,
    pub wearable_source: Option<Arc<dyn WearableSource>>,
    pub install_signals: bool,
}

/// Running background daemon instance with graceful shutdown and IPC control handles.
pub struct DaemonHandle {
    core: Arc<DaemonCore>,
    shutdown_tx: watch::Sender<bool>,
    ctrl_tx: mpsc::Sender<EngineControlEvent>,
    monitor_task: JoinHandle<()>,
    accept_task: JoinHandle<()>,
    persistence_worker: PersistenceWorker,
    socket_path: PathBuf,
}

impl DaemonHandle {
    /// Binds the `0600` Unix domain socket, starts the persistence worker and monitoring loop,
    /// and returns a handle.
    pub async fn spawn(opts: DaemonOptions) -> Result<Self, DaemonError> {
        let socket_path = opts.paths.socket_path.clone();
        if let Some(parent) = socket_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        prepare_socket_path(&socket_path).await?;
        let listener = UnixListener::bind(&socket_path)?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

        let db_path = db_path_in_data_dir(&opts.paths.data_dir);
        let lock_path = lock_path_in_data_dir(&opts.paths.data_dir);
        let startup_ms = opts.clock.now_utc().timestamp_millis();
        let (persistence_worker, persistence_handle, initial_calibration) =
            PersistenceWorker::spawn(&db_path, &lock_path, startup_ms)?;

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let broker = Arc::new(EventBroker::default());
        let initial_detection = opts.loaded_config.config.detection.clone();
        let initial_retention = opts.loaded_config.config.retention.raw_samples_days;

        let core = Arc::new(DaemonCore::new(DaemonCoreInit {
            paths: opts.paths,
            loaded_config: opts.loaded_config,
            env: opts.env,
            cli_overrides: opts.cli_overrides,
            clock: opts.clock,
            broker,
            shutdown_rx: shutdown_rx.clone(),
            persistence: persistence_handle,
            initial_calibration,
        }));

        let wearable_source = opts
            .wearable_source
            .unwrap_or_else(|| Arc::new(NullWearableSource));
        let wearable_rx = wearable_source.subscribe(shutdown_rx.clone());
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<EngineControlEvent>(32);

        let monitor_task = tokio::spawn(run_monitoring_loop(
            Arc::clone(&core),
            wearable_rx,
            ctrl_rx,
            shutdown_rx.clone(),
            initial_detection,
            initial_retention,
        ));

        let core_clone = Arc::clone(&core);
        let socket_for_task = socket_path.clone();
        let install_signals = opts.install_signals;

        let accept_task = tokio::spawn(async move {
            run_accept_loop(
                listener,
                core_clone,
                shutdown_rx,
                &socket_for_task,
                install_signals,
            )
            .await;
        });

        tracing::info!(socket = %socket_path.display(), "bpmute daemon listening");

        Ok(Self {
            core,
            shutdown_tx,
            ctrl_tx,
            monitor_task,
            accept_task,
            persistence_worker,
            socket_path,
        })
    }

    /// Returns the Unix domain socket path bound by this daemon.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Publishes an `HrSample` stream event through the drop-oldest broker.
    pub fn publish_hr_sample(&self, sample: Value) {
        self.core.publish_hr_sample(sample);
    }

    /// Injects a timestamped [`GraceOutcome`] event into the live sleep-onset engine.
    pub async fn deliver_grace_outcome(&self, outcome: GraceOutcome) {
        let _ = self
            .ctrl_tx
            .send(EngineControlEvent::GraceOutcome(outcome))
            .await;
    }

    /// Waits until the daemon exits (typically after receiving `SIGINT`/`SIGTERM`),
    /// then flushes the monitoring loop and shuts down the persistence worker cleanly.
    pub async fn wait(self) {
        let _ = self.accept_task.await;
        let _ = self.shutdown_tx.send(true);
        let _ = self.monitor_task.await;
        self.persistence_worker.shutdown_async().await;
    }

    /// Triggers graceful shutdown, drains monitoring and IPC connection tasks,
    /// shuts down the persistence worker, and cleans up the socket file.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.monitor_task.await;
        let _ = self.accept_task.await;
        self.persistence_worker.shutdown_async().await;
    }
}

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
        let engine = SleepEngine::new(EngineConfig {
            detection: detection_cfg.clone(),
            priors: cal.to_priors(),
            wearable_floor: Default::default(),
        });
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

    async fn handle_wearable_event(&mut self, ev: WearableEvent) {
        match ev {
            WearableEvent::StreamState { wearable, state } => {
                self.last_event_ts_ms = Some(state.ts_ms);
                let Some(wearable_id) = self.resolve_wearable(&wearable, state.ts_ms).await else {
                    return;
                };
                self.switch_wearable_if_needed(&wearable_id, state.ts_ms)
                    .await;
                let outputs = self.engine.handle_event(EngineEvent::StreamState(state));
                self.process_outputs(&wearable_id, outputs).await;
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
        let Some(wearable_id) = self.active_wearable_id.clone() else {
            return;
        };
        let outputs = match ctrl {
            EngineControlEvent::GraceOutcome(gr) => {
                self.last_event_ts_ms = Some(gr.ts_ms);
                self.engine.handle_event(EngineEvent::GraceOutcome(gr))
            }
        };
        self.process_outputs(&wearable_id, outputs).await;
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

    async fn process_outputs(&mut self, wearable_id: &str, outputs: Vec<EngineOutput>) {
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
                            self.active_session_id = Some(sid);
                            self.epoch_seq = 0;
                            self.raw_seq = 0;
                            self.pending_raw.clear();
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
                EngineOutput::StateChanged { to, .. } => {
                    self.core.set_detector_phase(
                        to,
                        self.active_session_id.clone(),
                        self.active_wearable_id.clone(),
                    );
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
                        match self
                            .core
                            .persistence
                            .close_session_and_recompute(
                                CloseSessionRequest {
                                    session_id: sid,
                                    summary,
                                    trigger_power_action: PowerAction::None,
                                    rapid_resume_within_5m: false,
                                },
                                close_now_ms,
                            )
                            .await
                        {
                            Ok((detail, cal_state)) => {
                                let floor = cal_state.wearable_floor(wearable_id);
                                self.engine.update_calibration(cal_state.to_priors(), floor);
                                self.core.on_session_closed(detail, cal_state);
                            }
                            Err(err) => {
                                tracing::error!(error = %err, "failed to close session and recompute calibration");
                            }
                        }
                    }
                    self.epoch_seq = 0;
                    self.raw_seq = 0;
                    self.core.set_detector_phase(
                        self.engine.state(),
                        None,
                        self.active_wearable_id.clone(),
                    );
                }
                EngineOutput::GraceAbortRequested { .. }
                | EngineOutput::FalsePositiveRecorded { .. }
                | EngineOutput::GapResolved { .. } => {}
            }
        }
    }
}

async fn run_monitoring_loop(
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
        tokio::select! {
            ev_opt = wearable_rx.recv(), if !wearable_closed => {
                match ev_opt {
                    Some(ev) => ctx.handle_wearable_event(ev).await,
                    None => {
                        wearable_closed = true;
                        ctx.flush_close_latest(SessionEndReason::StreamEnded).await;
                    }
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
            }
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    ctx.flush_close_latest(SessionEndReason::StreamEnded).await;
}

async fn prepare_socket_path(socket_path: &Path) -> Result<(), DaemonError> {
    match fs::symlink_metadata(socket_path) {
        Ok(meta) => {
            if !meta.file_type().is_socket() {
                return Err(DaemonError::NonSocketConflict(socket_path.to_path_buf()));
            }
            if UnixStream::connect(socket_path).await.is_ok() {
                return Err(DaemonError::AlreadyRunning(socket_path.to_path_buf()));
            }
            fs::remove_file(socket_path)?;
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(DaemonError::Io(err)),
    }
}

async fn run_accept_loop(
    listener: UnixListener,
    core: Arc<DaemonCore>,
    mut shutdown_rx: watch::Receiver<bool>,
    socket_path: &Path,
    install_signals: bool,
) {
    let mut sighup = if install_signals {
        signal(SignalKind::hangup()).ok()
    } else {
        None
    };
    let mut sigterm = if install_signals {
        signal(SignalKind::terminate()).ok()
    } else {
        None
    };
    let mut sigint = if install_signals {
        signal(SignalKind::interrupt()).ok()
    } else {
        None
    };

    let (conn_shutdown_tx, _) = watch::channel(false);
    let mut conn_tasks: Vec<JoinHandle<()>> = Vec::new();

    loop {
        tokio::select! {
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _addr)) => {
                        conn_tasks.retain(|h| !h.is_finished());
                        let c = Arc::clone(&core);
                        let rx = conn_shutdown_tx.subscribe();
                        conn_tasks.push(tokio::spawn(async move {
                            handle_connection(stream, c, rx).await;
                        }));
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "unix socket accept error");
                        break;
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            _ = async {
                match sighup {
                    Some(ref mut sig) => { let _ = sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("received SIGHUP; reloading configuration");
                let _ = core.reload_from_disk();
            }
            _ = async {
                match sigterm {
                    Some(ref mut sig) => { let _ = sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("received SIGTERM; shutting down daemon");
                break;
            }
            _ = async {
                match sigint {
                    Some(ref mut sig) => { let _ = sig.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("received SIGINT; shutting down daemon");
                break;
            }
        }
    }

    let _ = conn_shutdown_tx.send(true);
    for handle in conn_tasks {
        let _ = handle.await;
    }
    let _ = fs::remove_file(socket_path);
}
