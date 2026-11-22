use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::bluetooth::BluezWearableSource;
use crate::config::{CliOverrides, LoadedConfig, Sensitivity, init_config_file};
use crate::daemon::{DaemonError, DaemonHandle, DaemonOptions, SystemClock};
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{ConfigSnapshotPayload, PauseMode, PauseParams};
use crate::ipc::{DaemonStatus, IpcClient, IpcClientError, OfflineStatus, SnapshotEvent};
use crate::logging::init_logging;
use crate::paths::{EnvSource, ResolvedPaths, resolve_config_file, split_shell_words};
use crate::storage::{LedgerStore, StorageError, db_exists, db_path_in_data_dir};
use crate::tui::format_close_countdown;

pub mod completions;
pub mod data;
pub mod monitor;
pub mod service;
pub mod sessions;
pub mod system_exec;
pub mod update;
pub mod wearables;

use data::run_data;
use monitor::{run_cancel, run_snooze, run_test_trigger};
use sessions::{run_calibration, run_replay, run_sessions};
use wearables::{run_enroll, run_wearables};

use super::args::{Cli, Command, ConfigCommand};
use super::duration::{PauseTarget, parse_pause_target};

/// Executes a parsed CLI command using injected environment and output streams.
pub async fn execute_cli<WOut: Write, WErr: Write>(
    cli: Cli,
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    if cli.command.is_some() && (cli.no_mouse || cli.ascii) {
        let _ = writeln!(
            stderr,
            "error: --no-mouse and --ascii are only valid when launching the interactive TUI without a subcommand"
        );
        return ExitCode::UsageOrConfigError;
    }

    match cli.command {
        Some(Command::Completions { shell }) => {
            return completions::run_completions(shell, stdout);
        }
        Some(Command::Update) => return update::run_update(env, stdout, stderr),
        Some(Command::Service { action }) => {
            let config_file = match action {
                crate::cli::args::ServiceCommand::Uninstall { purge_config: true } => {
                    match resolve_config_file(cli.config.as_deref(), env) {
                        Ok(p) => Some(p),
                        Err(err) => {
                            let _ = writeln!(stderr, "error: {err}");
                            return ExitCode::UsageOrConfigError;
                        }
                    }
                }
                _ => None,
            };
            return service::run_service(action, config_file.as_deref(), env, stdout, stderr);
        }
        _ => {}
    }

    let paths = match ResolvedPaths::resolve(
        cli.config.as_deref(),
        cli.data_dir.as_deref(),
        cli.socket.as_deref(),
        env,
    ) {
        Ok(paths) => paths,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return ExitCode::UsageOrConfigError;
        }
    };

    let cli_overrides = build_cli_overrides(&cli, None);

    match cli.command {
        None => crate::tui::run_tui(paths, env, cli.no_mouse, cli.ascii, stdout, stderr).await,
        Some(Command::Daemon { dry_run }) => {
            let daemon_overrides = build_cli_overrides(&cli, dry_run.then_some(true));
            run_daemon(paths, env, daemon_overrides, stderr).await
        }
        Some(Command::Status { json }) => run_status(&paths, json, stdout, stderr).await,
        Some(Command::Pause { target }) => {
            run_pause(&paths, target.as_deref(), stdout, stderr).await
        }
        Some(Command::Resume) => run_resume(&paths, stdout, stderr).await,
        Some(Command::Cancel) => run_cancel(&paths.socket_path, stdout, stderr).await,
        Some(Command::Snooze { duration }) => {
            run_snooze(&paths.socket_path, duration.as_deref(), stdout, stderr).await
        }
        Some(Command::TestTrigger { grace_sec }) => {
            run_test_trigger(&paths.socket_path, grace_sec, stdout, stderr).await
        }
        Some(Command::Replay {
            file,
            session,
            preset,
            cold_start,
            json,
        }) => run_replay(
            ReplayCliArgs {
                file: file.as_deref(),
                session: session.as_deref(),
                preset,
                cold_start,
                as_json: json,
                paths: &paths,
                env,
                cli_overrides: &cli_overrides,
            },
            stdout,
            stderr,
        ),
        Some(Command::Sessions { action }) => {
            run_sessions(action, &paths, env, stdout, stderr).await
        }
        Some(Command::Calibration { action }) => {
            run_calibration(action, &paths, env, stdout, stderr).await
        }
        Some(Command::Data { action }) => run_data(action, &paths, env, stdout, stderr).await,
        Some(Command::Wearables { action }) => run_wearables(action, &paths, stdout, stderr).await,
        Some(Command::Enroll { address, alias }) => {
            run_enroll(address.as_deref(), alias.as_deref(), &paths, stdout, stderr).await
        }
        Some(Command::Config { action }) => {
            run_config(action, &paths, env, &cli_overrides, stdout, stderr).await
        }
        Some(Command::Service { .. })
        | Some(Command::Completions { .. })
        | Some(Command::Update) => {
            unreachable!("handled before path resolution")
        }
    }
}

