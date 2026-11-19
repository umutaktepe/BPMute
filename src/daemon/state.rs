use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::config::{CliOverrides, ClockTime, ConfigPatch, LoadedConfig, patch_config_file};
use crate::ipc::protocol::{
    AlertSeverity, ConfigSnapshotPayload, DaemonAlert, DaemonStatus, PROTOCOL_VERSION, PauseMode,
    PauseParams, SetDryRunParams, error_codes,
};
use crate::ipc::pubsub::{EventBroker, SubscriberQueue};
use crate::paths::{EnvSource, ResolvedPaths};

use super::clock::DaemonClock;

const CONFIG_RELOAD_ALERT_ID: &str = "config_reload_failed";

/// Shared mutable daemon runtime state behind a short-lived synchronous mutex.
pub struct DaemonCore {
    pub paths: ResolvedPaths,
    pub env: Arc<dyn EnvSource>,
    pub cli_overrides: CliOverrides,
    pub clock: Arc<dyn DaemonClock>,
    pub broker: Arc<EventBroker>,
    pub shutdown_rx: watch::Receiver<bool>,
    inner: Mutex<InnerState>,
}

struct InnerState {
    loaded_config: LoadedConfig,
    dry_run: bool,
    paused: bool,
    paused_until: Option<DateTime<Local>>,
    pause_mode: Option<PauseMode>,
    pause_generation: u64,
    alerts: Vec<DaemonAlert>,
    started_at: tokio::time::Instant,
}

