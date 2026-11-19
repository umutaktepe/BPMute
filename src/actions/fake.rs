use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::config::PowerAction;

use super::traits::{
    ActionError, ActionExecutor, BoxFuture, CommandOutput, NotificationEvent, Notifier,
    PowerActionResult, Urgency, is_player_ignored,
};

#[derive(Default)]
struct FakeActionExecutorInner {
    available_players: Vec<String>,
    playing_players: Vec<String>,
    events: Vec<String>,
    command_override: Option<Result<CommandOutput, ActionError>>,
    power_action_override: Option<Result<PowerActionResult, ActionError>>,
    lock_screen_error: Option<String>,
    resume_tx: Option<mpsc::Sender<()>>,
    resume_rx: Option<mpsc::Receiver<()>>,
}

#[derive(Clone)]
pub struct FakeActionExecutor {
    inner: Arc<Mutex<FakeActionExecutorInner>>,
}

impl Default for FakeActionExecutor {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(1);
        Self {
            inner: Arc::new(Mutex::new(FakeActionExecutorInner {
                available_players: Vec::new(),
                playing_players: Vec::new(),
                events: Vec::new(),
                command_override: None,
                power_action_override: None,
                lock_screen_error: None,
                resume_tx: Some(tx),
                resume_rx: Some(rx),
            })),
        }
    }
}

impl FakeActionExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trigger_mpris_resume(&self) {
        let guard = self.inner.lock().expect("mutex poisoned");
        if let Some(ref tx) = guard.resume_tx {
            let _ = tx.try_send(());
        }
    }

    pub fn set_available_players(&self, players: &[&str]) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.available_players = players.iter().map(|s| s.to_string()).collect();
    }

    pub fn set_playing_players(&self, players: &[&str]) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.playing_players = players.iter().map(|s| s.to_string()).collect();
    }

    pub fn set_command_override(&self, res: Result<CommandOutput, ActionError>) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.command_override = Some(res);
    }

    pub fn set_power_action_override(&self, res: Result<PowerActionResult, ActionError>) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.power_action_override = Some(res);
    }

    pub fn set_lock_screen_error(&self, err: Option<String>) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.lock_screen_error = err;
    }

    pub fn events(&self) -> Vec<String> {
        let guard = self.inner.lock().expect("mutex poisoned");
        guard.events.clone()
    }

    pub fn clear_events(&self) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.events.clear();
    }
}

impl ActionExecutor for FakeActionExecutor {
    fn pause_mpris_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<String>, ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            let mut paused = Vec::new();
            for p in &guard.playing_players {
                if !is_player_ignored(p, ignore_players) {
                    paused.push(p.clone());
                }
            }
            guard
                .events
                .push(format!("pause_mpris:[{}]", paused.join(", ")));
            Ok(paused)
        })
    }

    fn has_playing_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<bool, ActionError>> {
        Box::pin(async move {
            let guard = self.inner.lock().expect("mutex poisoned");
            for p in &guard.playing_players {
                if !is_player_ignored(p, ignore_players) {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    fn run_custom_command<'a>(
        &'a self,
        command: &'a [String],
        _env: &'a [(&'a str, &'a str)],
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<CommandOutput, ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            let cmd_str = command.join(" ");
            guard.events.push(format!("custom_command:{cmd_str}"));
            if let Some(res) = guard.command_override.clone() {
                res
            } else {
                Ok(CommandOutput {
                    exit_code: Some(0),
                    success: true,
                    stdout_truncated: String::new(),
                    stderr_truncated: String::new(),
                })
            }
        })
    }

    fn lock_screen<'a>(&'a self) -> BoxFuture<'a, Result<(), ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            guard.events.push("lock_screen".to_string());
            if let Some(ref err) = guard.lock_screen_error {
                Err(ActionError::LockScreen(err.clone()))
            } else {
                Ok(())
            }
        })
    }

    fn execute_power_action<'a>(
        &'a self,
        action: PowerAction,
    ) -> BoxFuture<'a, Result<PowerActionResult, ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            guard.events.push(format!("power_action:{action}"));
            if let Some(res) = guard.power_action_override.clone() {
                res
            } else {
                Ok(PowerActionResult::Executed)
            }
        })
    }

    fn watch_mpris_resume<'a>(&'a self, _paused_players: &'a [String]) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let mut rx = {
                let mut guard = self.inner.lock().expect("mutex poisoned");
                guard.events.push("watch_mpris_resume".to_string());
                guard.resume_rx.take()
            };
            if let Some(ref mut receiver) = rx {
                tokio::select! {
                    res = receiver.recv() => res.is_some(),
                    _ = tokio::time::sleep(Duration::from_secs(300)) => false,
                }
            } else {
                tokio::time::sleep(Duration::from_secs(300)).await;
                false
            }
        })
    }
}

