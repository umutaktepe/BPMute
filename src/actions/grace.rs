use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::{ActionsConfig, PowerAction};
use crate::daemon::DaemonClock;
use crate::idle::IdleProbe;
use crate::ipc::protocol::GraceTickEvent;
use crate::ipc::pubsub::EventBroker;
use crate::sleep_engine::{GraceOutcome, GraceResult};

use super::traits::{GracePhase, NotificationEvent, Notifier, Urgency};

/// Fixed non-resetting duration of the `Activity Hold` phase once desktop activity is detected.
const ACTIVITY_HOLD_SEC: u32 = 10;

/// Control messages delivered to [`GraceCoordinator`] while a Grace Period is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraceControlCmd {
    Cancel,
    Snooze { duration_sec: Option<u32> },
    ArousalAbort,
    SignalLost,
    PrepareForSleep,
}

/// Parameters for starting a cancellable Grace Period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraceRunParams {
    pub session_id: String,
    pub wearable_id: String,
    pub config: ActionsConfig,
    pub dry_run: bool,
}

/// Execution outcome of [`GraceCoordinator::run`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraceRunOutcome {
    Completed(GraceOutcome),
    AsymmetricSignalLost,
}

/// Manages the cancellable Grace Period countdown, notification updates, and cancellation hooks.
pub struct GraceCoordinator {
    notifier: Arc<dyn Notifier>,
    idle_probe: Arc<dyn IdleProbe>,
    clock: Arc<dyn DaemonClock>,
    broker: Arc<EventBroker>,
}

impl GraceCoordinator {
    /// Creates a new coordinator with injected notification, clock, and event broker boundaries.
    pub fn new(
        notifier: Arc<dyn Notifier>,
        idle_probe: Arc<dyn IdleProbe>,
        clock: Arc<dyn DaemonClock>,
        broker: Arc<EventBroker>,
    ) -> Self {
        Self {
            notifier,
            idle_probe,
            clock,
            broker,
        }
    }

