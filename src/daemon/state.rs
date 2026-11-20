use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use crate::actions::grace::{GraceControlCmd, GraceCoordinator, GraceRunOutcome, GraceRunParams};
use crate::actions::pipeline::{PipelineContext, SleepActionPipeline};
use crate::actions::traits::{ActionExecutor, BoxFuture, Notifier, PowerActionResult};
use crate::bluetooth::agent::{AgentAuthRequest, ENROLLMENT_AGENT_PATH, EnrollmentAgent};
use crate::bluetooth::dbus::AgentManager1Proxy;
use crate::calibration::CalibrationState;
use crate::config::{
    CliOverrides, ClockTime, ConfigPatch, DetectionConfig, LoadedConfig, PowerAction,
    RawSamplesRetention, patch_config_file,
};
use crate::idle::{
    IdleBackendKind, IdleProbe, compute_effective_grace_period, is_power_action_refused,
};
use crate::ipc::protocol::{
    AlertSeverity, CalibrationSetBoundaryParams, CalibrationUpdatedEvent, ConfigSnapshotPayload,
    DaemonAlert, DaemonStatus, DataBackupSqliteParams, DataExportBundleParams, DataHardWipeParams,
    DataImportBundleParams, DiscoveredWearable, EnrollAuthResponseParams, EnrollSelectParams,
    EnrollStartParams, EnrolledWearableStatus, EnrollmentUpdateEvent, EpochUpdateEvent,
    ErrorPayload, HrSampleEvent, MonitoringState, PROTOCOL_VERSION, PairingPrompt, PauseMode,
    PauseParams, PushEventEnvelope, SessionClosedEvent, SessionDeleteParams,
    SessionExportTraceParams, SessionGetParams, SessionListParams, SetDryRunParams,
    SetFeedbackParams, SetSeedParams, WearableConnectParams, WearableRenameParams,
    WearableUnenrollParams, alert_ids, error_codes,
};
use crate::ipc::pubsub::{EventBroker, SubscriberQueue};
use crate::logging::mask_mac;
use crate::paths::{EnvSource, ResolvedPaths};
use crate::sleep_engine::{DetectorState, GraceOutcome, GraceResult, StreamStatus};
use crate::storage::ImportMode;
use crate::storage::worker::PersistenceHandle;
use crate::storage::{SessionFullDetail, SetFeedbackRequest, StorageError, WearableDescriptor};

use super::clock::DaemonClock;

const MAX_HR_RING_BUFFER_LEN: usize = 1_800; // 30 min at 1 Hz

/// Narrow async boundary for BLE bonding and interactive `Agent1` pairing during enrollment.
pub trait EnrollmentPairingHook: Send + Sync {
    /// Sets device trust and optionally executes BLE SMP pairing bridged through `DaemonCore`.
    fn bond_and_pair<'a>(
        &'a self,
        core: &'a Arc<DaemonCore>,
        mac: &'a str,
        pair: bool,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// No-op [`EnrollmentPairingHook`] used in hermetic tests that do not inject a pairing harness.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopEnrollmentDriver;

impl EnrollmentPairingHook for NoopEnrollmentDriver {
    fn bond_and_pair<'a>(
        &'a self,
        _core: &'a Arc<DaemonCore>,
        _mac: &'a str,
        _pair: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

/// Production BlueZ [`EnrollmentPairingHook`] registering a temporary enrollment-scoped `Agent1`.
#[derive(Debug, Default, Clone, Copy)]
pub struct BluezEnrollmentDriver;

impl EnrollmentPairingHook for BluezEnrollmentDriver {
    fn bond_and_pair<'a>(
        &'a self,
        core: &'a Arc<DaemonCore>,
        mac: &'a str,
        pair: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let Some(conn) = try_system_conn().await else {
                return Ok(());
            };
            let _ = crate::bluetooth::operations::stop_le_discovery(&conn, "/org/bluez/hci0").await;

            if !pair {
                let _ =
                    crate::bluetooth::operations::bond_and_trust_device(&conn, mac, false).await;
                return Ok(());
            }

            let (auth_tx, mut auth_rx) = mpsc::channel::<AgentAuthRequest>(8);
            let agent = EnrollmentAgent::new(auth_tx);
            let agent_path = zbus::zvariant::ObjectPath::try_from(ENROLLMENT_AGENT_PATH)
                .map_err(|e| format!("invalid agent path: {e}"))?;

            let registered_obj = conn
                .object_server()
                .at(agent_path.clone(), agent)
                .await
                .is_ok();
            let agent_mgr = AgentManager1Proxy::new(&conn).await.ok();
            let mut registered_mgr = false;
            if let Some(ref mgr) = agent_mgr
                && mgr
                    .register_agent(agent_path.as_ref(), "KeyboardDisplay")
                    .await
                    .is_ok()
            {
                registered_mgr = true;
            }

            let core_bridge = Arc::clone(core);
            let bridge_task = tokio::spawn(async move {
                while let Some(req) = auth_rx.recv().await {
                    core_bridge.bridge_agent_auth_request(req).await;
                }
            });

            let res = crate::bluetooth::operations::bond_and_trust_device(&conn, mac, true)
                .await
                .map_err(|e| format!("Bluetooth pairing failed: {e}"));

            bridge_task.abort();
            let _ = bridge_task.await;
            if registered_mgr && let Some(ref mgr) = agent_mgr {
                let _ = mgr.unregister_agent(agent_path.as_ref()).await;
            }
            if registered_obj {
                let _ = conn
                    .object_server()
                    .remove::<EnrollmentAgent, _>(agent_path)
                    .await;
            }

