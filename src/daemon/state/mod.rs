mod enrollment;
mod grace;
mod ipc;

pub use enrollment::{BluezEnrollmentDriver, EnrollmentPairingHook, NoopEnrollmentDriver};

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::actions::grace::{GraceControlCmd, GraceCoordinator, GraceRunParams};
use crate::actions::pipeline::SleepActionPipeline;
use crate::actions::traits::{ActionExecutor, Notifier};
use crate::calibration::CalibrationState;
use crate::config::{
    CliOverrides, ClockTime, ConfigPatch, DetectionConfig, LoadedConfig, PowerAction,
    RawSamplesRetention, patch_config_file,
};
use crate::daemon::EngineControlEvent;
use crate::idle::{IdleBackendKind, IdleProbe};
use crate::ipc::protocol::{
    AlertSeverity, CalibrationUpdatedEvent, ConfigSnapshotPayload, DaemonAlert, DaemonStatus,
    DiscoveredWearable, EnrollAuthResponseParams, EnrolledWearableStatus, EpochUpdateEvent,
    ErrorPayload, HrSampleEvent, MonitoringState, PROTOCOL_VERSION, PauseMode, PauseParams,
    PushEventEnvelope, SessionClosedEvent, WearablesUpdatedEvent, alert_ids, error_codes,
};
use crate::ipc::pubsub::{EventBroker, SubscriberQueue};
use crate::paths::{EnvSource, ResolvedPaths};
use crate::sleep_engine::{DetectorState, StreamStatus};
use crate::storage::worker::PersistenceHandle;
use crate::storage::{SessionFullDetail, StorageError};

use super::clock::DaemonClock;

const MAX_HR_RING_BUFFER_LEN: usize = 1_800; // 30 min at 1 Hz

pub(super) fn ipc_err(code: &str, message: impl Into<String>) -> ErrorPayload {
    ErrorPayload {
        code: code.to_string(),
        message: message.into(),
    }
}

pub(super) fn storage_to_ipc_err(err: StorageError) -> ErrorPayload {
    ipc_err(err.ipc_code(), err.to_string())
}

pub(super) fn serialize_json(value: impl Serialize) -> Result<Value, ErrorPayload> {
    serde_json::to_value(value).map_err(|e| ipc_err(error_codes::INTERNAL_ERROR, e.to_string()))
}

/// Shared mutable daemon runtime state behind a short-lived synchronous mutex.
pub struct DaemonCore {
    pub paths: ResolvedPaths,
    pub env: Arc<dyn EnvSource>,
    pub cli_overrides: CliOverrides,
    pub clock: Arc<dyn DaemonClock>,
    pub broker: Arc<EventBroker>,
    pub shutdown_rx: watch::Receiver<bool>,
    pub persistence: PersistenceHandle,
    pub action_executor: Arc<dyn ActionExecutor>,
    pub notifier: Arc<dyn Notifier>,
    pub idle_probe: Arc<dyn IdleProbe>,
    pub enrollment_driver: Arc<dyn EnrollmentPairingHook>,
    pub wearable_source: Arc<dyn crate::daemon::wearable::WearableSource>,
    pub pipeline: Arc<SleepActionPipeline>,
    pub coordinator: Arc<GraceCoordinator>,
    pub engine_ctrl_tx: mpsc::Sender<EngineControlEvent>,
    pub(super) active_grace_tx: Mutex<Option<mpsc::Sender<GraceControlCmd>>>,
    pub(super) active_grace_params: Mutex<Option<GraceRunParams>>,
    pub(super) active_discovery: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub sleep_triggered_at: Mutex<Option<i64>>,
    pub trigger_power_action: Mutex<PowerAction>,
    pub rapid_resume_within_5m: Mutex<bool>,
    calibration_tx: watch::Sender<CalibrationState>,
    config_tx: watch::Sender<(DetectionConfig, RawSamplesRetention)>,
    pub(super) inner: Mutex<InnerState>,
}