fn build_cli_overrides(cli: &Cli, dry_run: Option<bool>) -> CliOverrides {
    CliOverrides {
        dry_run,
        log_level: cli.log_level.clone(),
        log_format: cli.log_format,
        log_biometrics: cli.log_biometrics.then_some(true),
    }
}

async fn run_daemon<WErr: Write>(
    paths: ResolvedPaths,
    env: &dyn EnvSource,
    cli_overrides: CliOverrides,
    stderr: &mut WErr,
) -> ExitCode {
    let loaded = match LoadedConfig::load(&paths.config_file, env, &cli_overrides) {
        Ok(loaded) => loaded,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return err.exit_code();
        }
    };

    if let Err(err) = init_logging(&loaded, env) {
        let _ = writeln!(stderr, "error: failed to initialize logging: {err}");
        return err.exit_code();
    }

    let env_arc = env.clone_arc();
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let clock = Arc::new(SystemClock);
    let wearable_source = Arc::new(BluezWearableSource::new(
        db_path,
        loaded.config.notifications.connection_lifecycle,
        loaded.config.bluetooth.adapter.clone(),
        Arc::clone(&clock) as Arc<dyn crate::daemon::DaemonClock>,
    ));

    let handle = match DaemonHandle::spawn(DaemonOptions {
        paths,
        loaded_config: loaded,
        env: env_arc.clone(),
        cli_overrides,
        clock,
        wearable_source: Some(wearable_source),
        install_signals: true,
        action_executor: Some(Arc::new(crate::actions::SystemActionExecutor)),
        notifier: Some(Arc::new(crate::actions::DesktopNotifier::new())),
        idle_probe: Some(Arc::new(crate::idle::SystemIdleProbe::new(env_arc))),
        enrollment_driver: Some(Arc::new(crate::daemon::BluezEnrollmentDriver)),
    })
    .await
    {
        Ok(handle) => handle,
        Err(err) => {
            tracing::error!(error = %err, "failed to start bpmute daemon");
            let _ = writeln!(stderr, "error: {err}");
            return match err {
                DaemonError::Storage(ref s) => s.exit_code(),
                DaemonError::Io(ref io_err) if io_err.kind() == io::ErrorKind::PermissionDenied => {
                    ExitCode::PermissionOrBackendRefusal
                }
                _ => ExitCode::RuntimeError,
            };
        }
    };

    handle.wait().await;
    ExitCode::Success
}

