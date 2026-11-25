use std::time::Duration;

use tokio::sync::mpsc;
use zbus::proxy;
use zbus::zvariant::{OwnedFd, Value};

use crate::config::PowerAction;

use super::traits::{
    ActionError, ActionExecutor, BoxFuture, CommandOutput, PowerActionResult, is_player_ignored,
};

#[proxy(
    default_service = "org.freedesktop.DBus",
    default_path = "/org/freedesktop/DBus",
    interface = "org.freedesktop.DBus"
)]
trait DBus1 {
    fn list_names(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait MediaPlayer2Player {
    fn pause(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn can_pause(&self) -> zbus::Result<bool>;
}

#[proxy(
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    interface = "org.freedesktop.login1.Manager"
)]
pub trait LogindManager {
    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;
    fn lock_sessions(&self) -> zbus::Result<()>;
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<OwnedFd>;
    fn can_suspend(&self) -> zbus::Result<String>;
    fn can_power_off(&self) -> zbus::Result<String>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

#[proxy(
    default_service = "org.freedesktop.login1",
    interface = "org.freedesktop.login1.Session"
)]
trait LogindSession {
    fn lock(&self) -> zbus::Result<()>;
}

const DEFAULT_MPRIS_WATCH_TIMEOUT: Duration = Duration::from_secs(300);
const MPRIS_OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const MPRIS_STATUS_PLAYING: &str = "Playing";
const MPRIS_STATUS_PAUSED: &str = "Paused";
const MPRIS_STATUS_STOPPED: &str = "Stopped";

/// Production system action executor communicating over session and system D-Bus.
#[derive(Debug, Clone)]
pub struct SystemActionExecutor {
    session_bus_address: Option<String>,
    mpris_watch_timeout: Duration,
}

impl Default for SystemActionExecutor {
    fn default() -> Self {
        Self {
            session_bus_address: None,
            mpris_watch_timeout: DEFAULT_MPRIS_WATCH_TIMEOUT,
        }
    }
}

impl SystemActionExecutor {
    /// Creates an executor backed by the ambient session and system D-Bus.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an executor targeting a specific session D-Bus address (used in hermetic tests).
    pub fn with_session_bus_address(address: impl Into<String>) -> Self {
        Self {
            session_bus_address: Some(address.into()),
            ..Self::default()
        }
    }

    /// Overrides the MPRIS post-trigger watch timeout (defaults to 300 s).
    pub fn with_mpris_watch_timeout(mut self, timeout: Duration) -> Self {
        self.mpris_watch_timeout = timeout;
        self
    }

    async fn session_conn(&self) -> Result<zbus::Connection, zbus::Error> {
        match &self.session_bus_address {
            Some(addr) => {
                zbus::connection::Builder::address(addr.as_str())?
                    .build()
                    .await
            }
            None => zbus::Connection::session().await,
        }
    }
}