pub(super) struct InnerState {
    pub(super) loaded_config: LoadedConfig,
    pub(super) dry_run: bool,
    pub(super) paused: bool,
    pub(super) paused_until: Option<DateTime<Local>>,
    pub(super) pause_mode: Option<PauseMode>,
    pub(super) pause_generation: u64,
    pub(super) detector_state: DetectorState,
    pub(super) active_session_id: Option<String>,
    pub(super) active_wearable_id: Option<String>,
    pub(super) calibration_state: CalibrationState,
    pub(super) hr_ring_buffer: VecDeque<HrSampleEvent>,
    pub(super) alerts: Vec<DaemonAlert>,
    pub(super) started_at: tokio::time::Instant,
    pub(super) enrollment: Option<EnrollmentLock>,
    pub(super) enrollment_seq: u64,
    pub(super) enrollment_cancel_tx: Option<watch::Sender<bool>>,
    pub(super) discovered_wearables: Vec<DiscoveredWearable>,
    pub(super) wearable_statuses: HashMap<String, StreamStatus>,
    pub(super) wearable_batteries: HashMap<String, u8>,
    pub(super) wearable_body_locations: HashMap<String, String>,
    pub(super) playing_players_count: Option<usize>,
    pub(super) active_auth_reply: Option<tokio::sync::oneshot::Sender<EnrollAuthResponseParams>>,
}

pub(super) struct EnrollmentLock {
    pub(super) client_id: u64,
    pub(super) started_at_ms: i64,
    pub(super) timeout_ms: i64,
}

/// Initialization parameters for [`DaemonCore::new`].
pub struct DaemonCoreInit {
    pub paths: ResolvedPaths,
    pub loaded_config: LoadedConfig,
    pub env: Arc<dyn EnvSource>,
    pub cli_overrides: CliOverrides,
    pub clock: Arc<dyn DaemonClock>,
    pub broker: Arc<EventBroker>,
    pub shutdown_rx: watch::Receiver<bool>,
    pub persistence: PersistenceHandle,
    pub initial_calibration: CalibrationState,
    pub action_executor: Arc<dyn ActionExecutor>,
    pub notifier: Arc<dyn Notifier>,
    pub idle_probe: Arc<dyn IdleProbe>,
    pub enrollment_driver: Arc<dyn EnrollmentPairingHook>,
    pub wearable_source: Arc<dyn crate::daemon::wearable::WearableSource>,
    pub engine_ctrl_tx: mpsc::Sender<EngineControlEvent>,
}

impl DaemonCore {
    /// Initializes daemon state from validated startup configuration and persisted calibration state.
    pub fn new(init: DaemonCoreInit) -> Self {
        let DaemonCoreInit {
            paths,
            loaded_config,
            env,
            cli_overrides,
            clock,
            broker,
            shutdown_rx,
            persistence,
            initial_calibration,
            action_executor,
            notifier,
            idle_probe,
            enrollment_driver,
            wearable_source,
            engine_ctrl_tx,
        } = init;
        let dry_run = loaded_config.config.actions.dry_run;
        let started_at = clock.now_instant();
        let (calibration_tx, _) = watch::channel(initial_calibration.clone());
        let (config_tx, _) = watch::channel((
            loaded_config.config.detection.clone(),
            loaded_config.config.retention.raw_samples_days,
        ));
        let mut alerts = Vec::new();
        sync_shift_alert_locked(&mut alerts, &initial_calibration);
        sync_idle_alerts_locked(
            &mut alerts,
            idle_probe.backend_kind(),
            loaded_config.config.actions.power_action,
        );

        let pipeline = Arc::new(SleepActionPipeline::new(
            Arc::clone(&action_executor),
            Arc::clone(&notifier),
            Arc::clone(&clock),
        ));
        let coordinator = Arc::new(GraceCoordinator::new(
            Arc::clone(&notifier),
            Arc::clone(&idle_probe),
            Arc::clone(&clock),
            Arc::clone(&broker),
        ));

        Self {
            paths,
            env,
            cli_overrides,
            clock,
            broker,
            shutdown_rx,
            persistence,
            action_executor,
            notifier,
            idle_probe,
            enrollment_driver,
            wearable_source,
            pipeline,
            coordinator,
            engine_ctrl_tx,
            active_grace_tx: Mutex::new(None),
            active_grace_params: Mutex::new(None),
            active_discovery: tokio::sync::Mutex::new(None),
            sleep_triggered_at: Mutex::new(None),
            trigger_power_action: Mutex::new(PowerAction::None),
            rapid_resume_within_5m: Mutex::new(false),
            calibration_tx,
            config_tx,
            inner: Mutex::new(InnerState {
                loaded_config,
                dry_run,
                paused: false,
                paused_until: None,
                pause_mode: None,
                pause_generation: 0,
                detector_state: DetectorState::Idle,
                active_session_id: None,
                active_wearable_id: None,
                calibration_state: initial_calibration,
                hr_ring_buffer: VecDeque::with_capacity(MAX_HR_RING_BUFFER_LEN),
                alerts,
                started_at,
                enrollment: None,
                enrollment_seq: 0,
                enrollment_cancel_tx: None,
                discovered_wearables: Vec::new(),
                wearable_statuses: HashMap::new(),
                wearable_batteries: HashMap::new(),
                wearable_body_locations: HashMap::new(),
                playing_players_count: None,
                active_auth_reply: None,
            }),
        }
    }