async fn run_status<WOut: Write, WErr: Write>(
    paths: &ResolvedPaths,
    as_json: bool,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let mut client = match connect_or_report(paths, stderr, !as_json).await {
        Ok(client) => client,
        Err(ExitCode::DaemonOffline) => {
            if as_json {
                let offline = OfflineStatus::default();
                let _ = serde_json::to_writer(&mut *stdout, &offline);
                let _ = writeln!(stdout);
            }
            return ExitCode::DaemonOffline;
        }
        Err(code) => return code,
    };

    match client.request("daemon.status", json!({})).await {
        Ok(data) => {
            if as_json {
                let _ = serde_json::to_writer(&mut *stdout, &data);
                let _ = writeln!(stdout);
            } else if let Ok(status) = serde_json::from_value::<DaemonStatus>(data.clone()) {
                let _ = writeln!(
                    stdout,
                    "bpmute daemon running (pid {}, state: {}, armed: {}, paused: {}, dry_run: {})",
                    status.pid,
                    status.monitoring_state,
                    status.armed,
                    status.paused,
                    status.dry_run
                );
                if let Some(until) = status.paused_until {
                    let _ = writeln!(stdout, "paused_until: {until}");
                }
                if let Some(deadline_ms) = status.session_close_deadline_ms {
                    let now_ms = fetch_daemon_now_ms(&mut client).await;
                    let remaining_ms = deadline_ms.saturating_sub(now_ms).max(0) as u64;
                    let rem_sec = remaining_ms.saturating_sub(1).div_ceil(1000);
                    let formatted = format_close_countdown(rem_sec);
                    let _ = writeln!(stdout, "session_closing_in: {formatted}");
                }
            } else {
                let _ = serde_json::to_writer(&mut *stdout, &data);
                let _ = writeln!(stdout);
            }
            ExitCode::Success
        }
        Err(err) => report_ipc_error(err, stderr),
    }
}

async fn fetch_daemon_now_ms(client: &mut IpcClient) -> i64 {
    if client.request("daemon.subscribe", json!({})).await.is_ok() {
        for _ in 0..8 {
            let Ok(ev) = client.next_event().await else {
                break;
            };
            if ev.event == "Snapshot"
                && let Ok(snap) = serde_json::from_value::<SnapshotEvent>(ev.data)
                && let Some(server_now_ms) = snap.server_now_ms
            {
                return server_now_ms;
            }
        }
    }
    chrono::Utc::now().timestamp_millis()
}

async fn run_pause<WOut: Write, WErr: Write>(
    paths: &ResolvedPaths,
    target: Option<&str>,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let parsed = match parse_pause_target(target) {
        Ok(parsed) => parsed,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return ExitCode::UsageOrConfigError;
        }
    };

    let mut client = match connect_or_report(paths, stderr, true).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    let params = match parsed {
        PauseTarget::Indefinite => PauseParams {
            mode: Some(PauseMode::Indefinite),
            duration_sec: None,
        },
        PauseTarget::UntilMorning => PauseParams {
            mode: Some(PauseMode::UntilMorning),
            duration_sec: None,
        },
        PauseTarget::DurationSecs(secs) => PauseParams {
            mode: Some(PauseMode::Duration),
            duration_sec: Some(secs),
        },
    };

    match ipc_request_params::<_, DaemonStatus, _>(&mut client, "monitor.pause", &params, stderr)
        .await
    {
        Ok(status) => {
            if let Some(until) = status.paused_until {
                let _ = writeln!(stdout, "Monitoring paused until {until}.");
            } else {
                let _ = writeln!(stdout, "Monitoring paused indefinitely.");
            }
            ExitCode::Success
        }
        Err(code) => code,
    }
}

async fn run_resume<WOut: Write, WErr: Write>(
    paths: &ResolvedPaths,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let mut client = match connect_or_report(paths, stderr, true).await {
        Ok(client) => client,
        Err(code) => return code,
    };

    match client.request("monitor.resume", json!({})).await {
        Ok(_) => {
            let _ = writeln!(stdout, "Monitoring resumed.");
            ExitCode::Success
        }
        Err(err) => report_ipc_error(err, stderr),
    }
}

pub(crate) struct ReplayCliArgs<'a> {
    file: Option<&'a Path>,
    session: Option<&'a str>,
    preset: Option<Sensitivity>,
    cold_start: bool,
    as_json: bool,
    paths: &'a ResolvedPaths,
    env: &'a dyn EnvSource,
    cli_overrides: &'a CliOverrides,
}

pub(crate) enum OnlineOrOffline {
    Online(IpcClient),
    Offline,
}

