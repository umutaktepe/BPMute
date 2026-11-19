use std::io::{self, Write};
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::Arc;

use serde_json::json;

use crate::config::{CliOverrides, LoadedConfig, Sensitivity, init_config_file};
use crate::daemon::{DaemonError, DaemonHandle, DaemonOptions, SystemClock};
use crate::exit_code::ExitCode;
use crate::ipc::protocol::ConfigSnapshotPayload;
use crate::ipc::{DaemonStatus, IpcClient, IpcClientError, OfflineStatus};
use crate::logging::init_logging;
use crate::paths::{EnvSource, ResolvedPaths, split_shell_words};
use crate::replay::{ReplayOptions, replay_file};
use crate::sleep_engine::{CalibrationPriors, WearableFloor};

use super::args::{Cli, Command, ConfigCommand};
use super::duration::{PauseTarget, parse_pause_target};

/// Executes a parsed CLI command using injected environment and output streams.
pub async fn execute_cli<WOut: Write, WErr: Write>(
    cli: Cli,
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
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
        Command::Daemon { dry_run } => {
            let daemon_overrides = build_cli_overrides(&cli, dry_run.then_some(true));
            run_daemon(paths, env, daemon_overrides, stderr).await
        }
        Command::Status { json } => run_status(&paths, json, stdout, stderr).await,
        Command::Pause { target } => run_pause(&paths, target.as_deref(), stdout, stderr).await,
        Command::Resume => run_resume(&paths, stdout, stderr).await,
        Command::Replay {
            file,
            preset,
            cold_start,
            json,
        } => run_replay(
            ReplayCliArgs {
                file: &file,
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
        Command::Config { action } => {
            run_config(action, &paths, env, &cli_overrides, stdout, stderr).await
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

    let handle = match DaemonHandle::spawn(DaemonOptions {
        paths,
        loaded_config: loaded,
        env: env_arc,
        cli_overrides,
        clock: Arc::new(SystemClock),
        install_signals: true,
    })
    .await
    {
        Ok(handle) => handle,
        Err(err) => {
            tracing::error!(error = %err, "failed to start bpmute daemon");
            let _ = writeln!(stderr, "error: {err}");
            return match err {
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
            } else {
                let _ = serde_json::to_writer(&mut *stdout, &data);
                let _ = writeln!(stdout);
            }
            ExitCode::Success
        }
        Err(err) => report_ipc_error(err, stderr),
    }
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
        PauseTarget::Indefinite => json!({ "mode": "indefinite" }),
        PauseTarget::UntilMorning => json!({ "mode": "until_morning" }),
        PauseTarget::DurationSecs(secs) => {
            json!({ "mode": "duration", "duration_sec": secs })
        }
    };

    match client.request("monitor.pause", params).await {
        Ok(data) => {
            if let Some(until) = data.get("paused_until").and_then(|v| v.as_str()) {
                let _ = writeln!(stdout, "Monitoring paused until {until}.");
            } else {
                let _ = writeln!(stdout, "Monitoring paused indefinitely.");
            }
            ExitCode::Success
        }
        Err(err) => report_ipc_error(err, stderr),
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

struct ReplayCliArgs<'a> {
    file: &'a Path,
    preset: Option<Sensitivity>,
    cold_start: bool,
    as_json: bool,
    paths: &'a ResolvedPaths,
    env: &'a dyn EnvSource,
    cli_overrides: &'a CliOverrides,
}

fn run_replay<WOut: Write, WErr: Write>(
    args: ReplayCliArgs<'_>,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let loaded = match LoadedConfig::load(&args.paths.config_file, args.env, args.cli_overrides) {
        Ok(loaded) => loaded,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return err.exit_code();
        }
    };

    let options = ReplayOptions {
        preset: args.preset,
        cold_start: args.cold_start,
        grace_period_sec: loaded.config.actions.effective_grace_period_sec(),
        priors: CalibrationPriors::cold_start(),
        wearable_floor: WearableFloor::default(),
    };

    let report = match replay_file(args.file, loaded.config.detection, options) {
        Ok(report) => report,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return err.exit_code();
        }
    };

    if args.as_json {
        if let Err(err) = serde_json::to_writer(&mut *stdout, &report) {
            let _ = writeln!(stderr, "error: failed to serialize replay report: {err}");
            return ExitCode::RuntimeError;
        }
        let _ = writeln!(stdout);
        return ExitCode::Success;
    }

    let _ = writeln!(
        stdout,
        "Replay fidelity: {} (read-only) | format: {} | preset: {} | cold_start: {}",
        report.fidelity.as_str(),
        report.format.as_str(),
        report.preset,
        report.cold_start
    );
    let _ = writeln!(
        stdout,
        "Processed {} HR samples ({} events) -> {} epochs across {} session(s)",
        report.samples_processed,
        report.events_processed,
        report.epochs_emitted,
        report.sessions.len()
    );
    for s in &report.sessions {
        let end_tag = s
            .end_reason
            .map(|r| format!("{r:?}"))
            .unwrap_or_else(|| "open".to_string());
        let _ = writeln!(
            stdout,
            "  Session #{}: start={} ms, end={} ms ({}), epochs={}, confirmed={:?}, sleeping={:?}, anomalous={}, fps={}",
            s.session_index,
            s.start_ts_ms,
            s.end_ts_ms,
            end_tag,
            s.epochs_count,
            s.confirmed_ts_ms,
            s.sleeping_ts_ms,
            s.anomalous,
            s.fp_count
        );
    }

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
    if !paths.config_file.exists()
        && let Err(err) = init_config_file(&paths.config_file, false)
    {
        let _ = writeln!(stderr, "error: failed to initialize config file: {err}");
        return err.exit_code();
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

async fn connect_or_report<WErr: Write>(
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

fn report_ipc_error<WErr: Write>(err: IpcClientError, stderr: &mut WErr) -> ExitCode {
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