    /// Subscribes to live calibration state updates (for the monitoring loop).
    pub fn subscribe_calibration(&self) -> watch::Receiver<CalibrationState> {
        self.calibration_tx.subscribe()
    }

    /// Subscribes to live detection/retention config updates (for the monitoring loop).
    pub fn subscribe_config(&self) -> watch::Receiver<(DetectionConfig, RawSamplesRetention)> {
        self.config_tx.subscribe()
    }

    /// Returns a snapshot of whether biometric telemetry logging is enabled.
    pub fn log_biometrics(&self) -> bool {
        self.inner
            .lock()
            .expect("daemon state mutex poisoned")
            .loaded_config
            .log_biometrics
    }

    /// Returns a snapshot of the current in-memory [`CalibrationState`].
    pub fn calibration_state(&self) -> CalibrationState {
        self.inner
            .lock()
            .expect("daemon state mutex poisoned")
            .calibration_state
            .clone()
    }

    /// Computes a consistent snapshot of `DaemonStatus`.
    pub fn status(&self) -> DaemonStatus {
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        self.build_status_locked(&guard)
    }

    pub(super) fn build_status_locked(&self, guard: &InnerState) -> DaemonStatus {
        let uptime_sec = self
            .clock
            .now_instant()
            .saturating_duration_since(guard.started_at)
            .as_secs();

        let armed = is_armed_locked(guard, self.clock.as_ref());
        let monitoring_state = if guard.paused {
            MonitoringState::Paused
        } else {
            MonitoringState::from(guard.detector_state)
        };

        DaemonStatus {
            v: PROTOCOL_VERSION,
            daemon_running: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            uptime_sec,
            monitoring_state,
            armed,
            paused: guard.paused,
            paused_until: guard.paused_until.map(|dt| dt.to_rfc3339()),
            pause_mode: guard.pause_mode,
            dry_run: guard.dry_run,
            schedule: guard.loaded_config.config.schedule.clone(),
            config_path: self.paths.config_file.display().to_string(),
            config_loaded_from_file: guard.loaded_config.file_present,
            socket_path: self.paths.socket_path.display().to_string(),
            log_biometrics: guard.loaded_config.log_biometrics,
            alerts: guard.alerts.clone(),
            idle_backend: Some(self.idle_probe.backend_kind().display_name().to_string()),
            playing_players_count: guard.playing_players_count,
        }
    }

    /// Probes active non-ignored MPRIS2 players side-effect-freely and stores the latest count.
    pub async fn refresh_playing_players_count(&self) {
        let ignore_players = {
            let guard = self.inner.lock().expect("daemon state mutex poisoned");
            guard.loaded_config.config.actions.ignore_players.clone()
        };
        let count = self
            .action_executor
            .count_playing_players(&ignore_players)
            .await
            .ok();
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard.playing_players_count = count;
    }

    /// Updates the live stream status for a wearable keyed by normalized MAC address.
    pub fn set_wearable_status(&self, mac: &str, status: StreamStatus) {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard
            .wearable_statuses
            .insert(mac.trim().to_ascii_uppercase(), status);
    }

    /// Updates cached battery and body sensor location metadata for a wearable.
    pub fn set_wearable_metadata(
        &self,
        mac: &str,
        battery: Option<u8>,
        body_location: Option<String>,
    ) {
        let norm = mac.trim().to_ascii_uppercase();
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        if let Some(pct) = battery {
            guard.wearable_batteries.insert(norm.clone(), pct);
        }
        if let Some(loc) = body_location {
            guard.wearable_body_locations.insert(norm, loc);
        }
    }

