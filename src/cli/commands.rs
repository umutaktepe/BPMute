use std::io::{self, Write};
use std::path::Path;
use std::process::Command as ProcessCommand;
use std::sync::Arc;

use chrono::Utc;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::calibration::{CalibrationState, validate_sleep_hr_bpm};
use crate::config::{CliOverrides, LoadedConfig, PatchField, Sensitivity, init_config_file};
use crate::daemon::{DaemonError, DaemonHandle, DaemonOptions, SystemClock};
use crate::exit_code::ExitCode;
use crate::ipc::protocol::ConfigSnapshotPayload;
use crate::ipc::{DaemonStatus, IpcClient, IpcClientError, OfflineStatus};
use crate::logging::{format_wearable_label, init_logging};
use crate::paths::{EnvSource, ResolvedPaths, split_shell_words};
use crate::replay::{ReplayFidelity, ReplayOptions, replay_file, replay_stored_session};
use crate::sleep_engine::{CalibrationPriors, WearableFloor};
use crate::storage::{
    AcceptShiftResult, LedgerStore, SessionFullDetail, SessionListPage, SetFeedbackRequest,
    StorageError, db_exists, db_path_in_data_dir, lock_path_in_data_dir,
};

use super::args::{CalibrationCommand, Cli, Command, ConfigCommand, SessionsCommand};
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
            session,
            preset,
            cold_start,
            json,
        } => run_replay(
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
        Command::Sessions { action } => run_sessions(action, &paths, stdout, stderr).await,
        Command::Calibration { action } => run_calibration(action, &paths, stdout, stderr).await,
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
        wearable_source: None,
        install_signals: true,
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
    file: Option<&'a Path>,
    session: Option<&'a str>,
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

    let grace_sec = loaded.config.actions.effective_grace_period_sec();
    let report = if let Some(session_id) = args.session {
        let db_path = db_path_in_data_dir(&args.paths.data_dir);
        replay_stored_session(
            &db_path,
            session_id,
            loaded.config.detection,
            args.preset,
            args.cold_start,
            grace_sec,
        )
    } else if let Some(file) = args.file {
        let options = ReplayOptions {
            preset: args.preset,
            cold_start: args.cold_start,
            grace_period_sec: grace_sec,
            priors: CalibrationPriors::cold_start(),
            wearable_floor: WearableFloor::default(),
        };
        replay_file(file, loaded.config.detection, options)
    } else {
        let _ = writeln!(stderr, "error: either <FILE> or --session <ID> is required");
        return ExitCode::UsageOrConfigError;
    };

    let report = match report {
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

    let fidelity_desc = match report.fidelity {
        ReplayFidelity::RawSamples => "raw_samples (full 4-stage filter + HMM pipeline)",
        ReplayFidelity::SessionEpochs => {
            "session_epochs (fallback: raw_samples unavailable; post-filter HMM + state machine)"
        }
    };
    let _ = writeln!(
        stdout,
        "Replay fidelity: {} (read-only) | format: {} | preset: {} | cold_start: {}",
        fidelity_desc,
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

enum OnlineOrOffline {
    Online(IpcClient),
    Offline,
}

async fn connect_online_or_offline<WErr: Write>(
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

fn report_storage_error<WErr: Write>(err: StorageError, stderr: &mut WErr) -> ExitCode {
    let _ = writeln!(stderr, "error: {err}");
    err.exit_code()
}

async fn ipc_request_typed<T: DeserializeOwned, WErr: Write>(
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

fn open_existing_store_rw<WErr: Write>(
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

fn write_json_line<WOut: Write, WErr: Write>(
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

async fn run_sessions<WOut: Write, WErr: Write>(
    action: SessionsCommand,
    paths: &ResolvedPaths,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    match action {
        SessionsCommand::List { limit, json } => {
            if limit == 0 {
                let _ = writeln!(stderr, "error: --limit must be >= 1");
                return ExitCode::UsageOrConfigError;
            }
            let page: SessionListPage = match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match ipc_request_typed(
                        &mut client,
                        "session.list",
                        json!({ "limit": limit, "offset": 0 }),
                        stderr,
                    )
                    .await
                    {
                        Ok(page) => page,
                        Err(code) => return code,
                    }
                }
                Ok(OnlineOrOffline::Offline) => match LedgerStore::open_read_only(&db_path) {
                    Ok(Some(store)) => match store.list_sessions(limit, 0) {
                        Ok(page) => page,
                        Err(err) => return report_storage_error(err, stderr),
                    },
                    Ok(None) => SessionListPage {
                        sessions: Vec::new(),
                        total_count: 0,
                        limit,
                        offset: 0,
                    },
                    Err(err) => return report_storage_error(err, stderr),
                },
                Err(code) => return code,
            };

            if json {
                return write_json_line(&page, stdout, stderr);
            }
            let _ = writeln!(
                stdout,
                "Monitoring Sessions ({} shown of {} total):",
                page.sessions.len(),
                page.total_count
            );
            for s in &page.sessions {
                let wearable_label = format_wearable_label(
                    s.wearable_alias.as_deref(),
                    &s.wearable_mac_masked,
                    false,
                );
                let end_str = s
                    .end_reason
                    .map(|r| r.as_str())
                    .unwrap_or(s.status.as_str());
                let fb_tag = s
                    .feedback
                    .as_ref()
                    .and_then(|f| f.verdict)
                    .map(|v| format!(" verdict={v}"))
                    .unwrap_or_default();
                let _ = writeln!(
                    stdout,
                    "  {} | {} | {}..{} ms | {} | epochs={} | wake={:?} | sleep={:?}{}",
                    s.id,
                    wearable_label,
                    s.start_ts_ms,
                    s.end_ts_ms,
                    end_str,
                    s.epochs_count,
                    s.warmup_mu_hr,
                    s.sleep_mu_hr,
                    fb_tag
                );
            }
            ExitCode::Success
        }
        SessionsCommand::Show { id, json } => {
            if id.trim().is_empty() {
                let _ = writeln!(stderr, "error: session ID must not be empty");
                return ExitCode::UsageOrConfigError;
            }
            let detail: SessionFullDetail = match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match ipc_request_typed(&mut client, "session.get", json!({ "id": id }), stderr)
                        .await
                    {
                        Ok(detail) => detail,
                        Err(code) => return code,
                    }
                }
                Ok(OnlineOrOffline::Offline) => match LedgerStore::open_read_only(&db_path) {
                    Ok(Some(store)) => match store.get_session(&id) {
                        Ok(detail) => detail,
                        Err(err) => return report_storage_error(err, stderr),
                    },
                    Ok(None) => {
                        let _ = writeln!(
                            stderr,
                            "error: session '{id}' not found (no ledger database)"
                        );
                        return ExitCode::RuntimeError;
                    }
                    Err(err) => return report_storage_error(err, stderr),
                },
                Err(code) => return code,
            };

            if json {
                return write_json_line(&detail, stdout, stderr);
            }
            let wearable_label = format_wearable_label(
                detail.wearable.alias.as_deref(),
                &detail.wearable.mac_masked,
                false,
            );
            let _ = writeln!(stdout, "Session {} ({})", detail.session.id, detail.status);
            let _ = writeln!(stdout, "  Wearable: {wearable_label}");
            let _ = writeln!(
                stdout,
                "  Window: {}..{} ms ({})",
                detail.session.start_ts_ms,
                detail.session.end_ts_ms,
                detail.session.end_reason.as_str()
            );
            let _ = writeln!(
                stdout,
                "  Metrics: warmup_mu={:?}, sleep_mu={:?}, delta_hr={:?}, post_trigger_sleep_sec={}, anomalous={}",
                detail.session.warmup_mu_hr,
                detail.session.sleep_mu_hr,
                detail.session.delta_hr_onset,
                detail.session.post_trigger_sleep_sec,
                detail.session.auto_anomalous
            );
            let _ = writeln!(
                stdout,
                "  Data: epochs={}, raw_samples={} (retained={})",
                detail.epochs.len(),
                detail.session.raw_samples_count,
                detail.has_raw_samples
            );
            if let Some(ref fb) = detail.feedback {
                let _ = writeln!(
                    stdout,
                    "  Feedback: verdict={:?}, manual_onset_ts_ms={:?}, manual_avg_sleep_hr={:?}",
                    fb.verdict, fb.manual_onset_ts_ms, fb.manual_avg_sleep_hr
                );
            }
            ExitCode::Success
        }
        SessionsCommand::Feedback {
            id,
            verdict,
            onset,
            avg_sleep_hr,
            clear,
        } => {
            if id.trim().is_empty() {
                let _ = writeln!(stderr, "error: session ID must not be empty");
                return ExitCode::UsageOrConfigError;
            }
            if !clear && verdict.is_none() && onset.is_none() && avg_sleep_hr.is_none() {
                let _ = writeln!(
                    stderr,
                    "error: specify at least one of --verdict, --onset, --avg-sleep-hr, or --clear"
                );
                return ExitCode::UsageOrConfigError;
            }
            if let Some(hr) = avg_sleep_hr
                && let Err(msg) = validate_sleep_hr_bpm("--avg-sleep-hr", hr)
            {
                let _ = writeln!(stderr, "error: {msg}");
                return ExitCode::UsageOrConfigError;
            }

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let mut map = Map::new();
                    map.insert("session_id".to_string(), Value::String(id.clone()));
                    if clear {
                        map.insert("clear".to_string(), Value::Bool(true));
                    } else {
                        if let Some(v) = verdict {
                            map.insert("verdict".to_string(), json!(v));
                        }
                        if let Some(t) = onset {
                            map.insert("onset_hhmm".to_string(), json!(t));
                        }
                        if let Some(hr) = avg_sleep_hr {
                            map.insert("avg_sleep_hr".to_string(), json!(hr));
                        }
                    }
                    match client
                        .request("session.set_feedback", Value::Object(map))
                        .await
                    {
                        Ok(val) => {
                            let sid = val["session"]["id"].as_str().unwrap_or(&id);
                            let _ = writeln!(
                                stdout,
                                "Updated feedback for session {sid} and recomputed calibration."
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_ipc_error(err, stderr),
                    }
                }
                Ok(OnlineOrOffline::Offline) => {
                    let mut store = match open_existing_store_rw(
                        &db_path,
                        &lock_path,
                        ExitCode::RuntimeError,
                        &format!("error: session '{id}' not found (no ledger database)"),
                        stderr,
                    ) {
                        Ok(s) => s,
                        Err(code) => return code,
                    };
                    let req = SetFeedbackRequest {
                        session_id_or_prefix: id,
                        verdict: verdict.map_or(PatchField::Unchanged, PatchField::Set),
                        onset_hhmm: onset.map_or(PatchField::Unchanged, PatchField::Set),
                        avg_sleep_hr: avg_sleep_hr.map_or(PatchField::Unchanged, PatchField::Set),
                        clear_all: clear,
                    };
                    let now_ms = Utc::now().timestamp_millis();
                    match store.set_feedback_and_recompute(&req, now_ms) {
                        Ok((detail, _)) => {
                            let _ = writeln!(
                                stdout,
                                "Updated feedback for session {} and recomputed calibration.",
                                detail.session.id
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }
    }
}

async fn run_calibration<WOut: Write, WErr: Write>(
    action: CalibrationCommand,
    paths: &ResolvedPaths,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    match action {
        CalibrationCommand::Status { json } => {
            let state: CalibrationState = match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match ipc_request_typed(&mut client, "calibration.status", json!({}), stderr)
                        .await
                    {
                        Ok(st) => st,
                        Err(code) => return code,
                    }
                }
                Ok(OnlineOrOffline::Offline) => match LedgerStore::open_read_only(&db_path) {
                    Ok(Some(store)) => match store.get_calibration_state() {
                        Ok(st) => st,
                        Err(err) => return report_storage_error(err, stderr),
                    },
                    Ok(None) => CalibrationState::default(),
                    Err(err) => return report_storage_error(err, stderr),
                },
                Err(code) => return code,
            };

            if json {
                return write_json_line(&state, stdout, stderr);
            }
            let _ = writeln!(
                stdout,
                "Calibration Maturity: {:?} (valid={}, quarantined={})",
                state.maturity, state.valid_sessions, state.quarantined_sessions
            );
            let _ = writeln!(
                stdout,
                "  mu_wake={:?}, sigma_wake={:?}, mu_sleep={:?}, delta_hr_onset={:?}, tau_offset={:.4}, dwell_adjust={}s",
                state.mu_wake,
                state.sigma_wake,
                state.mu_sleep,
                state.delta_hr_onset,
                state.tau_p_sleep_offset,
                state.dwell_adjust_sec
            );
            if let Some(seed) = state.day1_seed_sleep_hr {
                let _ = writeln!(stdout, "  day1_seed_sleep_hr={seed:.1} bpm");
            }
            if let Some(ref alert) = state.pending_shift_alert {
                let _ = writeln!(
                    stdout,
                    "  ALERT: persistent {} shift across {} consecutive sessions (run `bpmute calibration accept-shift` or `dismiss-shift`)",
                    alert.direction, alert.session_count
                );
            }
            for w in state.wearables.values() {
                let _ = writeln!(
                    stdout,
                    "  Wearable {}: hr_var_floor={:.3}, supports_rr={}, typical_sqi={:?}, sessions={}",
                    w.wearable_id, w.hr_var_floor, w.supports_rr, w.typical_sqi, w.sessions_count
                );
            }
            ExitCode::Success
        }
        CalibrationCommand::Seed { bpm, clear } => {
            if let Some(hr) = bpm
                && let Err(msg) = validate_sleep_hr_bpm("Day-1 sleeping HR seed", hr)
            {
                let _ = writeln!(stderr, "error: {msg}");
                return ExitCode::UsageOrConfigError;
            }
            let target_seed = if clear { None } else { bpm };

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client
                        .request("calibration.set_seed", json!({ "sleep_hr": target_seed }))
                        .await
                    {
                        Ok(_) => {
                            if let Some(hr) = target_seed {
                                let _ =
                                    writeln!(stdout, "Set Day-1 sleeping-HR seed to {hr:.1} bpm.");
                            } else {
                                let _ = writeln!(stdout, "Cleared Day-1 sleeping-HR seed.");
                            }
                            ExitCode::Success
                        }
                        Err(err) => report_ipc_error(err, stderr),
                    }
                }
                Ok(OnlineOrOffline::Offline) => {
                    match db_exists(&db_path) {
                        Ok(false) if target_seed.is_none() => {
                            let _ = writeln!(stdout, "Cleared Day-1 sleeping-HR seed.");
                            return ExitCode::Success;
                        }
                        Ok(_) => {}
                        Err(err) => return report_storage_error(err, stderr),
                    }
                    let mut store = match LedgerStore::open_read_write(&db_path, &lock_path) {
                        Ok(s) => s,
                        Err(err) => return report_storage_error(err, stderr),
                    };
                    let now_ms = Utc::now().timestamp_millis();
                    match store.set_day1_seed_and_recompute(target_seed, now_ms) {
                        Ok(_) => {
                            if let Some(hr) = target_seed {
                                let _ =
                                    writeln!(stdout, "Set Day-1 sleeping-HR seed to {hr:.1} bpm.");
                            } else {
                                let _ = writeln!(stdout, "Cleared Day-1 sleeping-HR seed.");
                            }
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }
        CalibrationCommand::AcceptShift => match connect_online_or_offline(paths, stderr).await {
            Ok(OnlineOrOffline::Online(mut client)) => {
                match client.request("calibration.accept_shift", json!({})).await {
                    Ok(val) => match serde_json::from_value::<AcceptShiftResult>(val) {
                        Ok(res) => {
                            let _ = writeln!(
                                stdout,
                                "Accepted persistent shift; created Calibration Boundary {} (effective_ts_ms={}).",
                                res.boundary.id, res.boundary.effective_ts_ms
                            );
                            ExitCode::Success
                        }
                        Err(_) => {
                            let _ = writeln!(stdout, "Accepted persistent shift.");
                            ExitCode::Success
                        }
                    },
                    Err(err) => report_ipc_error(err, stderr),
                }
            }
            Ok(OnlineOrOffline::Offline) => {
                let mut store = match open_existing_store_rw(
                    &db_path,
                    &lock_path,
                    ExitCode::PreconditionConflict,
                    "error: precondition conflict: no pending 5-session persistent-shift alert to accept",
                    stderr,
                ) {
                    Ok(s) => s,
                    Err(code) => return code,
                };
                let now_ms = Utc::now().timestamp_millis();
                match store.accept_shift_and_recompute(now_ms) {
                    Ok(res) => {
                        let _ = writeln!(
                            stdout,
                            "Accepted persistent shift; created Calibration Boundary {} (effective_ts_ms={}).",
                            res.boundary.id, res.boundary.effective_ts_ms
                        );
                        ExitCode::Success
                    }
                    Err(err) => report_storage_error(err, stderr),
                }
            }
            Err(code) => code,
        },
        CalibrationCommand::DismissShift => match connect_online_or_offline(paths, stderr).await {
            Ok(OnlineOrOffline::Online(mut client)) => {
                match client.request("calibration.dismiss_shift", json!({})).await {
                    Ok(_) => {
                        let _ = writeln!(stdout, "Dismissed persistent-shift prompt.");
                        ExitCode::Success
                    }
                    Err(err) => report_ipc_error(err, stderr),
                }
            }
            Ok(OnlineOrOffline::Offline) => {
                let mut store = match open_existing_store_rw(
                    &db_path,
                    &lock_path,
                    ExitCode::PreconditionConflict,
                    "error: precondition conflict: no pending 5-session persistent-shift alert to dismiss",
                    stderr,
                ) {
                    Ok(s) => s,
                    Err(code) => return code,
                };
                let now_ms = Utc::now().timestamp_millis();
                match store.dismiss_shift_and_recompute(now_ms) {
                    Ok(_) => {
                        let _ = writeln!(stdout, "Dismissed persistent-shift prompt.");
                        ExitCode::Success
                    }
                    Err(err) => report_storage_error(err, stderr),
                }
            }
            Err(code) => code,
        },
    }
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