impl DaemonCore {
    /// Initializes daemon state from validated startup configuration.
    pub fn new(
        paths: ResolvedPaths,
        loaded_config: LoadedConfig,
        env: Arc<dyn EnvSource>,
        cli_overrides: CliOverrides,
        clock: Arc<dyn DaemonClock>,
        broker: Arc<EventBroker>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        let dry_run = loaded_config.config.actions.dry_run;
        let started_at = clock.now_instant();
        Self {
            paths,
            env,
            cli_overrides,
            clock,
            broker,
            shutdown_rx,
            inner: Mutex::new(InnerState {
                loaded_config,
                dry_run,
                paused: false,
                paused_until: None,
                pause_mode: None,
                pause_generation: 0,
                alerts: Vec::new(),
                started_at,
            }),
        }
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
            "Paused".to_string()
        } else {
            "Idle".to_string()
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
            log_format: guard.loaded_config.log_format.to_string(),
            log_biometrics: guard.loaded_config.log_biometrics,
        }
    }

    /// Registers a subscriber and builds its initial `Snapshot` push event.
    pub fn subscribe(&self) -> (u64, Arc<SubscriberQueue>, DaemonStatus) {
        let status = self.status();
        let (sub_id, queue) = self.broker.subscribe();
        (sub_id, queue, status)
    }

    /// Builds a `Snapshot` push envelope with current status and alerts.
    pub fn make_snapshot_event(
        &self,
        status: &DaemonStatus,
    ) -> crate::ipc::protocol::PushEventEnvelope {
        self.broker.make_event(
            "Snapshot",
            json!({
                "status": status,
                "hr_ring_buffer": [],
                "alerts": status.alerts,
            }),
        )
    }

    /// Dispatches an IPC command belonging to the `#15` tracer bullet surface.
    pub fn handle_op(self: &Arc<Self>, op: &str, params: Value) -> Result<Value, (String, String)> {
        match op {
            "daemon.ping" => Ok(json!({ "pong": true, "v": PROTOCOL_VERSION })),
            "daemon.status" => serde_json::to_value(self.status())
                .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string())),
            "monitor.pause" => {
                let parsed: PauseParams = serde_json::from_value(params).map_err(|e| {
                    (
                        error_codes::INVALID_PARAMS.to_string(),
                        format!("invalid monitor.pause params: {e}"),
                    )
                })?;
                let status = self.pause(parsed)?;
                serde_json::to_value(status)
                    .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string()))
            }
            "monitor.resume" => {
                let status = self.resume();
                serde_json::to_value(status)
                    .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string()))
            }
            "monitor.set_dry_run" => {
                let parsed: SetDryRunParams = serde_json::from_value(params).map_err(|e| {
                    (
                        error_codes::INVALID_PARAMS.to_string(),
                        format!("invalid monitor.set_dry_run params: {e}"),
                    )
                })?;
                let status = self.set_dry_run(parsed.dry_run);
                serde_json::to_value(status)
                    .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string()))
            }
            "config.get" => {
                let guard = self.inner.lock().expect("daemon state mutex poisoned");
                let snapshot = self.build_config_snapshot_locked(&guard);
                serde_json::to_value(snapshot)
                    .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string()))
            }
            "config.update" => {
                let patch: ConfigPatch = serde_json::from_value(params).map_err(|e| {
                    (
                        error_codes::INVALID_CONFIG.to_string(),
                        format!("invalid config patch: {e}"),
                    )
                })?;
                let snapshot = self.update_config(&patch)?;
                serde_json::to_value(snapshot)
                    .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string()))
            }
            "config.reload" => {
                let snapshot = self.reload_from_disk()?;
                serde_json::to_value(snapshot)
                    .map_err(|e| (error_codes::INTERNAL_ERROR.to_string(), e.to_string()))
            }
            _ => Err((
                error_codes::UNKNOWN_OP.to_string(),
                format!("unknown IPC operation '{op}'"),
            )),
        }
    }

    fn pause(self: &Arc<Self>, params: PauseParams) -> Result<DaemonStatus, (String, String)> {
        let mode = params.mode.unwrap_or(PauseMode::Indefinite);
        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");

        let (paused_until, sleep_duration) = match mode {
            PauseMode::Indefinite => {
                if params.duration_sec.is_some() {
                    return Err((
                        error_codes::INVALID_PARAMS.to_string(),
                        "duration_sec is only valid with mode = \"duration\"".to_string(),
                    ));
                }
                (None, None)
            }
            PauseMode::Duration => {
                let secs = params.duration_sec.ok_or_else(|| {
                    (
                        error_codes::INVALID_PARAMS.to_string(),
                        "duration_sec is required when mode = \"duration\"".to_string(),
                    )
                })?;
                if secs == 0 {
                    return Err((
                        error_codes::INVALID_PARAMS.to_string(),
                        "duration_sec must be >= 1".to_string(),
                    ));
                }
                let secs_i64 = i64::try_from(secs).map_err(|_| {
                    (
                        error_codes::INVALID_PARAMS.to_string(),
                        "duration_sec exceeds supported range".to_string(),
                    )
                })?;
                let now = self.clock.now_local();
                let until = now + chrono::Duration::seconds(secs_i64);
                (Some(until), Some(Duration::from_secs(secs)))
            }
            PauseMode::UntilMorning => {
                if params.duration_sec.is_some() {
                    return Err((
                        error_codes::INVALID_PARAMS.to_string(),
                        "duration_sec is not valid with mode = \"until_morning\"".to_string(),
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

    fn update_config(
        &self,
        patch: &ConfigPatch,
    ) -> Result<ConfigSnapshotPayload, (String, String)> {
        let updated = patch_config_file(
            &self.paths.config_file,
            patch,
            self.env.as_ref(),
            &self.cli_overrides,
        )
        .map_err(|err| (error_codes::INVALID_CONFIG.to_string(), err.to_string()))?;

        let (snapshot, state_changed_status, alerts_changed) = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            let old_armed = is_armed_locked(&guard, self.clock.as_ref());
            let old_dry_run = guard.dry_run;

            guard.loaded_config = updated;
            if self.cli_overrides.dry_run.is_none() {
                guard.dry_run = guard.loaded_config.config.actions.dry_run;
            }

            let cleared_alert = clear_reload_alert(&mut guard.alerts);
            let alerts_opt = cleared_alert.then(|| guard.alerts.clone());

            let new_armed = is_armed_locked(&guard, self.clock.as_ref());
            let status_opt = (old_armed != new_armed || old_dry_run != guard.dry_run)
                .then(|| self.build_status_locked(&guard));

            let snapshot = self.build_config_snapshot_locked(&guard);
            (snapshot, status_opt, alerts_opt)
        };

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

        tracing::info!("configuration updated and reloaded live");
        Ok(snapshot)
    }

    /// Reloads `config.toml` from disk, preserving the last valid configuration on error.
    pub fn reload_from_disk(&self) -> Result<ConfigSnapshotPayload, (String, String)> {
        match LoadedConfig::load(
            &self.paths.config_file,
            self.env.as_ref(),
            &self.cli_overrides,
        ) {
            Ok(reloaded) => {
                let (snapshot, state_changed_status, alerts_changed) = {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    let old_armed = is_armed_locked(&guard, self.clock.as_ref());
                    let old_dry_run = guard.dry_run;

                    guard.loaded_config = reloaded;
                    guard.dry_run = guard.loaded_config.config.actions.dry_run;

                    let cleared = clear_reload_alert(&mut guard.alerts);
                    let alerts_opt = cleared.then(|| guard.alerts.clone());

                    let new_armed = is_armed_locked(&guard, self.clock.as_ref());
                    let status_opt = (old_armed != new_armed || old_dry_run != guard.dry_run)
                        .then(|| self.build_status_locked(&guard));

                    let snapshot = self.build_config_snapshot_locked(&guard);
                    (snapshot, status_opt, alerts_opt)
                };

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

                tracing::info!("configuration reloaded");
                Ok(snapshot)
            }
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
                Err((error_codes::INVALID_CONFIG.to_string(), err_msg))
            }
        }
    }

    fn publish_state_changed(&self, status: &DaemonStatus) {
        if let Ok(val) = serde_json::to_value(status) {
            self.broker.publish_control("StateChanged", val);
        }
    }

    /// Publishes an `HrSample` stream event through the drop-oldest broker lane.
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
    if let Some(existing) = alerts.iter_mut().find(|a| a.id == CONFIG_RELOAD_ALERT_ID) {
        if existing.message == message {
            return false;
        }
        existing.message = message;
        return true;
    }

    alerts.push(DaemonAlert {
        id: CONFIG_RELOAD_ALERT_ID.to_string(),
        kind: CONFIG_RELOAD_ALERT_ID.to_string(),
        severity: AlertSeverity::Error,
        message,
        dismissible: true,
    });
    true
}