    /// Builds current enrolled wearable status snapshots by combining SQLite allowlist with runtime status/metadata.
    pub async fn build_wearable_statuses(
        &self,
    ) -> Result<Vec<EnrolledWearableStatus>, StorageError> {
        let enrolled = self.persistence.list_wearables().await?;
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        Ok(enrolled
            .into_iter()
            .map(|w| {
                let norm = w.mac.to_ascii_uppercase();
                let status = guard
                    .wearable_statuses
                    .get(&norm)
                    .copied()
                    .unwrap_or(StreamStatus::Disconnected);
                let battery = guard.wearable_batteries.get(&norm).copied();
                let body_location = guard.wearable_body_locations.get(&norm).cloned();
                EnrolledWearableStatus {
                    wearable: w,
                    status,
                    battery,
                    body_location,
                }
            })
            .collect())
    }

    /// Publishes `WearablesUpdated` to all subscribed IPC clients.
    pub async fn publish_wearables_updated(&self) {
        let Ok(wearables) = self.build_wearable_statuses().await else {
            return;
        };
        if let Ok(val) = serde_json::to_value(WearablesUpdatedEvent { wearables }) {
            self.broker.publish_control("WearablesUpdated", val);
        }
    }

    /// Populates discovered BlueZ wearables returned by `wearable.enroll_start` in integration tests.
    pub fn seed_discovered_wearables(&self, wearables: Vec<DiscoveredWearable>) {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard.discovered_wearables = wearables;
    }

    pub(super) fn build_config_snapshot_locked(&self, guard: &InnerState) -> ConfigSnapshotPayload {
        ConfigSnapshotPayload {
            config: guard.loaded_config.config.clone(),
            file_present: guard.loaded_config.file_present,
            config_path: self.paths.config_file.display().to_string(),
            log_filter: guard.loaded_config.log_filter.clone(),
            log_format: guard.loaded_config.log_format,
            log_biometrics: guard.loaded_config.log_biometrics,
        }
    }

    /// Registers a subscriber and builds its initial `Snapshot` push event.
    pub fn subscribe(&self) -> (u64, Arc<SubscriberQueue>, DaemonStatus) {
        let status = self.status();
        let (sub_id, queue) = self.broker.subscribe();
        (sub_id, queue, status)
    }

    /// Builds a `Snapshot` push envelope with current status, rolling 30-min HR ring buffer, and alerts.
    pub fn make_snapshot_event(&self, status: &DaemonStatus) -> PushEventEnvelope {
        let ring: Vec<HrSampleEvent> = {
            let guard = self.inner.lock().expect("daemon state mutex poisoned");
            guard.hr_ring_buffer.iter().cloned().collect()
        };
        self.broker.make_event(
            "Snapshot",
            json!({
                "status": status,
                "hr_ring_buffer": ring,
                "alerts": status.alerts,
            }),
        )
    }

    /// Records a live `HrSampleEvent` into the 30-minute ring buffer and pushes `HrSample`.
    pub fn record_hr_sample(&self, sample: HrSampleEvent) {
        {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if guard.hr_ring_buffer.len() >= MAX_HR_RING_BUFFER_LEN {
                guard.hr_ring_buffer.pop_front();
            }
            guard.hr_ring_buffer.push_back(sample.clone());
        }
        if let Ok(val) = serde_json::to_value(sample) {
            self.broker.publish_stream("HrSample", val);
        }
    }

