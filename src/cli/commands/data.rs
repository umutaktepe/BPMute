use std::io::Write;

use chrono::Utc;
use serde_json::json;

use crate::cli::args::{DataCommand, ImportModeArg};
use crate::cli::commands::{
    OnlineOrOffline, confirm_action, confirm_wipe, connect_online_or_offline,
    open_existing_store_rw, report_ipc_error, report_storage_error, resolve_cli_path,
    write_json_line,
};
use crate::exit_code::ExitCode;
use crate::paths::{EnvSource, ResolvedPaths};
use crate::sleep_engine::CalibrationMaturity;
use crate::storage::{
    ExportBundleOptions, ImportMode, LedgerStore, StorageStats, db_path_in_data_dir,
    lock_path_in_data_dir,
};

pub(crate) async fn run_data<WOut: Write, WErr: Write>(
    action: DataCommand,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    match action {
        DataCommand::Stats { json } => {
            let stats: StorageStats = match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client.request("data.stats", json!({})).await {
                        Ok(val) => match serde_json::from_value(val) {
                            Ok(s) => s,
                            Err(err) => {
                                let _ = writeln!(stderr, "error: invalid IPC response: {err}");
                                return ExitCode::RuntimeError;
                            }
                        },
                        Err(err) => return report_ipc_error(err, stderr),
                    }
                }
                Ok(OnlineOrOffline::Offline) => match LedgerStore::open_read_only(&db_path) {
                    Ok(Some(store)) => match store.get_storage_stats() {
                        Ok(s) => s,
                        Err(err) => return report_storage_error(err, stderr),
                    },
                    Ok(None) => StorageStats {
                        db_size_bytes: 0,
                        db_size_formatted: "0 B".to_string(),
                        sessions_count: 0,
                        session_epochs_count: 0,
                        raw_samples_count: 0,
                        session_feedback_count: 0,
                        calibration_boundaries_count: 0,
                        wearables_count: 0,
                        raw_samples_retention: "none".to_string(),
                        session_epochs_retention: "none".to_string(),
                        calibration_maturity: CalibrationMaturity::ColdStart,
                        active_calibration_boundary: None,
                    },
                    Err(err) => return report_storage_error(err, stderr),
                },
                Err(code) => return code,
            };

            if json {
                return write_json_line(&stats, stdout, stderr);
            }

            let _ = writeln!(stdout, "Storage Statistics:");
            let _ = writeln!(
                stdout,
                "  Database size: {} ({} bytes)",
                stats.db_size_formatted, stats.db_size_bytes
            );
            let _ = writeln!(stdout, "  Sessions: {}", stats.sessions_count);
            let _ = writeln!(stdout, "  Epochs: {}", stats.session_epochs_count);
            let _ = writeln!(stdout, "  Raw samples: {}", stats.raw_samples_count);
            let _ = writeln!(
                stdout,
                "  Session feedback: {}",
                stats.session_feedback_count
            );
            let _ = writeln!(
                stdout,
                "  Calibration boundaries: {}",
                stats.calibration_boundaries_count
            );
            let _ = writeln!(stdout, "  Enrolled wearables: {}", stats.wearables_count);
            let _ = writeln!(stdout, "  Retention policies:");
            let _ = writeln!(stdout, "    Raw samples: {}", stats.raw_samples_retention);
            let _ = writeln!(stdout, "    Epochs: {}", stats.session_epochs_retention);
            let _ = writeln!(
                stdout,
                "  Calibration maturity: {:?}",
                stats.calibration_maturity
            );
            let _ = writeln!(
                stdout,
                "  Active calibration boundary: {}",
                if stats.active_calibration_boundary.is_some() {
                    "yes"
                } else {
                    "none"
                }
            );

            ExitCode::Success
        }

        DataCommand::PurgeRaw { yes } => {
            match confirm_action(
                "Purge and zero-overwrite all raw biometric samples across closed sessions?",
                yes,
                env,
                stdout,
                stderr,
            ) {
                Ok(true) => {}
                Ok(false) => return ExitCode::Success,
                Err(code) => return code,
            }

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client.request("data.purge_raw", json!({})).await {
                        Ok(val) => {
                            let deleted = val["deleted_raw_samples"].as_u64().unwrap_or(0);
                            let _ = writeln!(
                                stdout,
                                "Purged raw biometric samples ({deleted} rows deleted)."
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
                    match store.purge_raw_samples() {
                        Ok(deleted) => {
                            let _ = writeln!(
                                stdout,
                                "Purged raw biometric samples ({deleted} rows deleted)."
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }

        DataCommand::Wipe { all, confirm } => {
            match confirm_wipe(confirm.as_deref(), all, env, stdout, stderr) {
                Ok(true) => {}
                Ok(false) => return ExitCode::Success,
                Err(code) => return code,
            }

            let now_ms = Utc::now().timestamp_millis();
            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client
                        .request(
                            "data.hard_wipe",
                            json!({
                                "wipe_all": all,
                                "confirm": "WIPE"
                            }),
                        )
                        .await
                    {
                        Ok(_) => {
                            if all && paths.config_file.try_exists().unwrap_or(false) {
                                let _ = std::fs::remove_file(&paths.config_file);
                            }
                            if all {
                                let _ = writeln!(
                                    stdout,
                                    "Wiped all biometric session history, calibration, and enrolled wearables."
                                );
                            } else {
                                let _ = writeln!(
                                    stdout,
                                    "Wiped biometric session history and calibration."
                                );
                            }
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
                    match store.hard_wipe(all, now_ms) {
                        Ok(_) => {
                            if all && paths.config_file.try_exists().unwrap_or(false) {
                                let _ = std::fs::remove_file(&paths.config_file);
                            }
                            if all {
                                let _ = writeln!(
                                    stdout,
                                    "Wiped all biometric session history, calibration, and enrolled wearables."
                                );
                            } else {
                                let _ = writeln!(
                                    stdout,
                                    "Wiped biometric session history and calibration."
                                );
                            }
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }

        DataCommand::Export {
            path,
            include_epochs,
            include_wearables,
            include_raw,
        } => {
            let abs_path = resolve_cli_path(&path);
            let options = ExportBundleOptions {
                include_epochs,
                include_wearables,
                include_raw,
            };
            let now_ms = Utc::now().timestamp_millis();
            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client
                        .request(
                            "data.export_bundle",
                            json!({
                                "path": abs_path.to_string_lossy(),
                                "destination_path": abs_path.to_string_lossy(),
                                "include_epochs": options.include_epochs,
                                "include_wearables": options.include_wearables,
                                "include_raw": options.include_raw,
                            }),
                        )
                        .await
                    {
                        Ok(val) => {
                            let count = val["sessions_exported"].as_u64().unwrap_or(0);
                            let dest = val["destination_path"].as_str().unwrap_or("");
                            let _ = writeln!(
                                stdout,
                                "Exported ledger bundle ({count} sessions) to {dest}"
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_ipc_error(err, stderr),
                    }
                }
                Ok(OnlineOrOffline::Offline) => {
                    let store = match open_existing_store_rw(
                        &db_path,
                        &lock_path,
                        ExitCode::RuntimeError,
                        "error: ledger database not found",
                        stderr,
                    ) {
                        Ok(s) => s,
                        Err(code) => return code,
                    };
                    match store.export_bundle(&abs_path, options, now_ms) {
                        Ok(()) => {
                            let _ = writeln!(
                                stdout,
                                "Exported ledger bundle to {}",
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

        DataCommand::Import {
            path,
            mode,
            force,
            yes,
        } => {
            let abs_path = resolve_cli_path(&path);

            if mode == ImportModeArg::Replace {
                match confirm_action(
                    &format!(
                        "Replace mode will overwrite current ledger database with data from {} (a rollback backup will be created). Continue?",
                        abs_path.display()
                    ),
                    yes,
                    env,
                    stdout,
                    stderr,
                ) {
                    Ok(true) => {}
                    Ok(false) => return ExitCode::Success,
                    Err(code) => return code,
                }
            }

            let mode_str = match mode {
                ImportModeArg::Merge => "merge",
                ImportModeArg::Replace => "replace",
            };
            let import_mode = match mode {
                ImportModeArg::Merge => ImportMode::Merge,
                ImportModeArg::Replace => ImportMode::Replace,
            };
            let now_ms = Utc::now().timestamp_millis();

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client
                        .request(
                            "data.import_bundle",
                            json!({
                                "bundle_path": abs_path.to_string_lossy(),
                                "mode": mode_str,
                                "force": force,
                            }),
                        )
                        .await
                    {
                        Ok(val) => {
                            let mode = val["mode"].as_str().unwrap_or(mode_str);
                            let sessions = val["sessions_imported"].as_u64().unwrap_or(0);
                            let wearables = val["wearables_imported"].as_u64().unwrap_or(0);
                            let epochs = val["epochs_imported"].as_u64().unwrap_or(0);
                            let feedback = val["feedback_imported"].as_u64().unwrap_or(0);
                            let _ = writeln!(
                                stdout,
                                "Imported bundle ({mode}): {sessions} sessions, {wearables} wearables, {epochs} epochs, {feedback} feedback records imported. Calibration recomputed."
                            );
                            if let Some(bkp) = val["rollback_backup_path"].as_str() {
                                let _ = writeln!(stdout, "Rollback backup saved to: {bkp}");
                            }
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
                    match store.import_bundle(&abs_path, import_mode, force, now_ms) {
                        Ok((summary, _)) => {
                            let _ = writeln!(
                                stdout,
                                "Imported bundle ({}): {} sessions, {} wearables, {} epochs, {} feedback records imported. Calibration recomputed.",
                                summary.mode,
                                summary.sessions_imported,
                                summary.wearables_imported,
                                summary.epochs_imported,
                                summary.feedback_imported,
                            );
                            if let Some(ref bkp) = summary.rollback_backup_path {
                                let _ = writeln!(stdout, "Rollback backup saved to: {bkp}");
                            }
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }

        DataCommand::Backup { path } => {
            let abs_path = resolve_cli_path(&path);

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    match client
                        .request(
                            "data.backup_sqlite",
                            json!({
                                "destination_path": abs_path.to_string_lossy(),
                            }),
                        )
                        .await
                    {
                        Ok(_) => {
                            let _ = writeln!(
                                stdout,
                                "Backed up SQLite database to {}",
                                abs_path.display()
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_ipc_error(err, stderr),
                    }
                }
                Ok(OnlineOrOffline::Offline) => {
                    let store = match open_existing_store_rw(
                        &db_path,
                        &lock_path,
                        ExitCode::RuntimeError,
                        "error: ledger database not found",
                        stderr,
                    ) {
                        Ok(s) => s,
                        Err(code) => return code,
                    };
                    match store.backup_sqlite(&abs_path) {
                        Ok(_) => {
                            let _ = writeln!(
                                stdout,
                                "Backed up SQLite database to {}",
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