    /// Runs the cancellable Grace Period countdown loop:
    /// - Manages single `replaces_id` notification updated every 5 s, then every 1 s in final 10 s
    ///   during `Countdown`, and every 1 s during `ActivityHold`
    /// - Transitions from `Countdown` to a fixed 10 s `ActivityHold` on the first post-settle idle event
    /// - Emits `GraceTick` push events every 1 s (and immediately upon entering `ActivityHold`)
    /// - Handles `Cancel`, `Snooze`, `NotificationClosed` (non-cancelling), and asymmetric `Signal Lost`
    /// - Closes notification via `CloseNotification(id)` upon exit
    pub async fn run(
        &self,
        params: GraceRunParams,
        mut ctrl_rx: mpsc::Receiver<GraceControlCmd>,
    ) -> GraceRunOutcome {
        let total_sec = params.config.effective_grace_period_sec();
        let mut remaining_sec = total_sec;
        let mut phase = GracePhase::Countdown;

        let urgency = match params.config.power_action {
            PowerAction::None => Urgency::Normal,
            PowerAction::Suspend | PowerAction::Poweroff => Urgency::Critical,
        };

        let mut notif_rx = self.notifier.subscribe_actions();
        let mut active_notice_id = 0;
        let mut notice_dismissed = false;
        self.refresh_notice(
            &mut active_notice_id,
            notice_dismissed,
            remaining_sec,
            total_sec,
            phase,
            urgency,
            params.dry_run,
        )
        .await;

        let mut idle_rx = self
            .idle_probe
            .watch_grace_period_activity(Duration::from_secs(1))
            .await
            .ok();

        // Publish initial tick at start
        self.publish_tick(&params, remaining_sec, total_sec, phase);
        let mut tick_deadline = tokio::time::Instant::now() + Duration::from_secs(1);

        loop {
            tokio::select! {
                biased;

                cmd_opt = ctrl_rx.recv() => {
                    let Some(cmd) = cmd_opt else {
                        let now_ms = self.clock.now_utc().timestamp_millis();
                        self.close_notice_if_active(active_notice_id).await;
                        self.publish_tick(&params, 0, total_sec, phase);
                        return GraceRunOutcome::Completed(GraceOutcome {
                            ts_ms: now_ms,
                            outcome: GraceResult::AbortedNoPenalty,
                        });
                    };
                    let now_ms = self.clock.now_utc().timestamp_millis();
                    self.close_notice_if_active(active_notice_id).await;
                    self.publish_tick(&params, 0, total_sec, phase);
                    match cmd {
                        GraceControlCmd::Cancel => {
                            return GraceRunOutcome::Completed(GraceOutcome {
                                ts_ms: now_ms,
                                outcome: GraceResult::Cancelled,
                            });
                        }
                        GraceControlCmd::Snooze { duration_sec } => {
                            let dur = duration_sec.unwrap_or(params.config.snooze_duration_min * 60);
                            return GraceRunOutcome::Completed(GraceOutcome {
                                ts_ms: now_ms,
                                outcome: GraceResult::Snoozed { duration_sec: dur },
                            });
                        }
                        GraceControlCmd::ArousalAbort | GraceControlCmd::PrepareForSleep => {
                            return GraceRunOutcome::Completed(GraceOutcome {
                                ts_ms: now_ms,
                                outcome: GraceResult::AbortedNoPenalty,
                            });
                        }
                        GraceControlCmd::SignalLost => {
                            let _ = self.notifier.show_info(
                                "Signal Lost",
                                "Wearable signal lost during Grace Period. Skipping disruptive actions.",
                            ).await;
                            return GraceRunOutcome::AsymmetricSignalLost;
                        }
                    }
                }

                Some(()) = async {
                    match idle_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    // Single-shot trigger: stop consuming idle events so continuous input
                    // never resets or extends the fixed 10 s Activity Hold timer.
                    phase = GracePhase::ActivityHold;
                    remaining_sec = ACTIVITY_HOLD_SEC;
                    idle_rx = None;
                    tick_deadline = tokio::time::Instant::now() + Duration::from_secs(1);

                    self.publish_tick(&params, remaining_sec, total_sec, phase);
                    tracing::info!(
                        "desktop user activity detected during Grace Period; entering 10s Activity Hold"
                    );

                    self.refresh_notice(
                        &mut active_notice_id,
                        notice_dismissed,
                        remaining_sec,
                        total_sec,
                        phase,
                        urgency,
                        params.dry_run,
                    )
                    .await;
                }

                Some(notif_ev) = notif_rx.recv() => {
                    match notif_ev {
                        NotificationEvent::ActionInvoked { id, action_key } => {
                            if active_notice_id != 0 && id != active_notice_id {
                                continue;
                            }
                            let now_ms = self.clock.now_utc().timestamp_millis();
                            if action_key == "cancel" || action_key == "default" {
                                self.close_notice_if_active(active_notice_id).await;
                                self.publish_tick(&params, 0, total_sec, phase);
                                return GraceRunOutcome::Completed(GraceOutcome {
                                    ts_ms: now_ms,
                                    outcome: GraceResult::Cancelled,
                                });
                            } else if action_key == "snooze" {
                                self.close_notice_if_active(active_notice_id).await;
                                self.publish_tick(&params, 0, total_sec, phase);
                                let dur = params.config.snooze_duration_min * 60;
                                return GraceRunOutcome::Completed(GraceOutcome {
                                    ts_ms: now_ms,
                                    outcome: GraceResult::Snoozed { duration_sec: dur },
                                });
                            }
                        }
                        NotificationEvent::NotificationClosed { id, reason } => {
                            if active_notice_id != 0 && id == active_notice_id {
                                active_notice_id = 0;
                                if reason == 2 {
                                    // Non-cancelling: user dismiss / swipe keeps countdown active
                                    // without repeatedly re-popping the dismissed notification.
                                    notice_dismissed = true;
                                }
                            }
                        }
                    }
                }

                _ = tokio::time::sleep_until(tick_deadline) => {
                    tick_deadline += Duration::from_secs(1);
                    remaining_sec = remaining_sec.saturating_sub(1);
                    self.publish_tick(&params, remaining_sec, total_sec, phase);

                    if remaining_sec == 0 {
                        let now_ms = self.clock.now_utc().timestamp_millis();
                        self.close_notice_if_active(active_notice_id).await;
                        let outcome = match phase {
                            GracePhase::Countdown => GraceResult::Expired,
                            GracePhase::ActivityHold => GraceResult::Cancelled,
                        };
                        return GraceRunOutcome::Completed(GraceOutcome {
                            ts_ms: now_ms,
                            outcome,
                        });
                    }

                    let should_refresh_notice = match phase {
                        GracePhase::Countdown => {
                            remaining_sec.is_multiple_of(5) || remaining_sec <= 10
                        }
                        GracePhase::ActivityHold => true,
                    };

                    if should_refresh_notice {
                        self.refresh_notice(
                            &mut active_notice_id,
                            notice_dismissed,
                            remaining_sec,
                            total_sec,
                            phase,
                            urgency,
                            params.dry_run,
                        )
                        .await;
                    }
                }
            }
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "forwards Grace Period notification fields without allocating a transient struct"
    )]
    async fn refresh_notice(
        &self,
        active_notice_id: &mut u32,
        notice_dismissed: bool,
        remaining_sec: u32,
        total_sec: u32,
        phase: GracePhase,
        urgency: Urgency,
        dry_run: bool,
    ) {
        if !notice_dismissed
            && let Ok(new_id) = self
                .notifier
                .show_grace_notice(
                    *active_notice_id,
                    remaining_sec,
                    total_sec,
                    phase,
                    urgency,
                    dry_run,
                )
                .await
        {
            *active_notice_id = new_id;
        }
    }

    async fn close_notice_if_active(&self, id: u32) {
        if id != 0 {
            let _ = self.notifier.close_notice(id).await;
        }
    }

    fn publish_tick(
        &self,
        params: &GraceRunParams,
        remaining_sec: u32,
        total_sec: u32,
        phase: GracePhase,
    ) {
        let tick = GraceTickEvent {
            session_id: params.session_id.clone(),
            remaining_sec,
            total_sec,
            power_action: params.config.power_action,
            dry_run: params.dry_run || params.config.dry_run,
            planned_steps: params.config.planned_pipeline_steps(),
            activity_hold: phase == GracePhase::ActivityHold,
        };
        if let Ok(v) = serde_json::to_value(tick) {
            self.broker.publish_control("GraceTick", v);
        }
    }
}