    /// Updates active session/wearable tracking and detector state, publishing `StateChanged` if changed.
    pub fn set_detector_phase(
        &self,
        detector_state: DetectorState,
        active_session_id: Option<String>,
        active_wearable_id: Option<String>,
    ) {
        let status_opt = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            let changed = guard.detector_state != detector_state;
            guard.detector_state = detector_state;
            guard.active_session_id = active_session_id;
            guard.active_wearable_id = active_wearable_id;
            changed.then(|| self.build_status_locked(&guard))
        };
        if let Some(status) = status_opt {
            self.publish_state_changed(&status);
        }
    }

    /// Publishes an `EpochUpdate` control event for a recorded 15-second `Epoch`.
    pub fn publish_epoch_update(&self, payload: EpochUpdateEvent) {
        if let Ok(val) = serde_json::to_value(payload) {
            self.broker.publish_control("EpochUpdate", val);
        }
    }

    /// Publishes `SessionClosed` and refreshes calibration state + shift alerts.
    pub fn on_session_closed(&self, detail: SessionFullDetail, cal_state: CalibrationState) {
        if let Ok(val) = serde_json::to_value(SessionClosedEvent { session: detail }) {
            self.broker.publish_control("SessionClosed", val);
        }
        self.apply_calibration_update(cal_state);
    }

    /// Updates in-memory calibration state, syncs shift alerts, notifies the engine loop, and pushes IPC events.
    pub fn apply_calibration_update(&self, new_cal: CalibrationState) {
        let alerts_opt = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            guard.calibration_state = new_cal.clone();
            let changed = sync_shift_alert_locked(&mut guard.alerts, &new_cal);
            changed.then(|| guard.alerts.clone())
        };

        let _ = self.calibration_tx.send(new_cal.clone());

        if let Some(alerts) = alerts_opt {
            self.broker
                .publish_control("AlertChanged", json!({ "alerts": alerts }));
        }
        if let Ok(val) = serde_json::to_value(CalibrationUpdatedEvent {
            calibration: new_cal,
        }) {
            self.broker.publish_control("CalibrationUpdated", val);
        }
    }

    pub(super) fn pause(
        self: &Arc<Self>,
        params: PauseParams,
    ) -> Result<DaemonStatus, ErrorPayload> {
        let mode = params.mode.unwrap_or(PauseMode::Indefinite);
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");

        let (paused_until, sleep_duration) = match mode {
            PauseMode::Indefinite => {
                if params.duration_sec.is_some() {
                    return Err(ipc_err(
                        error_codes::INVALID_PARAMS,
                        "duration_sec is only valid with mode = \"duration\"",
                    ));
                }
                (None, None)
            }
            PauseMode::Duration => {
                let secs = params.duration_sec.ok_or_else(|| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        "duration_sec is required when mode = \"duration\"",
                    )
                })?;
                if secs == 0 {
                    return Err(ipc_err(
                        error_codes::INVALID_PARAMS,
                        "duration_sec must be >= 1",
                    ));
                }
                let secs_i64 = i64::try_from(secs).map_err(|_| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        "duration_sec exceeds supported range",
                    )
                })?;
                let now = self.clock.now_local();
                let until = now + chrono::Duration::seconds(secs_i64);
                (Some(until), Some(Duration::from_secs(secs)))
            }
            PauseMode::UntilMorning => {
                if params.duration_sec.is_some() {
                    return Err(ipc_err(
                        error_codes::INVALID_PARAMS,
                        "duration_sec is not valid with mode = \"until_morning\"",
                    ));
                }
                let now_local = self.clock.now_local();
                let target_clock = if guard.loaded_config.config.schedule.active_hours_enabled {
                    guard.loaded_config.config.schedule.end
                } else {
                    ClockTime::new(8, 0).expect("08:00 is valid")
                };
                let target_local = target_clock.next_occurrence_after(now_local);
                let delta = target_local.with_timezone(&Utc) - now_local.with_timezone(&Utc);
                let wait = delta.to_std().unwrap_or(Duration::from_secs(1));
                (Some(target_local), Some(wait))
            }
        };

        guard.paused = true;
        guard.paused_until = paused_until;
        guard.pause_mode = Some(mode);
        guard.pause_generation = guard.pause_generation.wrapping_add(1);
        let gen_id = guard.pause_generation;
        let status = self.build_status_locked(&guard);
        drop(guard);

        tracing::info!(?mode, paused_until = ?status.paused_until, "monitoring paused");
        self.publish_state_changed(&status);

        if let Some(wait) = sleep_duration {
            self.spawn_auto_resume_timer(gen_id, wait);
        }

        Ok(status)
    }

    fn spawn_auto_resume_timer(self: &Arc<Self>, gen_id: u64, wait: Duration) {
        let core = Arc::clone(self);
        let mut shutdown_rx = self.shutdown_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(wait) => {
                    let status_opt = {
                        let mut guard = core.inner.lock().expect("daemon state mutex poisoned");
                        if guard.pause_generation == gen_id && guard.paused {
                            guard.paused = false;
                            guard.paused_until = None;
                            guard.pause_mode = None;
                            Some(core.build_status_locked(&guard))
                        } else {
                            None
                        }
                    };
                    if let Some(status) = status_opt {
                        tracing::info!("monitoring pause expired; resuming automatically");
                        core.publish_state_changed(&status);
                    }
                }
                _ = shutdown_rx.changed() => {}
            }
        });
    }

    pub(super) fn resume(&self) -> DaemonStatus {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        let was_paused = guard.paused;
        guard.paused = false;
        guard.paused_until = None;
        guard.pause_mode = None;
        guard.pause_generation = guard.pause_generation.wrapping_add(1);
        let status = self.build_status_locked(&guard);
        drop(guard);

        if was_paused {
            tracing::info!("monitoring resumed");
            self.publish_state_changed(&status);
        }
        status
    }

    pub(super) fn set_dry_run(&self, dry_run: bool) -> DaemonStatus {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        let changed = guard.dry_run != dry_run;
        guard.dry_run = dry_run;
        let status = self.build_status_locked(&guard);
        drop(guard);

        if changed {
            tracing::info!(dry_run, "updated runtime dry_run mode");
            self.publish_state_changed(&status);
        }
        status
    }

    pub(super) fn update_config(
        &self,
        patch: &ConfigPatch,
    ) -> Result<ConfigSnapshotPayload, ErrorPayload> {
        let updated = patch_config_file(
            &self.paths.config_file,
            patch,
            self.env.as_ref(),
            &self.cli_overrides,
        )
        .map_err(|err| ipc_err(error_codes::INVALID_CONFIG, err.to_string()))?;

        let explicit_dry_run_patch = patch.actions.as_ref().and_then(|a| a.dry_run).is_some();
        let snapshot = self.apply_new_config(
            updated,
            explicit_dry_run_patch,
            "configuration updated and reloaded live",
        );
        Ok(snapshot)
    }

    /// Reloads `config.toml` from disk, preserving the last valid configuration on error.
    pub fn reload_from_disk(&self) -> Result<ConfigSnapshotPayload, ErrorPayload> {
        match LoadedConfig::load(
            &self.paths.config_file,
            self.env.as_ref(),
            &self.cli_overrides,
        ) {
            Ok(reloaded) => Ok(self.apply_new_config(reloaded, false, "configuration reloaded")),
            Err(err) => {
                let err_msg = err.to_string();
                let alerts_opt = {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    let changed = upsert_reload_alert(&mut guard.alerts, &err_msg);
                    changed.then(|| guard.alerts.clone())
                };

                if let Some(alerts) = alerts_opt {
                    self.broker
                        .publish_control("AlertChanged", json!({ "alerts": alerts }));
                }

                tracing::error!(
                    error = %err_msg,
                    "configuration reload failed; preserving last valid in-memory config"
                );
                Err(ipc_err(error_codes::INVALID_CONFIG, err_msg))
            }
        }
    }

    fn apply_new_config(
        &self,
        new_loaded: LoadedConfig,
        explicit_dry_run_patch: bool,
        log_msg: &str,
    ) -> ConfigSnapshotPayload {
        let (snapshot, state_changed_status, alerts_changed, cfg_watch) = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            let old_armed = is_armed_locked(&guard, self.clock.as_ref());
            let old_dry_run = guard.dry_run;
            let old_cfg_dry_run = guard.loaded_config.config.actions.dry_run;

            guard.loaded_config = new_loaded;
            let new_cfg_dry_run = guard.loaded_config.config.actions.dry_run;
            if (explicit_dry_run_patch && self.cli_overrides.dry_run.is_none())
                || new_cfg_dry_run != old_cfg_dry_run
            {
                guard.dry_run = new_cfg_dry_run;
            }

            let cleared_alert = clear_reload_alert(&mut guard.alerts);
            let power_action = guard.loaded_config.config.actions.power_action;
            let idle_alerts_changed = sync_idle_alerts_locked(
                &mut guard.alerts,
                self.idle_probe.backend_kind(),
                power_action,
            );
            let alerts_opt = (cleared_alert || idle_alerts_changed).then(|| guard.alerts.clone());

            let new_armed = is_armed_locked(&guard, self.clock.as_ref());
            let status_opt = (old_armed != new_armed || old_dry_run != guard.dry_run)
                .then(|| self.build_status_locked(&guard));

            let snapshot = self.build_config_snapshot_locked(&guard);
            let cfg_watch = (
                guard.loaded_config.config.detection.clone(),
                guard.loaded_config.config.retention.raw_samples_days,
            );
            (snapshot, status_opt, alerts_opt, cfg_watch)
        };

        let _ = self.config_tx.send(cfg_watch);
        let retention_cfg = snapshot.config.retention.clone();
        let persistence = self.persistence.clone();
        tokio::spawn(async move {
            let _ = persistence.set_retention_config(retention_cfg).await;
        });

        if let Some(alerts) = alerts_changed {
            self.broker
                .publish_control("AlertChanged", json!({ "alerts": alerts }));
        }
        if let Ok(val) = serde_json::to_value(&snapshot) {
            self.broker.publish_control("ConfigUpdated", val);
        }
        if let Some(status) = state_changed_status {
            self.publish_state_changed(&status);
        }

        tracing::info!("{log_msg}");
        snapshot
    }

    fn publish_state_changed(&self, status: &DaemonStatus) {
        if let Ok(val) = serde_json::to_value(status) {
            self.broker.publish_control("StateChanged", val);
        }
    }

    /// Publishes a raw JSON `HrSample` stream event through the drop-oldest broker lane.
    pub fn publish_hr_sample(&self, sample: Value) {
        self.broker.publish_stream("HrSample", sample);
    }
}