impl ActionExecutor for SystemActionExecutor {
    fn pause_mpris_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<String>, ActionError>> {
        Box::pin(async move {
            let conn = match self.session_conn().await {
                Ok(c) => c,
                Err(err) => return Err(ActionError::Mpris(format!("session bus error: {err}"))),
            };

            let dbus_proxy = match DBus1Proxy::new(&conn).await {
                Ok(p) => p,
                Err(err) => return Err(ActionError::Mpris(format!("DBus1Proxy error: {err}"))),
            };

            let names = match dbus_proxy.list_names().await {
                Ok(n) => n,
                Err(err) => return Err(ActionError::Mpris(format!("list_names error: {err}"))),
            };

            let mut targets = Vec::new();
            for name in names {
                if name.starts_with("org.mpris.MediaPlayer2.")
                    && !is_player_ignored(&name, ignore_players)
                {
                    targets.push(name);
                }
            }

            if targets.is_empty() {
                return Ok(Vec::new());
            }

            let mut tasks = Vec::new();
            for bus_name in targets {
                let c = conn.clone();
                tasks.push(tokio::spawn(async move {
                    let opt: Option<String> = async {
                        let builder = MediaPlayer2PlayerProxy::builder(&c)
                            .destination(bus_name.as_str())
                            .ok()?;
                        let player_proxy = builder.build().await.ok()?;

                        let status = tokio::time::timeout(
                            Duration::from_secs(2),
                            player_proxy.playback_status(),
                        )
                        .await
                        .ok()?
                        .ok()?;

                        if status != MPRIS_STATUS_PLAYING {
                            return None;
                        }

                        if let Ok(Ok(can_pause)) =
                            tokio::time::timeout(Duration::from_secs(1), player_proxy.can_pause())
                                .await
                            && !can_pause
                        {
                            return None;
                        }

                        match tokio::time::timeout(Duration::from_secs(2), player_proxy.pause())
                            .await
                        {
                            Ok(Ok(())) => Some(bus_name),
                            _ => None,
                        }
                    }
                    .await;
                    opt
                }));
            }

            let mut paused = Vec::new();
            for task in tasks {
                if let Ok(Some(name)) = task.await {
                    paused.push(name);
                }
            }

            Ok(paused)
        })
    }

    fn has_playing_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<bool, ActionError>> {
        Box::pin(async move {
            let count = self.count_playing_players(ignore_players).await?;
            Ok(count > 0)
        })
    }