pub(crate) async fn connect_online_or_offline<WErr: Write>(
    paths: &ResolvedPaths,
    stderr: &mut WErr,
) -> Result<OnlineOrOffline, ExitCode> {
    match IpcClient::connect(&paths.socket_path).await {
        Ok(client) => Ok(OnlineOrOffline::Online(client)),
        Err(IpcClientError::DaemonOffline { .. }) => Ok(OnlineOrOffline::Offline),
        Err(IpcClientError::PermissionDenied { .. }) => {
            let _ = writeln!(
                stderr,
                "error: permission denied accessing daemon socket {}",
                paths.socket_path.display()
            );
            Err(ExitCode::PermissionOrBackendRefusal)
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            Err(ExitCode::RuntimeError)
        }
    }
}

pub(crate) fn report_storage_error<WErr: Write>(err: StorageError, stderr: &mut WErr) -> ExitCode {
    let _ = writeln!(stderr, "error: {err}");
    err.exit_code()
}

pub(crate) async fn ipc_request_typed<T: DeserializeOwned, WErr: Write>(
    client: &mut IpcClient,
    op: &str,
    params: Value,
    stderr: &mut WErr,
) -> Result<T, ExitCode> {
    match client.request(op, params).await {
        Ok(val) => serde_json::from_value(val).map_err(|err| {
            let _ = writeln!(stderr, "error: invalid IPC response: {err}");
            ExitCode::RuntimeError
        }),
        Err(err) => Err(report_ipc_error(err, stderr)),
    }
}

pub(crate) async fn ipc_request_params<P: Serialize, T: DeserializeOwned, WErr: Write>(
    client: &mut IpcClient,
    op: &str,
    params: &P,
    stderr: &mut WErr,
) -> Result<T, ExitCode> {
    let val = serde_json::to_value(params).unwrap_or_else(|_| json!({}));
    ipc_request_typed(client, op, val, stderr).await
}

pub(crate) fn open_existing_store_rw<WErr: Write>(
    db_path: &Path,
    lock_path: &Path,
    missing_exit: ExitCode,
    missing_msg: &str,
    stderr: &mut WErr,
) -> Result<LedgerStore, ExitCode> {
    match db_exists(db_path) {
        Ok(true) => {}
        Ok(false) => {
            let _ = writeln!(stderr, "{missing_msg}");
            return Err(missing_exit);
        }
        Err(err) => return Err(report_storage_error(err, stderr)),
    }
    LedgerStore::open_read_write(db_path, lock_path)
        .map_err(|err| report_storage_error(err, stderr))
}

pub(crate) fn write_json_line<WOut: Write, WErr: Write>(
    value: &impl serde::Serialize,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    if let Err(err) = serde_json::to_writer(&mut *stdout, value) {
        let _ = writeln!(stderr, "error: failed to serialize JSON output: {err}");
        return ExitCode::RuntimeError;
    }
    let _ = writeln!(stdout);
    ExitCode::Success
}

