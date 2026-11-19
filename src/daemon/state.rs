use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::calibration::CalibrationState;
use crate::config::{
    CliOverrides, ClockTime, ConfigPatch, DetectionConfig, LoadedConfig, RawSamplesRetention,
    patch_config_file,
};
use crate::ipc::protocol::{
    AlertSeverity, CalibrationUpdatedEvent, ConfigSnapshotPayload, DaemonAlert, DaemonStatus,
    DiscoveredWearable, EnrollAuthResponseParams, EnrollSelectParams, EnrollStartParams,
    EnrolledWearableStatus, EpochUpdateEvent, ErrorPayload, HrSampleEvent, MonitoringState,
    PROTOCOL_VERSION, PauseMode, PauseParams, PushEventEnvelope, SessionClosedEvent,
    SessionGetParams, SessionListParams, SetDryRunParams, SetFeedbackParams, SetSeedParams,
    WearableConnectParams, WearableRenameParams, WearableUnenrollParams, error_codes,
};
use crate::ipc::pubsub::{EventBroker, SubscriberQueue};
use crate::paths::{EnvSource, ResolvedPaths};
use crate::sleep_engine::{DetectorState, StreamStatus};
use crate::storage::worker::PersistenceHandle;
use crate::storage::{SessionFullDetail, SetFeedbackRequest, StorageError, WearableDescriptor};

use super::clock::DaemonClock;

const CONFIG_RELOAD_ALERT_ID: &str = "config_reload_failed";
const PERSISTENT_SHIFT_ALERT_ID: &str = "calibration_persistent_shift";
const MAX_HR_RING_BUFFER_LEN: usize = 1_800; // 30 min at 1 Hz

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

        Self {
            paths,
            env,
            cli_overrides,
            clock,
            broker,
            shutdown_rx,
            persistence,
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
        }
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
            "daemon.status" => serialize_json(self.status()),
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
                        let status = guard
                            .wearable_statuses
                            .get(&w.mac)
                            .copied()
                            .unwrap_or(StreamStatus::Disconnected);
                        let battery = guard.wearable_batteries.get(&w.mac).copied();
                        EnrolledWearableStatus {
                            wearable: w,
                            status,
                            battery,
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
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.wearable_statuses.remove(&unenrolled.mac);
                    guard.wearable_batteries.remove(&unenrolled.mac);
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
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    match guard.enrollment {
                        Some(ref lock) if lock.client_id == client_id => {
                            guard.enrollment = None;
                        }
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
                    mac: norm_mac.clone(),
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

                if let Some(conn) = try_system_conn().await {
                    let _ =
                        crate::bluetooth::operations::stop_le_discovery(&conn, "/org/bluez/hci0")
                            .await;
                    let _ = crate::bluetooth::operations::bond_and_trust_device(
                        &conn, &norm_mac, is_rpa,
                    )
                    .await;
                }

                Ok(json!({
                    "wearable": enrolled,
                    "warnings": warnings,
                    "hints": hints,
                }))
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
                Ok(json!({ "ok": true }))
            }
            "wearable.enroll_auth_response" => {
                let _parsed: EnrollAuthResponseParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.enroll_auth_response params: {e}"),
                        )
                    })?;
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
        }
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
            let alerts_opt = cleared_alert.then(|| guard.alerts.clone());

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
    alerts.retain(|a| a.id != CONFIG_RELOAD_ALERT_ID);
    alerts.len() != before
}

fn upsert_reload_alert(alerts: &mut Vec<DaemonAlert>, err_msg: &str) -> bool {
    let message = format!("Configuration reload failed (keeping last valid config): {err_msg}");
    upsert_alert(
        alerts,
        DaemonAlert {
            id: CONFIG_RELOAD_ALERT_ID.to_string(),
            kind: CONFIG_RELOAD_ALERT_ID.to_string(),
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
                id: PERSISTENT_SHIFT_ALERT_ID.to_string(),
                kind: PERSISTENT_SHIFT_ALERT_ID.to_string(),
                severity: AlertSeverity::Warn,
                message,
                dismissible: true,
            },
        )
    } else {
        let before = alerts.len();
        alerts.retain(|a| a.id != PERSISTENT_SHIFT_ALERT_ID);
        alerts.len() != before
    }
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
