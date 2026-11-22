use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::actions::grace::{GraceControlCmd, GraceRunOutcome, GraceRunParams};
use crate::actions::pipeline::{PipelineContext, PipelineResult};
use crate::actions::traits::PowerActionResult;
use crate::config::PowerAction;
use crate::daemon::EngineControlEvent;
use crate::idle::{compute_effective_grace_period, is_power_action_refused};
use crate::ipc::protocol::{ErrorPayload, error_codes};
use crate::sleep_engine::{DetectorState, GraceOutcome, GraceResult};

use super::{DaemonCore, ipc_err};

impl DaemonCore {
    /// Returns whether the daemon is currently running an active Grace Period.
    pub fn is_in_grace_period(&self) -> bool {
        self.active_grace_tx.lock().unwrap().is_some()
    }

    /// Returns whether sleep triggers are currently armed.
    pub fn is_armed(&self) -> bool {
        let guard = self.inner.lock().expect("daemon state mutex poisoned");
        super::is_armed_locked(&guard, self.clock.as_ref())
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

    pub fn clear_active_grace_for(&self, tx: &mpsc::Sender<GraceControlCmd>) {
        let mut guard_tx = self.active_grace_tx.lock().unwrap();
        if guard_tx.as_ref().is_some_and(|cur| cur.same_channel(tx)) {
            let mut guard_p = self.active_grace_params.lock().unwrap();
            *guard_tx = None;
            *guard_p = None;
        }
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
        let tx_owner = tx.clone();
        self.set_active_grace(tx, params.clone());

        let core = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = core.coordinator.run(params, rx).await;
            core.clear_active_grace_for(&tx_owner);
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

    async fn persist_active_sleep_trigger(
        &self,
        session_id: Option<&str>,
        sleeping_ts_ms: i64,
        action: PowerAction,
    ) {
        let Some(sid) = session_id else {
            return;
        };
        if let Err(err) = self
            .persistence
            .mark_session_sleep_trigger(sid.to_string(), sleeping_ts_ms, action)
            .await
        {
            tracing::warn!(
                error = %err,
                ?action,
                "failed to persist active session sleep trigger state"
            );
        }
    }

    pub async fn deliver_grace_outcome(&self, outcome: GraceOutcome) {
        let _ = self
            .engine_ctrl_tx
            .send(EngineControlEvent::GraceOutcome(outcome))
            .await;
    }

    async fn deliver_silent_expired(&self, ts_ms: i64) {
        if !self.is_dry_run() {
            self.deliver_grace_outcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::Expired,
            })
            .await;
        }
    }

    pub async fn handle_confirmed_sleep_onset(self: &Arc<Self>, ts_ms: i64) {
        let (is_paused, cfg, active_session_id, wearable_id) = {
            let guard = self.inner.lock().unwrap();
            (
                guard.paused,
                guard.loaded_config.config.clone(),
                guard.active_session_id.clone(),
                guard
                    .active_wearable_id
                    .clone()
                    .unwrap_or_else(|| "default-wearable".to_string()),
            )
        };
        let session_id = active_session_id
            .clone()
            .unwrap_or_else(|| "default-session".to_string());

        if is_paused {
            tracing::info!(
                "sleep onset confirmed but triggers are paused; transitioning silently to Sleeping"
            );
            self.deliver_silent_expired(ts_ms).await;
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
            self.deliver_silent_expired(ts_ms).await;
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
                self.deliver_silent_expired(ts_ms).await;
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
        let tx_owner = tx.clone();
        self.set_active_grace(tx, params.clone());

        let core = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = core.coordinator.run(params, rx).await;
            core.clear_active_grace_for(&tx_owner);
            match outcome {
                GraceRunOutcome::Completed(gr) => {
                    if gr.outcome == GraceResult::Expired {
                        core.set_detector_phase(
                            DetectorState::Sleeping,
                            Some(session_id.clone()),
                            Some(wearable_id.clone()),
                        );
                        if !core.is_dry_run() {
                            *core.sleep_triggered_at.lock().unwrap() = Some(gr.ts_ms);
                            *core.trigger_power_action.lock().unwrap() = eff_actions.power_action;
                            // Deliver Sleeping transition and persist trigger metadata before any
                            // destructive power action can trigger SIGTERM or OS shutdown.
                            core.deliver_grace_outcome(gr).await;
                            if matches!(
                                eff_actions.power_action,
                                PowerAction::Suspend | PowerAction::Poweroff
                            ) {
                                core.persist_active_sleep_trigger(
                                    active_session_id.as_deref(),
                                    gr.ts_ms,
                                    eff_actions.power_action,
                                )
                                .await;
                            }
                        }
                        let ctx = PipelineContext {
                            session_id: session_id.clone(),
                            wearable_id: wearable_id.clone(),
                            event: "sleep_trigger".to_string(),
                            dry_run: core.is_dry_run(),
                        };
                        let res = match core.pipeline.execute(&ctx, &eff_actions).await {
                            Ok(res) => res,
                            Err(err) => {
                                tracing::warn!(error = %err, "sleep action pipeline aborted");
                                PipelineResult::default()
                            }
                        };
                        if !core.is_dry_run()
                            && eff_actions.power_action != PowerAction::None
                            && !matches!(res.power_action_result, Some(PowerActionResult::Executed))
                        {
                            *core.trigger_power_action.lock().unwrap() = PowerAction::None;
                            core.persist_active_sleep_trigger(
                                active_session_id.as_deref(),
                                gr.ts_ms,
                                PowerAction::None,
                            )
                            .await;
                        }
                        match res.power_action_result {
                            Some(PowerActionResult::RefusedInhibited(ref reason)) => {
                                core.set_logind_inhibited_alert(Some(reason));
                            }
                            Some(PowerActionResult::Executed) => {
                                core.set_logind_inhibited_alert(None);
                            }
                            _ => {}
                        }

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
                    } else {
                        if matches!(
                            gr.outcome,
                            GraceResult::Cancelled
                                | GraceResult::Snoozed { .. }
                                | GraceResult::AbortedNoPenalty
                        ) {
                            let is_still_active = {
                                let guard = core.inner.lock().expect("daemon state mutex poisoned");
                                guard.active_session_id.as_deref() == Some(&session_id)
                                    && guard.detector_state != DetectorState::Idle
                            };
                            if is_still_active {
                                core.set_detector_phase(
                                    DetectorState::Monitoring,
                                    Some(session_id.clone()),
                                    Some(wearable_id.clone()),
                                );
                            }
                        }
                        if !core.is_dry_run() {
                            core.deliver_grace_outcome(gr).await;
                        }
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
}
