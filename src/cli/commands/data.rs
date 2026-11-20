use std::io::Write;

use chrono::Utc;
use serde_json::{Value, json};

use crate::cli::args::{DataCommand, ImportModeArg};
use crate::cli::commands::{
    OnlineOrOffline, confirm_action, confirm_wipe, connect_online_or_offline, ipc_request_params,
    ipc_request_typed, open_existing_store_rw, report_storage_error, resolve_cli_path,
    write_json_line,
};
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{
    DataBackupSqliteParams, DataExportBundleParams, DataExportBundleResponse, DataHardWipeParams,
    DataImportBundleParams, DataImportBundleResponse, DataPurgeRawResponse,
};
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
                    match ipc_request_typed(&mut client, "data.stats", json!({}), stderr).await {
                        Ok(s) => s,
                        Err(code) => return code,
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
                    match ipc_request_typed::<DataPurgeRawResponse, _>(
                        &mut client,
                        "data.purge_raw",
                        json!({}),
                        stderr,
                    )
                    .await
                    {
                        Ok(resp) => {
                            let _ = writeln!(
                                stdout,
                                "Purged raw biometric samples ({} rows deleted).",
                                resp.reclaimed_rows
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
                    let params = DataHardWipeParams { all };
                    match ipc_request_params::<_, Value, _>(
                        &mut client,
                        "data.hard_wipe",
                        &params,
                        stderr,
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
                    let params = DataExportBundleParams {
                        path: abs_path.to_string_lossy().into_owned(),
                        include_epochs: options.include_epochs,
                        include_wearables: options.include_wearables,
                        include_raw: options.include_raw,
                    };
                    match ipc_request_params::<_, DataExportBundleResponse, _>(
                        &mut client,
                        "data.export_bundle",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(resp) => {
                            let _ = writeln!(stdout, "Exported ledger bundle to {}", resp.path);
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

            let import_mode = match mode {
                ImportModeArg::Merge => ImportMode::Merge,
                ImportModeArg::Replace => ImportMode::Replace,
            };
            let now_ms = Utc::now().timestamp_millis();

            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let params = DataImportBundleParams {
                        path: abs_path.to_string_lossy().into_owned(),
                        mode: Some(import_mode),
                        force,
                    };
                    match ipc_request_params::<_, DataImportBundleResponse, _>(
                        &mut client,
                        "data.import_bundle",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(resp) => {
                            let summary = resp.summary;
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
                    let params = DataBackupSqliteParams {
                        path: abs_path.to_string_lossy().into_owned(),
                    };
                    match ipc_request_params::<_, Value, _>(
                        &mut client,
                        "data.backup_sqlite",
                        &params,
                        stderr,
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
                        Err(code) => code,
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