async fn run_config<WOut: Write, WErr: Write>(
    action: ConfigCommand,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
    cli_overrides: &CliOverrides,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    match action {
        ConfigCommand::Init { force } => match init_config_file(&paths.config_file, force) {
            Ok(()) => {
                let _ = writeln!(
                    stdout,
                    "Initialized configuration at {}",
                    paths.config_file.display()
                );
                ExitCode::Success
            }
            Err(err) => {
                let _ = writeln!(stderr, "error: {err}");
                err.exit_code()
            }
        },
        ConfigCommand::Validate => {
            match LoadedConfig::load(&paths.config_file, env, cli_overrides) {
                Ok(loaded) => {
                    if loaded.file_present {
                        let _ = writeln!(
                            stdout,
                            "Configuration valid ({}).",
                            paths.config_file.display()
                        );
                    } else {
                        let _ = writeln!(
                            stdout,
                            "Configuration valid (using built-in defaults; {} not present).",
                            paths.config_file.display()
                        );
                    }
                    ExitCode::Success
                }
                Err(err) => {
                    let _ = writeln!(stderr, "error: {err}");
                    err.exit_code()
                }
            }
        }
        ConfigCommand::Path => {
            let _ = writeln!(stdout, "{}", paths.config_file.display());
            ExitCode::Success
        }
        ConfigCommand::Show { json } => {
            match LoadedConfig::load(&paths.config_file, env, cli_overrides) {
                Ok(loaded) => {
                    if json {
                        let snapshot = ConfigSnapshotPayload {
                            config: loaded.config,
                            file_present: loaded.file_present,
                            config_path: paths.config_file.display().to_string(),
                            log_filter: loaded.log_filter,
                            log_format: loaded.log_format,
                            log_biometrics: loaded.log_biometrics,
                        };
                        if let Err(err) = serde_json::to_writer_pretty(&mut *stdout, &snapshot) {
                            let _ = writeln!(stderr, "error: failed to serialize config: {err}");
                            return ExitCode::RuntimeError;
                        }
                        let _ = writeln!(stdout);
                    } else {
                        match toml::to_string_pretty(&loaded.config) {
                            Ok(rendered) => {
                                let _ = write!(stdout, "{rendered}");
                            }
                            Err(err) => {
                                let _ = writeln!(stderr, "error: failed to render TOML: {err}");
                                return ExitCode::RuntimeError;
                            }
                        }
                    }
                    ExitCode::Success
                }
                Err(err) => {
                    let _ = writeln!(stderr, "error: {err}");
                    err.exit_code()
                }
            }
        }
        ConfigCommand::Edit => run_config_edit(paths, env, cli_overrides, stdout, stderr),
        ConfigCommand::Reload => {
            let mut client = match connect_or_report(paths, stderr, true).await {
                Ok(client) => client,
                Err(code) => return code,
            };
            match client.request("config.reload", json!({})).await {
                Ok(_) => {
                    let _ = writeln!(stdout, "Configuration reloaded.");
                    ExitCode::Success
                }
                Err(err) => report_ipc_error(err, stderr),
            }
        }
    }
}

fn run_config_edit<WOut: Write, WErr: Write>(
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
    cli_overrides: &CliOverrides,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    match paths.config_file.try_exists() {
        Ok(false) => {
            if let Err(err) = init_config_file(&paths.config_file, false) {
                let _ = writeln!(stderr, "error: failed to initialize config file: {err}");
                return err.exit_code();
            }
        }
        Ok(true) => {}
        Err(err) => {
            let _ = writeln!(stderr, "error: failed to inspect config file: {err}");
            return if err.kind() == io::ErrorKind::PermissionDenied {
                ExitCode::PermissionOrBackendRefusal
            } else {
                ExitCode::RuntimeError
            };
        }
    }

    let editor_raw = env
        .get("VISUAL")
        .filter(|v| !v.trim().is_empty())
        .or_else(|| env.get("EDITOR").filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());

    let Some(parts) = split_shell_words(&editor_raw) else {
        let _ = writeln!(
            stderr,
            "error: invalid quoted editor command '{editor_raw}'"
        );
        return ExitCode::UsageOrConfigError;
    };
    let mut parts_iter = parts.into_iter();
    let Some(program) = parts_iter.next() else {
        let _ = writeln!(stderr, "error: empty editor command '{editor_raw}'");
        return ExitCode::UsageOrConfigError;
    };
    let args: Vec<String> = parts_iter.collect();

    let status = match ProcessCommand::new(&program)
        .args(&args)
        .arg(&paths.config_file)
        .status()
    {
        Ok(status) => status,
        Err(err) => {
            let _ = writeln!(stderr, "error: failed to launch editor '{program}': {err}");
            return ExitCode::RuntimeError;
        }
    };

    if !status.success() {
        let _ = writeln!(stderr, "error: editor exited with status {status}");
        return ExitCode::RuntimeError;
    }

    match LoadedConfig::load(&paths.config_file, env, cli_overrides) {
        Ok(_) => {
            let _ = writeln!(
                stdout,
                "Configuration saved and validated ({}).",
                paths.config_file.display()
            );
            ExitCode::Success
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: edited configuration is invalid: {err}");
            err.exit_code()
        }
    }
}