    fn count_playing_players<'a>(
        &'a self,
        ignore_players: &'a [String],
    ) -> BoxFuture<'a, Result<usize, ActionError>> {
        Box::pin(async move {
            let conn = match self.session_conn().await {
                Ok(c) => c,
                Err(err) => return Err(ActionError::Mpris(format!("session bus error: {err}"))),
            };

            let dbus_proxy = match DBus1Proxy::new(&conn).await {
                Ok(p) => p,
                Err(err) => return Err(ActionError::Mpris(format!("DBus1Proxy error: {err}"))),
            };

            let names = match dbus_proxy.list_names().await {
                Ok(n) => n,
                Err(err) => return Err(ActionError::Mpris(format!("list_names error: {err}"))),
            };

            let mut targets = Vec::new();
            for name in names {
                if name.starts_with("org.mpris.MediaPlayer2.")
                    && !is_player_ignored(&name, ignore_players)
                {
                    targets.push(name);
                }
            }

            if targets.is_empty() {
                return Ok(0);
            }

            let mut tasks = Vec::new();
            for bus_name in targets {
                let c = conn.clone();
                tasks.push(tokio::spawn(async move {
                    let opt: Option<bool> = async {
                        let builder = MediaPlayer2PlayerProxy::builder(&c)
                            .destination(bus_name.as_str())
                            .ok()?;
                        let player_proxy = builder.build().await.ok()?;

                        let status = tokio::time::timeout(
                            Duration::from_secs(2),
                            player_proxy.playback_status(),
                        )
                        .await
                        .ok()?
                        .ok()?;

                        Some(status == MPRIS_STATUS_PLAYING)
                    }
                    .await;
                    opt
                }));
            }

            let mut count = 0_usize;
            for task in tasks {
                if let Ok(Some(true)) = task.await {
                    count += 1;
                }
            }

            Ok(count)
        })
    }

    fn run_custom_command<'a>(
        &'a self,
        command: &'a [String],
        env: &'a [(&'a str, &'a str)],
        timeout: Duration,
    ) -> BoxFuture<'a, Result<CommandOutput, ActionError>> {
        Box::pin(async move {
            let Some(prog) = command.first() else {
                return Err(ActionError::CustomCommand(
                    "empty command vector".to_string(),
                ));
            };

            let mut cmd = tokio::process::Command::new(prog);
            cmd.args(&command[1..]);
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.kill_on_drop(true);

            let run_future = cmd.output();
            let output = match tokio::time::timeout(timeout, run_future).await {
                Ok(Ok(out)) => out,
                Ok(Err(err)) => return Err(ActionError::CustomCommand(err.to_string())),
                Err(_) => return Err(ActionError::CustomCommandTimeout(timeout)),
            };

            let stdout_truncated =
                String::from_utf8_lossy(&output.stdout[..output.stdout.len().min(4096)])
                    .to_string();
            let stderr_truncated =
                String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)])
                    .to_string();

            Ok(CommandOutput {
                exit_code: output.status.code(),
                success: output.status.success(),
                stdout_truncated,
                stderr_truncated,
            })
        })
    }

    fn lock_screen<'a>(&'a self) -> BoxFuture<'a, Result<(), ActionError>> {
        Box::pin(async move {
            let conn = match zbus::Connection::system().await {
                Ok(c) => c,
                Err(err) => {
                    return Err(ActionError::LockScreen(format!("system bus error: {err}")));
                }
            };

            // First attempt Session.Lock() on caller's active session, fallback to Manager.LockSessions()
            let session_proxy = LogindSessionProxy::builder(&conn)
                .destination("org.freedesktop.login1")
                .ok()
                .and_then(|b| b.path("/org/freedesktop/login1/session/auto").ok());

            let session_lock_ok = if let Some(sp) = session_proxy {
                if let Ok(proxy) = sp.build().await {
                    proxy.lock().await.is_ok()
                } else {
                    false
                }
            } else {
                false
            };

            if !session_lock_ok {
                let manager = match LogindManagerProxy::new(&conn).await {
                    Ok(m) => m,
                    Err(err) => {
                        return Err(ActionError::LockScreen(format!(
                            "LogindManagerProxy error: {err}"
                        )));
                    }
                };

                if let Err(err) = manager.lock_sessions().await {
                    return Err(ActionError::LockScreen(err.to_string()));
                }
            }

            Ok(())
        })
    }

    fn execute_power_action<'a>(
        &'a self,
        action: PowerAction,
    ) -> BoxFuture<'a, Result<PowerActionResult, ActionError>> {
        Box::pin(async move {
            let conn = match zbus::Connection::system().await {
                Ok(c) => c,
                Err(err) => {
                    return Err(ActionError::PowerAction(format!("system bus error: {err}")));
                }
            };

            let manager = match LogindManagerProxy::new(&conn).await {
                Ok(m) => m,
                Err(err) => {
                    return Err(ActionError::PowerAction(format!(
                        "LogindManagerProxy error: {err}"
                    )));
                }
            };

            let call_res = match action {
                PowerAction::Suspend => manager.suspend(false).await,
                PowerAction::Poweroff => manager.power_off(false).await,
                PowerAction::None => return Ok(PowerActionResult::Executed),
            };

            match call_res {
                Ok(()) => Ok(PowerActionResult::Executed),
                Err(err) => {
                    let err_str = err.to_string();
                    if err_str.contains("AccessDenied")
                        || err_str.contains("inhibited")
                        || err_str.contains("Inhibited")
                        || err_str.contains("Authorization")
                        || err_str.contains("not authorized")
                    {
                        Ok(PowerActionResult::RefusedInhibited(err_str))
                    } else {
                        Ok(PowerActionResult::Failed(err_str))
                    }
                }
            }
        })
    }

    fn watch_mpris_resume<'a>(&'a self, paused_players: &'a [String]) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            let conn = match self.session_conn().await {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "watch_mpris_resume failed to connect to session D-Bus"
                    );
                    return false;
                }
            };

            let (resumed_tx, mut resumed_rx) = mpsc::channel::<()>(1);
            let mut join_handles = Vec::new();

            for target in paused_players {
                let c = conn.clone();
                let tx = resumed_tx.clone();
                let target_name = target.clone();
                join_handles.push(tokio::spawn(async move {
                    let props_builder = match zbus::fdo::PropertiesProxy::builder(&c)
                        .destination(target_name.as_str())
                        .and_then(|b| b.path(MPRIS_OBJECT_PATH))
                    {
                        Ok(b) => b,
                        Err(err) => {
                            tracing::warn!(
                                player = %target_name,
                                error = %err,
                                "failed to configure MPRIS PropertiesProxy"
                            );
                            return;
                        }
                    };
                    let props_proxy = match props_builder.build().await {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!(
                                player = %target_name,
                                error = %err,
                                "failed to build MPRIS PropertiesProxy"
                            );
                            return;
                        }
                    };
                    // Subscribe to PropertiesChanged before reading initial PlaybackStatus so no
                    // async browser transition (e.g. Playing -> Paused -> Playing) is lost or
                    // replayed out of order from zbus's coalescing property cache.
                    let mut stream = match props_proxy.receive_properties_changed().await {
                        Ok(s) => s,
                        Err(err) => {
                            tracing::warn!(
                                player = %target_name,
                                error = %err,
                                "failed to subscribe to MPRIS PropertiesChanged"
                            );
                            return;
                        }
                    };

                    let player_builder = match MediaPlayer2PlayerProxy::builder(&c)
                        .destination(target_name.as_str())
                    {
                        Ok(b) => b.cache_properties(zbus::proxy::CacheProperties::No),
                        Err(err) => {
                            tracing::warn!(
                                player = %target_name,
                                error = %err,
                                "failed to configure MPRIS PlayerProxy"
                            );
                            return;
                        }
                    };
                    let player_proxy = match player_builder.build().await {
                        Ok(p) => p,
                        Err(err) => {
                            tracing::warn!(
                                player = %target_name,
                                error = %err,
                                "failed to build MPRIS PlayerProxy"
                            );
                            return;
                        }
                    };

                    let mut seen_paused_or_stopped = match player_proxy.playback_status().await {
                        Ok(status) => {
                            matches!(status.as_str(), MPRIS_STATUS_PAUSED | MPRIS_STATUS_STOPPED)
                        }
                        Err(err) => {
                            tracing::debug!(
                                player = %target_name,
                                error = %err,
                                "failed to read initial MPRIS PlaybackStatus; waiting for signal"
                            );
                            false
                        }
                    };

                    use zbus::export::ordered_stream::OrderedStreamExt;
                    while let Some(signal) = stream.next().await {
                        let args = match signal.args() {
                            Ok(args) => args,
                            Err(err) => {
                                tracing::debug!(
                                    player = %target_name,
                                    error = %err,
                                    "failed to decode MPRIS PropertiesChanged signal"
                                );
                                continue;
                            }
                        };
                        if args.interface_name.as_str() != MPRIS_PLAYER_INTERFACE {
                            continue;
                        }

                        let status_opt =
                            if let Some(val) = args.changed_properties.get("PlaybackStatus") {
                                match val {
                                    Value::Str(s) => Some(s.as_str().to_string()),
                                    other => {
                                        tracing::debug!(
                                            player = %target_name,
                                            value = ?other,
                                            "unexpected MPRIS PlaybackStatus property type"
                                        );
                                        None
                                    }
                                }
                            } else if args.invalidated_properties.contains(&"PlaybackStatus") {
                                match player_proxy.playback_status().await {
                                    Ok(status) => Some(status),
                                    Err(err) => {
                                        tracing::debug!(
                                            player = %target_name,
                                            error = %err,
                                            "failed to re-read invalidated MPRIS PlaybackStatus"
                                        );
                                        None
                                    }
                                }
                            } else {
                                None
                            };

                        if let Some(status) = status_opt {
                            match status.as_str() {
                                MPRIS_STATUS_PAUSED | MPRIS_STATUS_STOPPED => {
                                    seen_paused_or_stopped = true;
                                }
                                MPRIS_STATUS_PLAYING if seen_paused_or_stopped => {
                                    let _ = tx.send(()).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                }));
            }
            drop(resumed_tx);

            let resumed = tokio::select! {
                _ = tokio::time::sleep(self.mpris_watch_timeout) => false,
                res = resumed_rx.recv() => res.is_some(),
            };

            for h in join_handles {
                h.abort();
            }

            resumed
        })
    }
}
