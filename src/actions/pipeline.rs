use std::sync::Arc;
use std::time::Duration;

use crate::config::{ActionsConfig, PowerAction};
use crate::daemon::DaemonClock;

use super::traits::{ActionError, ActionExecutor, Notifier, PowerActionResult};

/// Context parameters passed into the Sleep Action Pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineContext {
    pub session_id: String,
    pub wearable_id: String,
    pub event: String,
    pub dry_run: bool,
}

/// Execution outcome report from [`SleepActionPipeline::execute`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipelineResult {
    pub paused_players: Vec<String>,
    pub command_executed: bool,
    pub screen_locked: bool,
    pub power_action_result: Option<PowerActionResult>,
}

/// Ordered coordinator executing desktop and system sleep actions (ADR-0006).
pub struct SleepActionPipeline {
    executor: Arc<dyn ActionExecutor>,
    notifier: Arc<dyn Notifier>,
    clock: Arc<dyn DaemonClock>,
}

impl SleepActionPipeline {
    /// Creates a new pipeline bound to injected action executor, notification, and clock interfaces.
    pub fn new(
        executor: Arc<dyn ActionExecutor>,
        notifier: Arc<dyn Notifier>,
        clock: Arc<dyn DaemonClock>,
    ) -> Self {
        Self {
            executor,
            notifier,
            clock,
        }
    }

    /// Returns a reference to the injected [`DaemonClock`].
    pub fn clock(&self) -> &Arc<dyn DaemonClock> {
        &self.clock
    }

    /// Executes the ordered Sleep Action Pipeline:
    /// 1. Concurrent MPRIS2 `Pause()` on non-ignored playing players
    /// 2. `1.5 s` settle delay if `≥ 1` player was paused and subsequent steps follow
    /// 3. Direct `exec` argv `custom_command` (bounded timeout, env injection, abort-on-failure veto)
    /// 4. `logind` `Session.Lock()` with fallback to `Manager.LockSessions()`
    /// 5. Unprivileged `Suspend(false)`/`PowerOff(false)` respecting blocking inhibitors
    pub async fn execute(
        &self,
        ctx: &PipelineContext,
        config: &ActionsConfig,
    ) -> Result<PipelineResult, ActionError> {
        let is_dry_run = ctx.dry_run || config.dry_run;
        if is_dry_run {
            tracing::info!(
                session_id = %ctx.session_id,
                wearable_id = %ctx.wearable_id,
                "[Dry Run] skipping Sleep Action Pipeline side effects"
            );
            return Ok(PipelineResult::default());
        }

        let mut result = PipelineResult::default();

        // 1. Concurrent MPRIS2 Pause()
        if config.pause_media {
            let paused = self
                .executor
                .pause_mpris_players(&config.ignore_players)
                .await?;
            result.paused_players = paused;
        }

        // 2. 1.5s Settle delay only if >= 1 player was paused and subsequent steps follow
        let has_subsequent_step = (!config.custom_command.is_empty())
            || config.lock_screen
            || config.power_action != PowerAction::None;

        if !result.paused_players.is_empty() && has_subsequent_step {
            tracing::debug!("waiting 1.5 s settle delay after pausing media players");
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }

        // 3. Direct exec argv custom_command
        if let Some(executable) = config.custom_command.first()
            && !executable.trim().is_empty()
        {
            let timeout = Duration::from_secs(u64::from(config.custom_command_timeout_sec));
            let pipeline_str = format!(
                "pause_media={},custom_command=true,lock_screen={},power_action={}",
                config.pause_media, config.lock_screen, config.power_action
            );
            let env_pairs = [
                ("BPMUTE_SESSION_ID", ctx.session_id.as_str()),
                ("BPMUTE_EVENT", ctx.event.as_str()),
                ("BPMUTE_PIPELINE", pipeline_str.as_str()),
                ("BPMUTE_WEARABLE_ID", ctx.wearable_id.as_str()),
            ];

            match self
                .executor
                .run_custom_command(&config.custom_command, &env_pairs, timeout)
                .await
            {
                Ok(out) => {
                    result.command_executed = true;
                    if !out.success {
                        if config.abort_on_hook_failure {
                            tracing::warn!(
                                exit_code = ?out.exit_code,
                                stdout = %out.stdout_truncated,
                                stderr = %out.stderr_truncated,
                                "custom_command hook failed with abort_on_hook_failure=true; vetoing remaining steps"
                            );
                            return Ok(result);
                        } else {
                            tracing::warn!(
                                exit_code = ?out.exit_code,
                                "custom_command hook failed; continuing pipeline because abort_on_hook_failure=false"
                            );
                        }
                    }
                }
                Err(err) => {
                    if config.abort_on_hook_failure {
                        tracing::error!(
                            error = %err,
                            "custom_command execution error with abort_on_hook_failure=true; vetoing remaining steps"
                        );
                        return Err(err);
                    } else {
                        tracing::warn!(
                            error = %err,
                            "custom_command execution error; continuing pipeline because abort_on_hook_failure=false"
                        );
                    }
                }
            }
        }

        // 4. logind Session.Lock() / Manager.LockSessions()
        if config.lock_screen {
            self.executor.lock_screen().await?;
            result.screen_locked = true;
        }

        // 5. Terminal power action
        if config.power_action != PowerAction::None {
            let power_res = self
                .executor
                .execute_power_action(config.power_action)
                .await?;
            match &power_res {
                PowerActionResult::RefusedInhibited(reason) => {
                    tracing::warn!(
                        action = %config.power_action,
                        reason = %reason,
                        "terminal power action refused by active inhibitors; preserving paused media"
                    );
                    let _ = self
                        .notifier
                        .show_warning(
                            "Sleep action inhibited",
                            &format!("Host power action refused by inhibitor: {reason}"),
                            false,
                        )
                        .await;
                }
                PowerActionResult::Failed(err) => {
                    tracing::error!(
                        action = %config.power_action,
                        error = %err,
                        "terminal power action failed; preserving paused media"
                    );
                    let _ = self
                        .notifier
                        .show_warning(
                            "Sleep action failed",
                            &format!("Host power action failed: {err}"),
                            false,
                        )
                        .await;
                }
                PowerActionResult::Executed => {
                    tracing::info!(action = %config.power_action, "terminal power action executed");
                }
            }
            result.power_action_result = Some(power_res);
        }

        Ok(result)
    }
}