            res
        })
    }
}

fn ipc_err(code: &str, message: impl Into<String>) -> ErrorPayload {
    ErrorPayload {
        code: code.to_string(),
        message: message.into(),
    }
}

fn storage_to_ipc_err(err: StorageError) -> ErrorPayload {
    ipc_err(err.ipc_code(), err.to_string())
}

fn serialize_json(value: impl Serialize) -> Result<Value, ErrorPayload> {
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
    pub pipeline: Arc<SleepActionPipeline>,
    pub coordinator: Arc<GraceCoordinator>,
    pub engine_ctrl_tx: mpsc::Sender<super::EngineControlEvent>,
    active_grace_tx: Mutex<Option<mpsc::Sender<GraceControlCmd>>>,
    active_grace_params: Mutex<Option<GraceRunParams>>,
    pub sleep_triggered_at: Mutex<Option<i64>>,
    pub trigger_power_action: Mutex<PowerAction>,
    pub rapid_resume_within_5m: Mutex<bool>,
    calibration_tx: watch::Sender<CalibrationState>,
    config_tx: watch::Sender<(DetectionConfig, RawSamplesRetention)>,
    inner: Mutex<InnerState>,
}

struct InnerState {
    loaded_config: LoadedConfig,
    dry_run: bool,
    paused: bool,
    paused_until: Option<DateTime<Local>>,
    pause_mode: Option<PauseMode>,
    pause_generation: u64,
    detector_state: DetectorState,
    active_session_id: Option<String>,
    active_wearable_id: Option<String>,
    calibration_state: CalibrationState,
    hr_ring_buffer: VecDeque<HrSampleEvent>,
    alerts: Vec<DaemonAlert>,
    started_at: tokio::time::Instant,
    enrollment: Option<EnrollmentLock>,
    discovered_wearables: Vec<DiscoveredWearable>,
    wearable_statuses: HashMap<String, StreamStatus>,
    wearable_batteries: HashMap<String, u8>,
    wearable_body_locations: HashMap<String, String>,
    playing_players_count: Option<usize>,
    active_auth_reply: Option<tokio::sync::oneshot::Sender<EnrollAuthResponseParams>>,
}

struct EnrollmentLock {
    client_id: u64,
    started_at_ms: i64,
    timeout_ms: i64,
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
    pub engine_ctrl_tx: mpsc::Sender<super::EngineControlEvent>,
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
            pipeline,
            coordinator,
            engine_ctrl_tx,
            active_grace_tx: Mutex::new(None),
            active_grace_params: Mutex::new(None),
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