#[derive(Default)]
struct FakeNotifierInner {
    events: Vec<String>,
    next_id: u32,
    active_notice_id: Option<u32>,
}

pub struct FakeNotifier {
    inner: Arc<Mutex<FakeNotifierInner>>,
    action_tx: mpsc::Sender<NotificationEvent>,
    action_rx: Mutex<Option<mpsc::Receiver<NotificationEvent>>>,
}

impl Default for FakeNotifier {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel(32);
        Self {
            inner: Arc::new(Mutex::new(FakeNotifierInner {
                events: Vec::new(),
                next_id: 1,
                active_notice_id: None,
            })),
            action_tx: tx,
            action_rx: Mutex::new(Some(rx)),
        }
    }
}

impl FakeNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<String> {
        let guard = self.inner.lock().expect("mutex poisoned");
        guard.events.clone()
    }

    pub fn clear_events(&self) {
        let mut guard = self.inner.lock().expect("mutex poisoned");
        guard.events.clear();
    }

    pub fn active_notice_id(&self) -> Option<u32> {
        let guard = self.inner.lock().expect("mutex poisoned");
        guard.active_notice_id
    }

    pub fn emit_action(&self, id: u32, action_key: &str) {
        let _ = self.action_tx.try_send(NotificationEvent::ActionInvoked {
            id,
            action_key: action_key.to_string(),
        });
    }

    pub fn emit_closed(&self, id: u32, reason: u32) {
        let _ = self
            .action_tx
            .try_send(NotificationEvent::NotificationClosed { id, reason });
    }
}

impl Notifier for FakeNotifier {
    fn show_grace_notice<'a>(
        &'a self,
        replaces_id: u32,
        remaining_sec: u32,
        total_sec: u32,
        urgency: Urgency,
        dry_run: bool,
    ) -> BoxFuture<'a, Result<u32, ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            let id = if replaces_id != 0 {
                replaces_id
            } else {
                let id = guard.next_id;
                guard.next_id = guard.next_id.saturating_add(1);
                id
            };
            guard.active_notice_id = Some(id);
            guard.events.push(format!(
                "grace_notice: id={id}, rem={remaining_sec}/{total_sec}, urg={:?}, dry={dry_run}",
                urgency
            ));
            Ok(id)
        })
    }

    fn close_notice<'a>(&'a self, id: u32) -> BoxFuture<'a, Result<(), ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            if guard.active_notice_id == Some(id) {
                guard.active_notice_id = None;
            }
            guard.events.push(format!("close_notice:{id}"));
            Ok(())
        })
    }

    fn show_warning<'a>(
        &'a self,
        summary: &'a str,
        body: &'a str,
        transient: bool,
    ) -> BoxFuture<'a, Result<u32, ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            let id = guard.next_id;
            guard.next_id = guard.next_id.saturating_add(1);
            guard.events.push(format!(
                "warning_notice: id={id}, sum={summary}, body={body}, transient={transient}"
            ));
            Ok(id)
        })
    }

    fn show_info<'a>(
        &'a self,
        summary: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<u32, ActionError>> {
        Box::pin(async move {
            let mut guard = self.inner.lock().expect("mutex poisoned");
            let id = guard.next_id;
            guard.next_id = guard.next_id.saturating_add(1);
            guard
                .events
                .push(format!("info_notice: id={id}, sum={summary}, body={body}"));
            Ok(id)
        })
    }

    fn subscribe_actions(&self) -> mpsc::Receiver<NotificationEvent> {
        let mut guard = self.action_rx.lock().expect("mutex poisoned");
        guard.take().unwrap_or_else(|| mpsc::channel(1).1)
    }
}