fn is_armed_locked(guard: &InnerState, clock: &dyn DaemonClock) -> bool {
    if guard.paused {
        return false;
    }
    if guard.loaded_config.config.schedule.active_hours_enabled {
        return guard
            .loaded_config
            .config
            .schedule
            .is_within_active_hours(clock.now_local().time());
    }
    true
}

fn clear_reload_alert(alerts: &mut Vec<DaemonAlert>) -> bool {
    let before = alerts.len();
    alerts.retain(|a| a.id != alert_ids::CONFIG_RELOAD_FAILED);
    alerts.len() != before
}

fn upsert_reload_alert(alerts: &mut Vec<DaemonAlert>, err_msg: &str) -> bool {
    let message = format!("Configuration reload failed (keeping last valid config): {err_msg}");
    upsert_alert(
        alerts,
        DaemonAlert {
            id: alert_ids::CONFIG_RELOAD_FAILED.to_string(),
            kind: alert_ids::CONFIG_RELOAD_FAILED.to_string(),
            severity: AlertSeverity::Error,
            message,
            dismissible: true,
        },
    )
}

fn sync_shift_alert_locked(alerts: &mut Vec<DaemonAlert>, state: &CalibrationState) -> bool {
    if let Some(ref shift) = state.pending_shift_alert {
        let message = format!(
            "Persistent {} baseline shift detected across {} consecutive sessions; accept or dismiss via calibration controls",
            shift.direction, shift.session_count
        );
        upsert_alert(
            alerts,
            DaemonAlert {
                id: alert_ids::PERSISTENT_SHIFT.to_string(),
                kind: alert_ids::PERSISTENT_SHIFT.to_string(),
                severity: AlertSeverity::Warn,
                message,
                dismissible: true,
            },
        )
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != alert_ids::PERSISTENT_SHIFT);
        alerts.len() != before
    }
}