pub(crate) async fn connect_or_report<WErr: Write>(
    paths: &ResolvedPaths,
    stderr: &mut WErr,
    print_offline_msg: bool,
) -> Result<IpcClient, ExitCode> {
    IpcClient::connect(&paths.socket_path)
        .await
        .map_err(|err| match err {
            IpcClientError::DaemonOffline { .. } => {
                if print_offline_msg {
                    let _ = writeln!(stderr, "error: {err}");
                }
                ExitCode::DaemonOffline
            }
            IpcClientError::PermissionDenied { .. } => {
                let _ = writeln!(stderr, "error: {err}");
                ExitCode::PermissionOrBackendRefusal
            }
            other => {
                let _ = writeln!(stderr, "error: {other}");
                ExitCode::RuntimeError
            }
        })
}

pub(crate) fn report_ipc_error<WErr: Write>(err: IpcClientError, stderr: &mut WErr) -> ExitCode {
    let _ = writeln!(stderr, "error: {err}");
    match err {
        IpcClientError::DaemonOffline { .. } => ExitCode::DaemonOffline,
        IpcClientError::PermissionDenied { .. } => ExitCode::PermissionOrBackendRefusal,
        IpcClientError::Server { code, .. } => ExitCode::from_ipc_error_code(&code),
        IpcClientError::Frame(_)
        | IpcClientError::InvalidResponse(_)
        | IpcClientError::ConnectionClosed => ExitCode::RuntimeError,
    }
}

fn is_interactive(env: &dyn EnvSource) -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && env.get("BPMUTE_NON_INTERACTIVE").is_none()
}

pub(crate) fn confirm_action<WOut: Write, WErr: Write>(
    prompt: &str,
    skip_confirmation: bool,
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> Result<bool, ExitCode> {
    if skip_confirmation {
        return Ok(true);
    }
    if !is_interactive(env) {
        let _ = writeln!(
            stderr,
            "error: interactive confirmation required (or use --yes)"
        );
        return Err(ExitCode::UsageOrConfigError);
    }
    let _ = write!(stdout, "{prompt} [y/N]: ");
    let _ = stdout.flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        let _ = writeln!(stderr, "error: failed to read confirmation input");
        return Err(ExitCode::RuntimeError);
    }
    let trimmed = input.trim().to_lowercase();
    if trimmed == "y" || trimmed == "yes" {
        Ok(true)
    } else {
        let _ = writeln!(stdout, "Aborted.");
        Ok(false)
    }
}

pub(crate) fn confirm_wipe<WOut: Write, WErr: Write>(
    confirm_arg: Option<&str>,
    all: bool,
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> Result<bool, ExitCode> {
    if let Some(c) = confirm_arg {
        if c == "WIPE" {
            return Ok(true);
        } else {
            let _ = writeln!(stderr, "error: confirmation mismatch: expected 'WIPE'");
            return Err(ExitCode::UsageOrConfigError);
        }
    }
    if !is_interactive(env) {
        let _ = writeln!(
            stderr,
            "error: --confirm WIPE is required when invoked non-interactively"
        );
        return Err(ExitCode::UsageOrConfigError);
    }
    let target = if all {
        "ALL biometric and wearable data (including enrolled wearables)"
    } else {
        "biometric session history and calibration"
    };
    let _ = write!(
        stdout,
        "WARNING: This will permanently wipe {target}.\nType 'WIPE' to confirm: "
    );
    let _ = stdout.flush();
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        let _ = writeln!(stderr, "error: failed to read confirmation input");
        return Err(ExitCode::RuntimeError);
    }
    if input.trim() == "WIPE" {
        Ok(true)
    } else {
        let _ = writeln!(stdout, "Aborted: confirmation mismatch.");
        Ok(false)
    }
}

/// Normalizes a CLI destination or source path to an absolute path.
pub(crate) fn resolve_cli_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}