    fn build_status_locked(&self, guard: &InnerState) -> DaemonStatus {
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

    /// Populates discovered BlueZ wearables returned by `wearable.enroll_start` in integration tests.
    pub fn seed_discovered_wearables(&self, wearables: Vec<DiscoveredWearable>) {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard.discovered_wearables = wearables;
    }

    fn build_config_snapshot_locked(&self, guard: &InnerState) -> ConfigSnapshotPayload {
        ConfigSnapshotPayload {
            config: guard.loaded_config.config.clone(),
            file_present: guard.loaded_config.file_present,
            config_path: self.paths.config_file.display().to_string(),
            log_filter: guard.loaded_config.log_filter.clone(),
            log_format: guard.loaded_config.log_format,
            log_biometrics: guard.loaded_config.log_biometrics,
        }
    }

    /// Returns whether the daemon is currently running an active Grace Period.
    pub fn is_in_grace_period(&self) -> bool {
        self.active_grace_tx.lock().unwrap().is_some()
    }

    /// Returns whether sleep triggers are currently armed.
    pub fn is_armed(&self) -> bool {
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        is_armed_locked(&guard, self.clock.as_ref())
    }

    /// Returns whether runtime or configuration dry-run mode is active.
    pub fn is_dry_run(&self) -> bool {
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        guard.dry_run || guard.loaded_config.config.actions.dry_run
    }

    pub fn set_active_grace(&self, tx: mpsc::Sender<GraceControlCmd>, params: GraceRunParams) {
        let mut guard_tx = self.active_grace_tx.lock().unwrap();
        let mut guard_p = self.active_grace_params.lock().unwrap();
        *guard_tx = Some(tx);
        *guard_p = Some(params);
    }

    pub fn clear_active_grace(&self) {
        let mut guard_tx = self.active_grace_tx.lock().unwrap();
        let mut guard_p = self.active_grace_params.lock().unwrap();
        *guard_tx = None;
        *guard_p = None;
    }

    pub async fn cancel_grace(&self) -> Result<(), ErrorPayload> {
        let tx = self.active_grace_tx.lock().unwrap().clone();
        if let Some(sender) = tx {
            let _ = sender.send(GraceControlCmd::Cancel).await;
            Ok(())
        } else {
            Err(ipc_err(
                error_codes::PRECONDITION_FAILED,
                "no active grace period to cancel",
            ))
        }
    }

    pub async fn snooze_grace(&self, duration_sec: Option<u32>) -> Result<u32, ErrorPayload> {
        let tx = self.active_grace_tx.lock().unwrap().clone();
        if let Some(sender) = tx {
            let default_sec = {
                let guard = self.inner.lock().unwrap();
                guard.loaded_config.config.actions.snooze_duration_min * 60
            };
            let eff = duration_sec.unwrap_or(default_sec);
            let _ = sender
                .send(GraceControlCmd::Snooze {
                    duration_sec: Some(eff),
                })
                .await;
            Ok(eff)
        } else {
            Err(ipc_err(
                error_codes::PRECONDITION_FAILED,
                "no active grace period to snooze",
            ))
        }
    }

    pub async fn signal_arousal_abort(&self) {
        let tx = self.active_grace_tx.lock().unwrap().clone();
        if let Some(sender) = tx {
            let _ = sender.send(GraceControlCmd::ArousalAbort).await;
        }
    }

    pub async fn signal_signal_lost(&self) {
        let tx = self.active_grace_tx.lock().unwrap().clone();
        if let Some(sender) = tx {
            let _ = sender.send(GraceControlCmd::SignalLost).await;
        }
    }

    pub async fn signal_prepare_for_sleep(&self, start: bool) {
        if start {
            let tx = self.active_grace_tx.lock().unwrap().clone();
            if let Some(sender) = tx {
                let _ = sender.send(GraceControlCmd::PrepareForSleep).await;
            }
        } else {
            let triggered_at = *self.sleep_triggered_at.lock().unwrap();
            if let Some(ts) = triggered_at {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let elapsed_ms = now_ms.saturating_sub(ts);
                if elapsed_ms < 300_000 {
                    *self.rapid_resume_within_5m.lock().unwrap() = true;
                    self.deliver_grace_outcome(GraceOutcome {
                        ts_ms: now_ms,
                        outcome: GraceResult::PostTriggerRapidWake,
                    })
                    .await;
                } else {
                    *self.rapid_resume_within_5m.lock().unwrap() = false;
                }
                *self.sleep_triggered_at.lock().unwrap() = None;
            }
        }
    }

    pub fn take_trigger_power_action(&self) -> PowerAction {
        let mut guard = self.trigger_power_action.lock().unwrap();
        let act = *guard;
        *guard = PowerAction::None;
        act
    }

    pub fn take_rapid_resume(&self) -> bool {
        let mut guard = self.rapid_resume_within_5m.lock().unwrap();
        let r = *guard;
        *guard = false;
        r
    }

    pub async fn start_test_trigger(
        self: &Arc<Self>,
        grace_sec: Option<u32>,
    ) -> Result<u32, ErrorPayload> {
        if self.is_in_grace_period() {
            return Err(ipc_err(
                error_codes::CONFLICT,
                "grace period is already active",
            ));
        }
        let (params, eff_sec) = {
            let guard = self.inner.lock().unwrap();
            let mut cfg = guard.loaded_config.config.actions.clone();
            let raw_sec = grace_sec.unwrap_or_else(|| cfg.effective_grace_period_sec());
            let eff_sec = raw_sec;
            cfg.grace_period_sec = Some(eff_sec);
            let params = GraceRunParams {
                session_id: "test-trigger-session".to_string(),
                wearable_id: "test-trigger-wearable".to_string(),
                config: cfg,
                dry_run: true,
            };
            (params, eff_sec)
        };

        let (tx, rx) = mpsc::channel(8);
        self.set_active_grace(tx, params.clone());

        let core = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = core.coordinator.run(params, rx).await;
            core.clear_active_grace();
            if let GraceRunOutcome::Completed(gr) = outcome
                && gr.outcome == GraceResult::Expired
            {
                let ctx = PipelineContext {
                    session_id: "test-trigger-session".to_string(),
                    wearable_id: "test-trigger-wearable".to_string(),
                    event: "dry_run".to_string(),
                    dry_run: true,
                };
                let cfg = core
                    .inner
                    .lock()
                    .unwrap()
                    .loaded_config
                    .config
                    .actions
                    .clone();
                let _ = core.pipeline.execute(&ctx, &cfg).await;
            }
        });