fn sync_idle_alerts_locked(
    alerts: &mut Vec<DaemonAlert>,
    backend: IdleBackendKind,
    power_action: PowerAction,
) -> bool {
    let mut changed = false;

    let needs_degraded = backend.is_degraded()
        && matches!(power_action, PowerAction::Suspend | PowerAction::Poweroff);
    if needs_degraded {
        changed |= upsert_alert(
            alerts,
            DaemonAlert {
                id: alert_ids::DEGRADED_IDLE.to_string(),
                kind: alert_ids::DEGRADED_IDLE.to_string(),
                severity: AlertSeverity::Warn,
                message: "No desktop idle backend available. Destructive power actions run in degraded mode with a 90s Grace Period safety floor.".to_string(),
                dismissible: false,
            },
        );
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != alert_ids::DEGRADED_IDLE);
        changed |= alerts.len() != before;
    }

    if backend.is_inhibitor_sensitive() {
        changed |= upsert_alert(
            alerts,
            DaemonAlert {
                id: alert_ids::INHIBITOR_SENSITIVE.to_string(),
                kind: alert_ids::INHIBITOR_SENSITIVE.to_string(),
                severity: AlertSeverity::Info,
                message: "Idle detection using Wayland ext-idle-notify-v1 v1 (inhibitor-sensitive: media playback idle inhibitors may suppress idle detection).".to_string(),
                dismissible: false,
            },
        );
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != alert_ids::INHIBITOR_SENSITIVE);
        changed |= alerts.len() != before;
    }

    changed
}

pub(super) fn upsert_alert(alerts: &mut Vec<DaemonAlert>, next: DaemonAlert) -> bool {
    if let Some(existing) = alerts.iter_mut().find(|a| a.id == next.id) {
        if *existing == next {
            return false;
        }
        *existing = next;
        return true;
    }
    alerts.push(next);
    true
}
