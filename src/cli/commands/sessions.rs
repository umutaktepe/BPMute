//! Offline and online session replay, session inspection, and calibration CLI commands.

use std::io::Write;

use chrono::Utc;
use serde_json::{Value, json};

use crate::calibration::{CalibrationState, validate_sleep_hr_bpm};
use crate::cli::args::{CalibrationCommand, SessionsCommand};
use crate::config::{LoadedConfig, PatchField};
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{
    CalibrationSetBoundaryParams, SessionDeleteParams, SessionDeleteResponse,
    SessionExportTraceParams, SessionExportTraceResponse, SessionGetParams, SessionListParams,
    SetFeedbackParams, SetSeedParams,
};
use crate::logging::format_wearable_label;
use crate::paths::{EnvSource, ResolvedPaths};
use crate::replay::{
    ReplayFidelity, ReplayOptions, TraceFormat, replay_file, replay_stored_session,
};
use crate::sleep_engine::{CalibrationPriors, WearableFloor};
use crate::storage::{
    AcceptShiftResult, LedgerStore, SessionFullDetail, SessionListPage, SetFeedbackRequest,
    db_exists, db_path_in_data_dir, lock_path_in_data_dir,
};

use super::{
    OnlineOrOffline, ReplayCliArgs, confirm_action, connect_online_or_offline, ipc_request_params,
    ipc_request_typed, open_existing_store_rw, report_ipc_error, report_storage_error,
    resolve_cli_path, write_json_line,
};

