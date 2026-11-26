mod alerts;
mod enrollment;
mod grace;
mod ipc;
mod schedule;

pub(super) use alerts::upsert_alert;
use alerts::{
    clear_reload_alert, sync_idle_alerts_locked, sync_shift_alert_locked, upsert_reload_alert,
};
pub use enrollment::{BluezEnrollmentDriver, EnrollmentPairingHook, NoopEnrollmentDriver};
use schedule::is_armed_locked;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Local};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::actions::grace::{GraceControlCmd, GraceCoordinator, GraceRunParams};
use crate::actions::pipeline::SleepActionPipeline;
use crate::actions::traits::{ActionExecutor, Notifier};
use crate::calibration::CalibrationState;
use crate::config::{
    CliOverrides, ConfigPatch, DetectionConfig, LoadedConfig, PowerAction, RawSamplesRetention,
    patch_config_file,
};
use crate::daemon::EngineControlEvent;
use crate::idle::IdleProbe;
use crate::ipc::protocol::{
    AlertChangedEvent, CalibrationUpdatedEvent, ConfigSnapshotPayload, DaemonAlert, DaemonStatus,
    DiscoveredWearable, EnrollAuthResponseParams, EnrolledWearableStatus, EpochUpdateEvent,
    ErrorPayload, HrSampleEvent, PauseMode, PushEventEnvelope, SessionClosedEvent,
    SessionDiscardedEvent, SessionStartedEvent, SnapshotEvent, WearablesUpdatedEvent, error_codes,
};
use crate::ipc::pubsub::{EventBroker, SubscriberQueue};
use crate::paths::{EnvSource, ResolvedPaths};
use crate::sleep_engine::{DetectorState, StreamStatus};
use crate::storage::worker::PersistenceHandle;
use crate::storage::{SessionFullDetail, StorageError};

use super::clock::DaemonClock;

const MAX_HR_RING_BUFFER_LEN: usize = 1_800; // 30 min at 1 Hz

pub(crate) fn ipc_err(code: &str, message: impl Into<String>) -> ErrorPayload {
    ErrorPayload {
        code: code.to_string(),
        message: message.into(),
    }
}