        Ok(eff_sec)
    }

    pub async fn deliver_grace_outcome(&self, outcome: GraceOutcome) {
        let _ = self
            .engine_ctrl_tx
            .send(super::EngineControlEvent::GraceOutcome(outcome))
            .await;
    }

    pub async fn handle_confirmed_sleep_onset(self: &Arc<Self>, ts_ms: i64) {
        let (is_paused, cfg, session_id, wearable_id) = {
            let guard = self.inner.lock().unwrap();
            (
                guard.paused,
                guard.loaded_config.config.clone(),
                guard
                    .active_session_id
                    .clone()
                    .unwrap_or_else(|| "default-session".to_string()),
                guard
                    .active_wearable_id
                    .clone()
                    .unwrap_or_else(|| "default-wearable".to_string()),
            )
        };

        if is_paused {
            tracing::info!(
                "sleep onset confirmed but triggers are paused; transitioning silently to Sleeping"
            );
            self.deliver_grace_outcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::Expired,
            })
            .await;
            return;
        }

        if cfg.schedule.active_hours_enabled
            && !cfg
                .schedule
                .is_within_active_hours(self.clock.now_local().time())
        {
            tracing::info!(
                "sleep onset confirmed but outside active hours; transitioning silently to Sleeping"
            );
            self.deliver_grace_outcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::Expired,
            })
            .await;
            return;
        }

        // 2-minute desktop input pre-gate:
        // Desktop activity within trailing 2 min suppresses entering Grace Period
        let is_active = self
            .idle_probe
            .is_user_active_trailing(Duration::from_secs(120))
            .await
            .unwrap_or(false);
        if is_active {
            tracing::info!(
                "desktop activity detected within trailing 2-min pre-gate window; suppressing Grace Period"
            );
            self.deliver_grace_outcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::AbortedNoPenalty,
            })
            .await;
            return;
        }

        if cfg.actions.pause_media
            && cfg.actions.custom_command.is_empty()
            && !cfg.actions.lock_screen
            && cfg.actions.power_action == PowerAction::None
        {
            let has_playing = self
                .action_executor
                .has_playing_players(&cfg.actions.ignore_players)
                .await
                .unwrap_or(false);
            if !has_playing {
                tracing::info!(
                    "strictly pause_media pipeline with 0 active players; skipping Grace Period and transitioning silently to Sleeping"
                );
                self.deliver_grace_outcome(GraceOutcome {
                    ts_ms,
                    outcome: GraceResult::Expired,
                })
                .await;
                return;
            }
        }

        let mut eff_actions = cfg.actions.clone();
        let eff_grace = compute_effective_grace_period(
            eff_actions.effective_grace_period_sec(),
            self.idle_probe.backend_kind(),
            eff_actions.power_action,
        );
        eff_actions.grace_period_sec = Some(eff_grace);

        if is_power_action_refused(
            self.idle_probe.backend_kind(),
            eff_actions.power_action,
            eff_actions.strict_idle_backend,
        ) {
            tracing::warn!(
                action = %eff_actions.power_action,
                "destructive power action refused due to strict_idle_backend on degraded idle probe"
            );
            let _ = self
                .notifier
                .show_warning(
                    "Power Action Refused",
                    "Destructive power action refused because strict_idle_backend is enabled and no desktop idle backend is available.",
                    false,
                )
                .await;
            eff_actions.power_action = PowerAction::None;
        } else if self.idle_probe.backend_kind().is_degraded()
            && matches!(
                eff_actions.power_action,
                PowerAction::Suspend | PowerAction::Poweroff
            )
        {
            let _ = self
                .notifier
                .show_warning(
                    "Degraded Idle Backend",
                    "No desktop idle backend available. Enforcing a >= 90s Grace Period safety floor.",
                    false,
                )
                .await;
        }

        let params = GraceRunParams {
            session_id: session_id.clone(),
            wearable_id: wearable_id.clone(),
            config: eff_actions.clone(),
            dry_run: self.is_dry_run(),
        };

        let (tx, rx) = mpsc::channel(8);
        self.set_active_grace(tx, params.clone());

        let core = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = core.coordinator.run(params, rx).await;
            core.clear_active_grace();
            match outcome {
                GraceRunOutcome::Completed(gr) => {
                    if gr.outcome == GraceResult::Expired {
                        core.set_detector_phase(
                            DetectorState::Sleeping,
                            Some(session_id.clone()),
                            Some(wearable_id.clone()),
                        );
                        let ctx = PipelineContext {
                            session_id: session_id.clone(),
                            wearable_id: wearable_id.clone(),
                            event: "sleep_trigger".to_string(),
                            dry_run: core.is_dry_run(),
                        };
                        let res = core
                            .pipeline
                            .execute(&ctx, &eff_actions)
                            .await
                            .unwrap_or_default();
                        match res.power_action_result {
                            Some(PowerActionResult::RefusedInhibited(ref reason)) => {
                                core.set_logind_inhibited_alert(Some(reason));
                            }
                            Some(PowerActionResult::Executed) => {
                                core.set_logind_inhibited_alert(None);
                            }
                            _ => {}
                        }
                        *core.sleep_triggered_at.lock().unwrap() = Some(gr.ts_ms);
                        *core.trigger_power_action.lock().unwrap() = eff_actions.power_action;

                        if eff_actions.power_action == PowerAction::None && !core.is_dry_run() {
                            let c_watch = Arc::clone(&core);
                            let paused_targets = res.paused_players.clone();
                            let s_id = session_id.clone();
                            let w_id = wearable_id.clone();
                            tokio::spawn(async move {
                                let mpris_watch = async {
                                    if paused_targets.is_empty() {
                                        std::future::pending::<bool>().await
                                    } else {
                                        c_watch
                                            .action_executor
                                            .watch_mpris_resume(&paused_targets)
                                            .await
                                    }
                                };
                                let idle_watch = c_watch
                                    .idle_probe
                                    .watch_post_trigger_activity(Duration::from_secs(300));

                                let trigger_fp = |reason: &'static str| {
                                    let c = Arc::clone(&c_watch);
                                    let s = s_id.clone();
                                    let w = w_id.clone();
                                    async move {
                                        tracing::info!("{reason}; recording FP");
                                        let now_ms = c.clock.now_utc().timestamp_millis();
                                        c.set_detector_phase(
                                            DetectorState::Monitoring,
                                            Some(s),
                                            Some(w),
                                        );
                                        c.deliver_grace_outcome(GraceOutcome {
                                            ts_ms: now_ms,
                                            outcome: GraceResult::PostTriggerRapidWake,
                                        })
                                        .await;
                                    }
                                };

                                tokio::select! {
                                    resumed = mpris_watch => {
                                        if resumed {
                                            trigger_fp(
                                                "MPRIS player resumed playback within 5-min post-trigger watch"
                                            ).await;
                                        }
                                    }
                                    activity = idle_watch => {
                                        if activity {
                                            trigger_fp(
                                                "desktop user activity detected within 5-min post-trigger watch"
                                            ).await;
                                        }
                                    }
                                }
                            });
                        }
                    } else if matches!(
                        gr.outcome,
                        GraceResult::Cancelled
                            | GraceResult::Snoozed { .. }
                            | GraceResult::AbortedNoPenalty
                    ) {
                        core.set_detector_phase(
                            DetectorState::Monitoring,
                            Some(session_id.clone()),
                            Some(wearable_id.clone()),
                        );
                    }
                    if !core.is_dry_run() {
                        core.deliver_grace_outcome(gr).await;
                    }
                }
                GraceRunOutcome::AsymmetricSignalLost => {
                    core.set_detector_phase(
                        DetectorState::Monitoring,
                        Some(session_id.clone()),
                        Some(wearable_id.clone()),
                    );
                    let ctx = PipelineContext {
                        session_id,
                        wearable_id,
                        event: "sleep_trigger".to_string(),
                        dry_run: core.is_dry_run(),
                    };
                    let mut non_disruptive = cfg.actions.clone();
                    non_disruptive.custom_command.clear();
                    non_disruptive.lock_screen = false;
                    non_disruptive.power_action = PowerAction::None;
                    let _ = core.pipeline.execute(&ctx, &non_disruptive).await;
                    if !core.is_dry_run() {
                        let now_ms = core.clock.now_utc().timestamp_millis();
                        core.deliver_grace_outcome(GraceOutcome {
                            ts_ms: now_ms,
                            outcome: GraceResult::Expired,
                        })
                        .await;
                    }
                }
            }
        });
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

    /// Dispatches an IPC command over the Unix domain socket.
    pub async fn handle_op(
        self: &Arc<Self>,
        op: &str,
        params: Value,
        client_id: u64,
    ) -> Result<Value, ErrorPayload> {
        match op {
            "daemon.ping" => Ok(json!({ "pong": true, "v": PROTOCOL_VERSION })),
            "daemon.status" => {
                self.refresh_playing_players_count().await;
                serialize_json(self.status())
            }
            "monitor.pause" => {
                let parsed: PauseParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid monitor.pause params: {e}"),
                    )
                })?;
                serialize_json(self.pause(parsed)?)
            }
            "monitor.resume" => serialize_json(self.resume()),
            "monitor.set_dry_run" => {
                let parsed: SetDryRunParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid monitor.set_dry_run params: {e}"),
                    )
                })?;
                serialize_json(self.set_dry_run(parsed.dry_run))
            }
            "monitor.cancel_grace" => {
                self.cancel_grace().await?;
                Ok(json!({ "cancelled": true }))
            }
            "monitor.snooze_grace" => {
                let dur_sec = params
                    .get("duration_sec")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .or_else(|| {
                        params
                            .get("duration_min")
                            .and_then(|v| v.as_u64())
                            .map(|v| (v * 60) as u32)
                    });
                let dur = self.snooze_grace(dur_sec).await?;
                Ok(json!({ "snoozed": true, "duration_sec": dur }))
            }
            "monitor.test_trigger" => {
                let grace_sec = params
                    .get("grace_sec")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
                    .or_else(|| {
                        params
                            .get("grace_period_sec")
                            .and_then(|v| v.as_u64())
                            .map(|v| v as u32)
                    });
                let eff_sec = self.start_test_trigger(grace_sec).await?;
                Ok(json!({ "test_trigger_started": true, "grace_period_sec": eff_sec }))
            }
            "config.get" => {
                let guard = self.inner.lock().expect("daemon state mutex poisoned");
                serialize_json(self.build_config_snapshot_locked(&guard))
            }
            "config.update" => {
                let patch: ConfigPatch = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_CONFIG,
                        format!("invalid config patch: {e}"),
                    )
                })?;
                serialize_json(self.update_config(&patch)?)
            }
            "config.reload" => serialize_json(self.reload_from_disk()?),
            "session.list" => {
                let parsed: SessionListParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.list params: {e}"),
                    )
                })?;
                let limit = parsed.limit.unwrap_or(20);
                let offset = parsed.offset.unwrap_or(0);
                let page = self
                    .persistence
                    .list_sessions(limit, offset)
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(page)
            }
            "session.get" => {
                let parsed: SessionGetParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.get params: {e}"),
                    )
                })?;
                let detail = self
                    .persistence
                    .get_session(parsed.id)
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(detail)
            }
            "session.delete" => {
                let parsed: SessionDeleteParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.delete params: {e}"),
                    )
                })?;
                {
                    let guard = self.inner.lock().expect("daemon state mutex poisoned");
                    if guard.active_session_id.as_deref() == Some(&parsed.id)
                        || guard
                            .active_session_id
                            .as_ref()
                            .is_some_and(|aid| aid.starts_with(&parsed.id))
                    {
                        return Err(ipc_err(
                            error_codes::PRECONDITION_FAILED,
                            "cannot delete an active session",
                        ));
                    }
                }
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (deleted_id, cal_state) = self
                    .persistence
                    .delete_session_and_recompute(parsed.id, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state);
                Ok(json!({ "deleted_session_id": deleted_id, "deleted_id": deleted_id }))
            }
            "session.export_trace" => {
                let parsed: SessionExportTraceParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid session.export_trace params: {e}"),
                        )
                    })?;
                let dest_path = std::path::PathBuf::from(&parsed.path);
                let format = parsed.format.unwrap_or(crate::replay::TraceFormat::Csv);
                let count = self
                    .persistence
                    .export_trace(parsed.id, dest_path, format, !parsed.no_anonymize)
                    .await
                    .map_err(storage_to_ipc_err)?;
                Ok(json!({ "path": parsed.path, "events_count": count }))
            }
            "session.set_feedback" => {
                let parsed: SetFeedbackParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.set_feedback params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let req = SetFeedbackRequest {
                    session_id_or_prefix: parsed.session_id,
                    verdict: parsed.verdict,
                    onset_hhmm: parsed.onset_hhmm,
                    avg_sleep_hr: parsed.avg_sleep_hr,
                    clear_all: parsed.clear,
                };
                let (detail, cal_state) = self
                    .persistence
                    .set_feedback_and_recompute(req, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state);
                serialize_json(detail)
            }
            "calibration.status" => serialize_json(self.calibration_state()),
            "calibration.set_seed" => {
                let parsed: SetSeedParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid calibration.set_seed params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let cal_state = self
                    .persistence
                    .set_day1_seed_and_recompute(parsed.sleep_hr, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                serialize_json(cal_state)
            }
            "calibration.accept_shift" => {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let accepted = self
                    .persistence
                    .accept_shift_and_recompute(now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(accepted.calibration.clone());
                serialize_json(accepted)
            }
            "calibration.dismiss_shift" => {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let cal_state = self
                    .persistence
                    .dismiss_shift_and_recompute(now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                serialize_json(cal_state)
            }
            "calibration.set_boundary" => {
                let parsed: CalibrationSetBoundaryParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid calibration.set_boundary params: {e}"),
                        )
                    })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (boundary, cal_state) = self
                    .persistence
                    .set_calibration_boundary_and_recompute(
                        parsed.effective_ts_ms,
                        parsed.seed_sleep_hr,
                        parsed.reset_floors,
                        now_ms,
                    )
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                Ok(json!({ "boundary": boundary, "calibration": cal_state }))
            }
            "calibration.undo_boundary" => {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (boundary, cal_state) = self
                    .persistence
                    .undo_calibration_boundary_and_recompute(now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                Ok(json!({ "boundary": boundary, "calibration": cal_state }))
            }
            "data.stats" => {
                let stats = self
                    .persistence
                    .get_storage_stats()
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(stats)
            }
            "data.purge_raw" => {
                let reclaimed = self
                    .persistence
                    .purge_raw_samples()
                    .await
                    .map_err(storage_to_ipc_err)?;
                Ok(json!({ "reclaimed_rows": reclaimed }))
            }
            "data.hard_wipe" => {
                let parsed: DataHardWipeParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid data.hard_wipe params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.active_session_id = None;
                    guard.detector_state = crate::sleep_engine::DetectorState::Idle;
                    guard.hr_ring_buffer.clear();
                    guard.calibration_state = CalibrationState::default();
                }
                self.persistence
                    .hard_wipe(parsed.all, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                if parsed.all && self.paths.config_file.try_exists().unwrap_or(false) {
                    let _ = std::fs::remove_file(&self.paths.config_file);
                }
                self.apply_calibration_update(CalibrationState::default());
                Ok(json!({ "wiped": true }))
            }
            "data.backup_sqlite" => {
                let parsed: DataBackupSqliteParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid data.backup_sqlite params: {e}"),
                        )
                    })?;
                let path = std::path::PathBuf::from(parsed.path);
                self.persistence
                    .backup_sqlite(path.clone())
                    .await
                    .map_err(storage_to_ipc_err)?;
                Ok(json!({ "path": path.to_string_lossy() }))
            }
            "data.export_bundle" => {
                let parsed: DataExportBundleParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid data.export_bundle params: {e}"),
                        )
                    })?;
                let options = parsed.options();
                let path = std::path::PathBuf::from(parsed.path);
                let now_ms = self.clock.now_utc().timestamp_millis();
                self.persistence
                    .export_bundle(path.clone(), options, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                Ok(json!({ "path": path.to_string_lossy() }))
            }
            "data.import_bundle" => {
                let parsed: DataImportBundleParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid data.import_bundle params: {e}"),
                        )
                    })?;
                let path = std::path::PathBuf::from(parsed.path);
                let mode = parsed.mode.unwrap_or(ImportMode::Merge);
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (summary, cal_state) = self
                    .persistence
                    .import_bundle(path, mode, parsed.force, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                Ok(json!({ "summary": summary, "calibration": cal_state }))
            }
            "wearable.list" => {
                let enrolled = self
                    .persistence
                    .list_wearables()
                    .await
                    .map_err(storage_to_ipc_err)?;
                let guard = self.inner.lock().expect("daemon state mutex poisoned");
                let statuses: Vec<EnrolledWearableStatus> = enrolled
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
                    .collect();
                Ok(json!({ "wearables": statuses }))
            }
            "wearable.rename" => {
                let parsed: WearableRenameParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid wearable.rename params: {e}"),
                    )
                })?;
                let updated = self
                    .persistence
                    .rename_wearable(parsed.id, parsed.alias)
                    .await
                    .map_err(storage_to_ipc_err)?;
                Ok(json!({ "wearable": updated }))
            }
            "wearable.connect" => {
                let parsed: WearableConnectParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.connect params: {e}"),
                        )
                    })?;
                let list = self
                    .persistence
                    .list_wearables()
                    .await
                    .map_err(storage_to_ipc_err)?;
                let w = list
                    .into_iter()
                    .find(|w| w.id == parsed.id || w.id.starts_with(&parsed.id))
                    .ok_or_else(|| {
                        ipc_err(
                            error_codes::NOT_FOUND,
                            format!("wearable '{}' not found", parsed.id),
                        )
                    })?;
                if let Some(conn) = try_system_conn().await {
                    let _ = crate::bluetooth::operations::connect_device(&conn, &w.mac).await;
                }
                Ok(json!({ "ok": true, "message": format!("connection requested for {}", w.id) }))
            }
            "wearable.unenroll" => {
                let parsed: WearableUnenrollParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.unenroll params: {e}"),
                        )
                    })?;
                let unenrolled = self
                    .persistence
                    .unenroll_wearable(parsed.id, parsed.delete_sessions)
                    .await
                    .map_err(storage_to_ipc_err)?;
                {
                    let norm = unenrolled.mac.to_ascii_uppercase();
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.wearable_statuses.remove(&norm);
                    guard.wearable_batteries.remove(&norm);
                    guard.wearable_body_locations.remove(&norm);
                }
                if parsed.forget_bluez
                    && let Some(conn) = try_system_conn().await
                {
                    let _ = crate::bluetooth::operations::remove_device(
                        &conn,
                        "/org/bluez/hci0",
                        &unenrolled.mac,
                    )
                    .await;
                }
                Ok(json!({ "wearable": unenrolled }))
            }
            "wearable.enroll_start" => {
                let parsed: EnrollStartParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid wearable.enroll_start params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let timeout_sec = parsed.timeout_sec.unwrap_or(30).clamp(5, 120);
                let timeout_ms = (timeout_sec as i64) * 1000;

                if let Some(conn) = try_system_conn().await {
                    let _ =
                        crate::bluetooth::operations::start_le_discovery(&conn, "/org/bluez/hci0")
                            .await;
                    if let Ok(discovered) =
                        crate::bluetooth::operations::list_heart_rate_devices(&conn).await
                    {
                        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                        guard.discovered_wearables = discovered;
                    }
                }

                let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                if let Some(ref lock) = guard.enrollment {
                    let expired = now_ms >= lock.started_at_ms + lock.timeout_ms;
                    if !expired && lock.client_id != client_id {
                        return Err(ipc_err(
                            error_codes::ENROLLMENT_IN_PROGRESS,
                            "another enrollment session is currently in progress",
                        ));
                    }
                }
                guard.enrollment = Some(EnrollmentLock {
                    client_id,
                    started_at_ms: now_ms,
                    timeout_ms,
                });
                let devices = guard.discovered_wearables.clone();
                Ok(json!({ "devices": devices, "wearables": devices }))
            }
            "wearable.enroll_select" => {
                let parsed: EnrollSelectParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid wearable.enroll_select params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let norm_mac = parsed.address.trim().to_ascii_uppercase();
                let (discovered, is_rpa, address_type, advertised_name) = {
                    let guard = self.inner.lock().expect("daemon state mutex poisoned");
                    match guard.enrollment {
                        Some(ref lock) if lock.client_id == client_id => {}
                        Some(_) => {
                            return Err(ipc_err(
                                error_codes::PRECONDITION_FAILED,
                                "enrollment session is owned by another client",
                            ));
                        }
                        None => {
                            return Err(ipc_err(
                                error_codes::PRECONDITION_FAILED,
                                "no active enrollment session",
                            ));
                        }
                    }
                    let discovered = guard
                        .discovered_wearables
                        .iter()
                        .find(|d| d.address.eq_ignore_ascii_case(&norm_mac))
                        .cloned();

                    let address_type = discovered
                        .as_ref()
                        .map(|d| d.address_type.clone())
                        .unwrap_or_else(|| {
                            if crate::bluetooth::dbus::is_resolvable_private_address(
                                &norm_mac, "random",
                            ) {
                                "random".to_string()
                            } else {
                                "public".to_string()
                            }
                        });
                    let is_rpa = crate::bluetooth::dbus::is_resolvable_private_address(
                        &norm_mac,
                        &address_type,
                    );
                    let advertised_name = discovered.as_ref().and_then(|d| d.name.clone());
                    (discovered, is_rpa, address_type, advertised_name)
                };

                let pair_result = Arc::clone(&self.enrollment_driver)
                    .bond_and_pair(self, &norm_mac, is_rpa)
                    .await;

                {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    if matches!(guard.enrollment, Some(ref lock) if lock.client_id == client_id) {
                        guard.enrollment = None;
                    }
                    guard.active_auth_reply = None;
                }

                if let Err(err) = pair_result {
                    return Err(ipc_err(error_codes::BACKEND_REFUSED, err));
                }

                let mut warnings = Vec::new();
                let mut hints = Vec::new();
                if is_rpa {
                    warnings.push("Wearable uses a rotating Resolvable Private Address (RPA). Bonding ensures auto-reconnection persists.".to_string());
                }
                if let Some(ref d) = discovered
                    && crate::bluetooth::dbus::is_dual_mode_bredr_candidate(&d.uuids)
                {
                    hints.push("Dual-mode BR/EDR wearable detected; classic profiles (A2DP/HFP) may share this adapter.".to_string());
                }

                let desc = WearableDescriptor {
                    mac: norm_mac,
                    address_type,
                    alias: parsed.alias,
                    advertised_name,
                    supports_rr: false,
                    supports_contact: true,
                };
                let w_id = self
                    .persistence
                    .upsert_wearable(desc, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                let list = self
                    .persistence
                    .list_wearables()
                    .await
                    .map_err(storage_to_ipc_err)?;
                let enrolled = list.into_iter().find(|w| w.id == w_id).ok_or_else(|| {
                    ipc_err(
                        error_codes::INTERNAL_ERROR,
                        "failed to load newly enrolled wearable",
                    )
                })?;

                Ok(json!({
                    "wearable": enrolled,
                    "warnings": warnings,
                    "hints": hints,
                }))
            }
            "alert.dismiss" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ipc_err(error_codes::INVALID_PARAMS, "missing 'id' param"))?;
                let (changed, alerts) = {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    let before = guard.alerts.len();
                    guard.alerts.retain(|a| a.id != id || !a.dismissible);
                    (guard.alerts.len() != before, guard.alerts.clone())
                };
                if changed {
                    self.broker
                        .publish_control("AlertChanged", json!({ "alerts": alerts }));
                }
                Ok(json!({ "dismissed": changed }))
            }
            "wearable.enroll_cancel" => {
                if let Some(conn) = try_system_conn().await {
                    let _ =
                        crate::bluetooth::operations::stop_le_discovery(&conn, "/org/bluez/hci0")
                            .await;
                }
                let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                guard.enrollment = None;
                guard.discovered_wearables.clear();
                guard.active_auth_reply = None;
                Ok(json!({ "ok": true }))
            }
            "wearable.enroll_auth_response" => {
                let parsed: EnrollAuthResponseParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.enroll_auth_response params: {e}"),
                        )
                    })?;
                let reply_tx = {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.active_auth_reply.take()
                };
                if let Some(tx) = reply_tx {
                    let _ = tx.send(parsed);
                }
                Ok(json!({ "ok": true }))
            }
            _ => Err(ipc_err(
                error_codes::UNKNOWN_OP,
                format!("unknown IPC operation '{op}'"),
            )),
        }
    }

    /// Automatically cancels any active enrollment owned by `client_id` when the socket disconnects.
    pub fn on_client_disconnected(&self, client_id: u64) {
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
        if matches!(guard.enrollment, Some(ref lock) if lock.client_id == client_id) {
            tracing::info!(
                client_id,
                "enrollment auto-cancelled due to client socket disconnect"
            );
            guard.enrollment = None;
            guard.discovered_wearables.clear();
            guard.active_auth_reply = None;
        }
    }

    /// Triggers an interactive Agent1 pairing prompt pushed over IPC for BlueZ/test bridging.
    pub fn trigger_pairing_prompt(
        &self,
        prompt: PairingPrompt,
    ) -> tokio::sync::oneshot::Receiver<EnrollAuthResponseParams> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let wearables = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            guard.active_auth_reply = Some(tx);
            guard.discovered_wearables.clone()
        };
        let event = EnrollmentUpdateEvent {
            wearables,
            pairing_prompt: Some(prompt),
            remaining_sec: Some(30),
        };
        if let Ok(v) = serde_json::to_value(event) {
            self.broker.publish_control("EnrollmentUpdate", v);
        }
        rx
    }

    /// Bridges an incoming `AgentAuthRequest` to the enrolling client over IPC and forwards its response.
    pub async fn bridge_agent_auth_request(&self, req: AgentAuthRequest) {
        match req {
            AgentAuthRequest::Confirmation {
                device,
                passkey,
                reply,
            } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx =
                    self.trigger_pairing_prompt(PairingPrompt::Confirmation { passkey, wearable });
                let confirmed = rx.await.ok().and_then(|r| r.confirmed).unwrap_or(false);
                let _ = reply.send(confirmed);
            }
            AgentAuthRequest::Passkey { device, reply } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx = self.trigger_pairing_prompt(PairingPrompt::Passkey { wearable });
                let passkey = match rx.await {
                    Ok(r) if r.confirmed != Some(false) => r.passkey,
                    _ => None,
                };
                let _ = reply.send(passkey);
            }
            AgentAuthRequest::PinCode { device, reply } => {
                let wearable = self.resolve_wearable_label(&device);
                let rx = self.trigger_pairing_prompt(PairingPrompt::PinCode { wearable });
                let pincode = match rx.await {
                    Ok(r) if r.confirmed != Some(false) => r.pincode.filter(|s| !s.is_empty()),
                    _ => None,
                };
                let _ = reply.send(pincode);
            }
            AgentAuthRequest::Authorization { reply, .. } => {
                let _ = reply.send(true);
            }
        }
    }

    /// Upserts or clears the dismissible `logind` inhibitor refusal alert and broadcasts `AlertChanged`.
    pub fn set_logind_inhibited_alert(&self, reason: Option<&str>) {
        let alerts_opt = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            let changed = match reason {
                Some(r) => upsert_alert(
                    &mut guard.alerts,
                    DaemonAlert {
                        id: alert_ids::LOGIND_INHIBITED.to_string(),
                        kind: alert_ids::LOGIND_INHIBITED.to_string(),
                        severity: AlertSeverity::Warn,
                        message: format!(
                            "Sleep action blocked by logind inhibitor ({r}); media remains paused."
                        ),
                        dismissible: true,
                    },
                ),
                None => {
                    let before = guard.alerts.len();
                    guard.alerts.retain(|a| a.id != alert_ids::LOGIND_INHIBITED);
                    guard.alerts.len() != before
                }
            };
            changed.then(|| guard.alerts.clone())
        };
        if let Some(alerts) = alerts_opt {
            self.broker
                .publish_control("AlertChanged", json!({ "alerts": alerts }));
        }
    }

    fn resolve_wearable_label(&self, path_or_mac: &str) -> String {
        let mac = if let Some(pos) = path_or_mac.rfind("/dev_") {
            path_or_mac[pos + 5..].replace('_', ":")
        } else {
            path_or_mac.to_string()
        };
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        if let Some(found) = guard
            .discovered_wearables
            .iter()
            .find(|w| w.address.eq_ignore_ascii_case(&mac))
            && let Some(ref name) = found.name
        {
            return name.clone();
        }
        mask_mac(&mac)
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

    fn pause(self: &Arc<Self>, params: PauseParams) -> Result<DaemonStatus, ErrorPayload> {
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

    fn resume(&self) -> DaemonStatus {
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

    fn set_dry_run(&self, dry_run: bool) -> DaemonStatus {
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

    fn update_config(&self, patch: &ConfigPatch) -> Result<ConfigSnapshotPayload, ErrorPayload> {
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

fn upsert_alert(alerts: &mut Vec<DaemonAlert>, next: DaemonAlert) -> bool {
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

async fn try_system_conn() -> Option<zbus::Connection> {
    zbus::connection::Builder::system().ok()?.build().await.ok()
}