pub(crate) fn run_replay<WOut: Write, WErr: Write>(
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

pub(crate) async fn run_sessions<WOut: Write, WErr: Write>(
    action: SessionsCommand,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
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
                    let params = SessionListParams {
                        limit: Some(limit),
                        offset: Some(0),
                    };
                    match ipc_request_params(&mut client, "session.list", &params, stderr).await {
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
                    let params = SessionGetParams { id: id.clone() };
                    match ipc_request_params(&mut client, "session.get", &params, stderr).await {
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
                    let params = if clear {
                        SetFeedbackParams {
                            session_id: id.clone(),
                            verdict: PatchField::Unchanged,
                            onset_hhmm: PatchField::Unchanged,
                            avg_sleep_hr: PatchField::Unchanged,
                            clear: true,
                        }
                    } else {
                        SetFeedbackParams {
                            session_id: id.clone(),
                            verdict: verdict.map_or(PatchField::Unchanged, PatchField::Set),
                            onset_hhmm: onset.map_or(PatchField::Unchanged, PatchField::Set),
                            avg_sleep_hr: avg_sleep_hr
                                .map_or(PatchField::Unchanged, PatchField::Set),
                            clear: false,
                        }
                    };
                    match ipc_request_params::<_, SessionFullDetail, _>(
                        &mut client,
                        "session.set_feedback",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(detail) => {
                            let _ = writeln!(
                                stdout,
                                "Updated feedback for session {} and recomputed calibration.",
                                detail.session.id
                            );
                            ExitCode::Success
                        }
                        Err(code) => code,
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
        SessionsCommand::Delete { id, yes } => {
            match confirm_action(&format!("Delete session '{id}'?"), yes, env, stdout, stderr) {
                Ok(true) => {}
                Ok(false) => return ExitCode::Success,
                Err(code) => return code,
            }

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let params = SessionDeleteParams { id: id.clone() };
                    match ipc_request_params::<_, SessionDeleteResponse, _>(
                        &mut client,
                        "session.delete",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(resp) => {
                            let _ = writeln!(
                                stdout,
                                "Deleted session {} and recomputed calibration.",
                                resp.deleted_session_id
                            );
                            ExitCode::Success
                        }
                        Err(code) => code,
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
                    let now_ms = Utc::now().timestamp_millis();
                    match store.delete_session_and_recompute(&id, now_ms) {
                        Ok((deleted_id, _)) => {
                            let _ = writeln!(
                                stdout,
                                "Deleted session {deleted_id} and recomputed calibration."
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }
        SessionsCommand::Export {
            id,
            path,
            format,
            no_anonymize,
        } => {
            let abs_path = resolve_cli_path(&path);
            let trace_format = match format {
                crate::cli::args::TraceFormatArg::Csv => TraceFormat::Csv,
                crate::cli::args::TraceFormatArg::Jsonl => TraceFormat::Jsonl,
            };

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let params = SessionExportTraceParams {
                        id: id.clone(),
                        path: abs_path.to_string_lossy().into_owned(),
                        format: Some(trace_format),
                        no_anonymize,
                    };
                    match ipc_request_params::<_, SessionExportTraceResponse, _>(
                        &mut client,
                        "session.export_trace",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(resp) => {
                            let _ = writeln!(
                                stdout,
                                "Exported {} samples from session {id} to {}",
                                resp.events_count, resp.path
                            );
                            ExitCode::Success
                        }
                        Err(code) => code,
                    }
                }
                Ok(OnlineOrOffline::Offline) => {
                    let store = match open_existing_store_rw(
                        &db_path,
                        &lock_path,
                        ExitCode::RuntimeError,
                        &format!("error: session '{id}' not found (no ledger database)"),
                        stderr,
                    ) {
                        Ok(s) => s,
                        Err(code) => return code,
                    };
                    match store.export_trace(&id, &abs_path, trace_format, !no_anonymize) {
                        Ok(count) => {
                            let _ = writeln!(
                                stdout,
                                "Exported {count} samples from session {id} to {}",
                                abs_path.display()
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

pub(crate) async fn run_calibration<WOut: Write, WErr: Write>(
    action: CalibrationCommand,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
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
                    let params = SetSeedParams {
                        sleep_hr: target_seed,
                    };
                    match ipc_request_params::<_, Value, _>(
                        &mut client,
                        "calibration.set_seed",
                        &params,
                        stderr,
                    )
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
                        Err(code) => code,
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
        CalibrationCommand::Rebaseline {
            seed,
            reset_floors,
            yes,
        } => {
            match confirm_action(
                "Set calibration boundary and rebaseline personal physiology to Cold Start?",
                yes,
                env,
                stdout,
                stderr,
            ) {
                Ok(true) => {}
                Ok(false) => return ExitCode::Success,
                Err(code) => return code,
            }

            let now_ms = Utc::now().timestamp_millis();
            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let params = CalibrationSetBoundaryParams {
                        effective_ts_ms: None,
                        seed_sleep_hr: seed,
                        reset_floors,
                    };
                    match ipc_request_params::<_, Value, _>(
                        &mut client,
                        "calibration.set_boundary",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(_) => {
                            let _ =
                                writeln!(stdout, "Rebaselined calibration to Cold Start boundary.");
                            ExitCode::Success
                        }
                        Err(code) => code,
                    }
                }
                Ok(OnlineOrOffline::Offline) => {
                    let mut store = match open_existing_store_rw(
                        &db_path,
                        &lock_path,
                        ExitCode::RuntimeError,
                        "error: ledger database not found",
                        stderr,
                    ) {
                        Ok(s) => s,
                        Err(code) => return code,
                    };
                    match store.set_calibration_boundary(None, seed, reset_floors, now_ms) {
                        Ok(_) => {
                            let _ =
                                writeln!(stdout, "Rebaselined calibration to Cold Start boundary.");
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }
        CalibrationCommand::UndoBoundary { yes } => {
            match confirm_action(
                "Undo the most recent calibration boundary?",
                yes,
                env,
                stdout,
                stderr,
            ) {
                Ok(true) => {}
                Ok(false) => return ExitCode::Success,
                Err(code) => return code,
            }

            let now_ms = Utc::now().timestamp_millis();
            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client.request("calibration.undo_boundary", json!({})).await {
                        Ok(_) => {
                            let _ = writeln!(
                                stdout,
                                "Undid calibration boundary; restored continuous calibration bit-for-bit."
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
                        "error: ledger database not found",
                        stderr,
                    ) {
                        Ok(s) => s,
                        Err(code) => return code,
                    };
                    match store.undo_calibration_boundary(now_ms) {
                        Ok(_) => {
                            let _ = writeln!(
                                stdout,
                                "Undid calibration boundary; restored continuous calibration bit-for-bit."
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