pub(crate) fn storage_to_ipc_err(err: StorageError) -> ErrorPayload {
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
    pub(super) session_close_deadline_ms: Option<i64>,
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
    pub(super) last_within_active_hours: Option<bool>,
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
        let last_within_active_hours =
            loaded_config.config.schedule.active_hours_enabled.then(|| {
                loaded_config
                    .config
                    .schedule
                    .is_within_active_hours(clock.now_local().time())
            });
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
                session_close_deadline_ms: None,
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
                last_within_active_hours,
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
        let norm = mac.trim().to_ascii_uppercase();
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard.wearable_statuses.insert(norm.clone(), status);
        if !status.is_connected() {
            guard.wearable_batteries.remove(&norm);
        }
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
        let connected = guard
            .wearable_statuses
            .get(&norm)
            .copied()
            .unwrap_or(StreamStatus::Disconnected)
            .is_connected();
        if connected && let Some(pct) = battery {
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
                let battery = if status.is_connected() {
                    guard.wearable_batteries.get(&norm).copied()
                } else {
                    None
                };
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

    /// Builds a `Snapshot` push envelope with current status, rolling 30-min HR ring buffer, alerts, and daemon clock timestamp.
    pub fn make_snapshot_event(&self, status: &DaemonStatus) -> PushEventEnvelope {
        let server_now_ms = self.clock.now_utc().timestamp_millis();
        let ring: Vec<HrSampleEvent> = {
            let guard = self.inner.lock().expect("daemon state mutex poisoned");
            guard.hr_ring_buffer.iter().cloned().collect()
        };
        let payload = SnapshotEvent {
            status: status.clone(),
            hr_ring_buffer: ring,
            alerts: status.alerts.clone(),
            server_now_ms: Some(server_now_ms),
        };
        self.broker.make_event(
            "Snapshot",
            serde_json::to_value(payload).unwrap_or_else(|_| json!({})),
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
        self.apply_detector_and_deadline(|guard| {
            let changed = guard.detector_state != detector_state;
            guard.detector_state = detector_state;
            guard.active_session_id = active_session_id;
            guard.active_wearable_id = active_wearable_id;
            changed
        });
    }

    /// Atomically updates detector state, active session/wearable IDs, and close deadline, publishing `StateChanged` if state or deadline changed.
    pub fn sync_detector_and_deadline(
        &self,
        detector_state: DetectorState,
        active_session_id: Option<String>,
        active_wearable_id: Option<String>,
        session_close_deadline_ms: Option<i64>,
    ) {
        self.apply_detector_and_deadline(|guard| {
            let changed = guard.detector_state != detector_state
                || guard.session_close_deadline_ms != session_close_deadline_ms;
            guard.detector_state = detector_state;
            guard.active_session_id = active_session_id;
            guard.active_wearable_id = active_wearable_id;
            guard.session_close_deadline_ms = session_close_deadline_ms;
            changed
        });
    }

    /// Updates the active session close deadline, publishing `StateChanged` if changed.
    pub fn set_session_close_deadline(&self, session_close_deadline_ms: Option<i64>) {
        self.apply_detector_and_deadline(|guard| {
            let changed = guard.session_close_deadline_ms != session_close_deadline_ms;
            guard.session_close_deadline_ms = session_close_deadline_ms;
            changed
        });
    }

    fn apply_detector_and_deadline(&self, mutate: impl FnOnce(&mut InnerState) -> bool) {
        let status_opt = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            mutate(&mut guard).then(|| self.build_status_locked(&guard))
        };
        if let Some(status) = status_opt {
            self.publish_state_changed(&status);
        }
    }

    /// Publishes a `SessionStarted` control event when a `Monitoring Session` opens in SQLite.
    pub fn publish_session_started(&self, payload: SessionStartedEvent) {
        if let Ok(val) = serde_json::to_value(payload) {
            self.broker.publish_control("SessionStarted", val);
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

    /// Publishes `SessionDiscarded` when an incomplete-Warmup session is removed on closure.
    pub fn on_session_discarded(&self, payload: SessionDiscardedEvent) {
        if let Ok(val) = serde_json::to_value(payload) {
            self.broker.publish_control("SessionDiscarded", val);
        }
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
            self.publish_alerts_changed(alerts);
        }
        if let Ok(val) = serde_json::to_value(CalibrationUpdatedEvent {
            calibration: new_cal,
        }) {
            self.broker.publish_control("CalibrationUpdated", val);
        }
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
                    self.publish_alerts_changed(alerts);
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
            self.publish_alerts_changed(alerts);
        }
        if let Ok(val) = serde_json::to_value(&snapshot) {
            self.broker.publish_control("ConfigUpdated", val);
        }
        self.check_active_hours_transition(self.clock.now_utc().timestamp_millis());
        if let Some(status) = state_changed_status {
            self.publish_state_changed(&status);
        }

        tracing::info!("{log_msg}");
        snapshot
    }

    pub(super) fn publish_state_changed(&self, status: &DaemonStatus) {
        if let Ok(val) = serde_json::to_value(status) {
            self.broker.publish_control("StateChanged", val);
        }
    }

    pub(super) fn publish_alerts_changed(&self, alerts: Vec<DaemonAlert>) {
        if let Ok(val) = serde_json::to_value(AlertChangedEvent { alerts }) {
            self.broker.publish_control("AlertChanged", val);
        }
    }

    /// Publishes a raw JSON `HrSample` stream event through the drop-oldest broker lane.
    pub fn publish_hr_sample(&self, sample: Value) {
        self.broker.publish_stream("HrSample", sample);
    }
}
