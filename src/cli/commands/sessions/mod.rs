//! Offline and online session replay, session inspection, and calibration CLI commands.

mod calibration;
mod replay;

use std::io::Write;

use chrono::Utc;

use crate::calibration::validate_sleep_hr_bpm;
use crate::cli::args::SessionsCommand;
use crate::config::PatchField;
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{
    SessionDeleteParams, SessionDeleteResponse, SessionEndResponse, SessionExportTraceParams,
    SessionExportTraceResponse, SessionListParams, SetFeedbackParams,
};
use crate::logging::format_wearable_label;
use crate::paths::{EnvSource, ResolvedPaths};
use crate::replay::TraceFormat;
use crate::storage::{
    LedgerStore, SessionFullDetail, SessionListPage, SetFeedbackRequest, db_path_in_data_dir,
    lock_path_in_data_dir,
};

use super::{
    OnlineOrOffline, confirm_action, connect_online_or_offline, ipc_request_params,
    ipc_request_typed, open_existing_store_rw, report_ipc_error, report_storage_error,
    resolve_cli_path, write_json_line,
};

pub(crate) use calibration::run_calibration;
pub(crate) use replay::run_replay;

fn write_session_metrics<W: Write>(
    stdout: &mut W,
    session: &crate::calibration::SessionLedgerRecord,
) {
    let _ = writeln!(
        stdout,
        "  Metrics: warmup_mu={:?}, sleep_mu={:?}, delta_hr={:?}, post_trigger_sleep_sec={}, anomalous={}",
        session.warmup_mu_hr,
        session.sleep_mu_hr,
        session.delta_hr_onset,
        session.post_trigger_sleep_sec,
        session.auto_anomalous
    );
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
                    match client.get_session_full(&id).await {
                        Ok(detail) => detail,
                        Err(err) => return report_ipc_error(err, stderr),
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
            write_session_metrics(stdout, &detail.session);
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
            clear_verdict,
            onset,
            clear_onset,
            avg_sleep_hr,
            clear_avg_sleep_hr,
            clear,
        } => {
            if id.trim().is_empty() {
                let _ = writeln!(stderr, "error: session ID must not be empty");
                return ExitCode::UsageOrConfigError;
            }
            if !clear
                && !clear_verdict
                && !clear_onset
                && !clear_avg_sleep_hr
                && verdict.is_none()
                && onset.is_none()
                && avg_sleep_hr.is_none()
            {
                let _ = writeln!(
                    stderr,
                    "error: specify at least one of --verdict, --onset, --avg-sleep-hr, --clear-verdict, --clear-onset, --clear-avg-sleep-hr, or --clear"
                );
                return ExitCode::UsageOrConfigError;
            }
            if let Some(hr) = avg_sleep_hr
                && let Err(msg) = validate_sleep_hr_bpm("--avg-sleep-hr", hr)
            {
                let _ = writeln!(stderr, "error: {msg}");
                return ExitCode::UsageOrConfigError;
            }

            let verdict_patch = resolve_feedback_patch(verdict, clear_verdict, clear);
            let onset_patch = resolve_feedback_patch(onset, clear_onset, clear);
            let avg_sleep_hr_patch =
                resolve_feedback_patch(avg_sleep_hr, clear_avg_sleep_hr, clear);

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let params = SetFeedbackParams {
                        session_id: id.clone(),
                        verdict: verdict_patch,
                        onset_hhmm: onset_patch,
                        avg_sleep_hr: avg_sleep_hr_patch,
                        clear,
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
                        verdict: verdict_patch,
                        onset_hhmm: onset_patch,
                        avg_sleep_hr: avg_sleep_hr_patch,
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
        SessionsCommand::End { json } => {
            let mut client = match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(client)) => client,
                Ok(OnlineOrOffline::Offline) => {
                    let _ = writeln!(
                        stderr,
                        "error: daemon is offline; start daemon or ensure socket is reachable"
                    );
                    return ExitCode::DaemonOffline;
                }
                Err(code) => return code,
            };

            let response: SessionEndResponse =
                match ipc_request_typed(&mut client, "session.end", serde_json::json!({}), stderr)
                    .await
                {
                    Ok(resp) => resp,
                    Err(code) => return code,
                };

            if json {
                return write_json_line(&response, stdout, stderr);
            }

            match response {
                SessionEndResponse::Closed(detail) => {
                    let wearable_label = format_wearable_label(
                        detail.wearable.alias.as_deref(),
                        &detail.wearable.mac_masked,
                        false,
                    );
                    let _ = writeln!(
                        stdout,
                        "Ended Monitoring Session {} (reason: {})",
                        detail.session.id,
                        detail.session.end_reason.as_str()
                    );
                    let _ = writeln!(stdout, "  Wearable: {wearable_label}");
                    let _ = writeln!(
                        stdout,
                        "  Window: {}..{} ms",
                        detail.session.start_ts_ms, detail.session.end_ts_ms
                    );
                    write_session_metrics(stdout, &detail.session);
                }
                SessionEndResponse::Discarded(discarded) => {
                    let _ = writeln!(
                        stdout,
                        "Discarded incomplete-Warmup Monitoring Session {} (reason: {})",
                        discarded.session_id,
                        discarded.end_reason.as_str()
                    );
                }
            }
            ExitCode::Success
        }
    }
}

fn resolve_feedback_patch<T>(set: Option<T>, clear_field: bool, clear_all: bool) -> PatchField<T> {
    if clear_all {
        PatchField::Unchanged
    } else if clear_field {
        PatchField::Clear
    } else {
        set.map_or(PatchField::Unchanged, PatchField::Set)
    }
}
