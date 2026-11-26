//! Online and offline calibration management CLI commands.

use std::io::Write;

use chrono::Utc;
use serde_json::{Value, json};

use crate::calibration::{CalibrationState, validate_sleep_hr_bpm};
use crate::cli::args::CalibrationCommand;
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{CalibrationSetBoundaryParams, SetSeedParams};
use crate::paths::{EnvSource, ResolvedPaths};
use crate::storage::{
    AcceptShiftResult, LedgerStore, db_exists, db_path_in_data_dir, lock_path_in_data_dir,
};

use super::super::{
    OnlineOrOffline, confirm_action, connect_online_or_offline, ipc_request_params,
    ipc_request_typed, open_existing_store_rw, report_ipc_error, report_storage_error,
    write_json_line,
};

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
