use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use super::{DaemonCore, InnerState, ipc_err};
use crate::daemon::EngineControlEvent;
use crate::daemon::clock::DaemonClock;
use crate::ipc::protocol::ErrorPayload;
use crate::ipc::protocol::{
    DaemonStatus, MonitoringState, PROTOCOL_VERSION, PauseMode, PauseParams, error_codes,
};

impl DaemonCore {
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
            session_close_deadline_ms: guard.session_close_deadline_ms,
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
                let target_clock = guard.loaded_config.config.schedule.end;
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

    /// Synchronizes Active Hours tracking and emits `StateChanged` when `Standby` exits Active Hours.
    pub fn notify_standby_active_hours_expired(&self) {
        let status_opt = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if guard.loaded_config.config.schedule.active_hours_enabled
                && guard.last_within_active_hours != Some(false)
            {
                guard.last_within_active_hours = Some(false);
                Some(self.build_status_locked(&guard))
            } else {
                None
            }
        };
        if let Some(status) = status_opt {
            self.publish_state_changed(&status);
        }
    }

    /// Returns whether the current local time falls within configured Active Hours
    /// (or `true` if Active Hours are disabled).
    pub fn is_within_active_hours(&self) -> bool {
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        if !guard.loaded_config.config.schedule.active_hours_enabled {
            return true;
        }
        guard
            .loaded_config
            .config
            .schedule
            .is_within_active_hours(self.clock.now_local().time())
    }

    /// Evaluates clock transitions against configured Active Hours, updating `InnerState` and
    /// signaling `MonitoringContext` if the active-hours boundary was crossed.
    pub fn check_active_hours_transition(&self, ts_ms: i64) {
        let action = {
            let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
            if !guard.loaded_config.config.schedule.active_hours_enabled {
                guard.last_within_active_hours = None;
                None
            } else {
                let now_within = guard
                    .loaded_config
                    .config
                    .schedule
                    .is_within_active_hours(self.clock.now_local().time());
                let prev_within = guard.last_within_active_hours;
                guard.last_within_active_hours = Some(now_within);

                match (prev_within, now_within) {
                    (Some(false), true) => Some(EngineControlEvent::ActiveHoursEntered { ts_ms }),
                    (Some(true), false) => Some(EngineControlEvent::ActiveHoursExpired { ts_ms }),
                    _ => None,
                }
            }
        };

        if let Some(event) = action {
            let _ = self.engine_ctrl_tx.try_send(event);
        }
    }
}

pub(super) fn is_armed_locked(guard: &InnerState, clock: &dyn DaemonClock) -> bool {
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
