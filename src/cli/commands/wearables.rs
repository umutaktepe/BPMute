//! Wearable allowlist, unenrollment, renaming, connection, and enrollment CLI commands.

use std::io::Write;

use chrono::Utc;
use serde_json::json;

use crate::cli::args::WearablesCommand;
use crate::exit_code::ExitCode;
use crate::ipc::protocol::{
    EnrollSelectParams, EnrollSelectResponse, EnrollStartParams, EnrollStartResponse,
    EnrolledWearableStatus, WearableConnectParams, WearableListResponse, WearableMutationResponse,
    WearableRenameParams, WearableUnenrollParams,
};
use crate::ipc::{IpcClient, IpcClientError};
use crate::paths::ResolvedPaths;
use crate::sleep_engine::StreamStatus;
use crate::storage::{LedgerStore, db_path_in_data_dir, lock_path_in_data_dir};

use super::{
    OnlineOrOffline, connect_online_or_offline, ipc_request_params, ipc_request_typed,
    open_existing_store_rw, report_ipc_error, report_storage_error, write_json_line,
};

pub(crate) async fn run_wearables<WOut: Write, WErr: Write>(
    action: WearablesCommand,
    paths: &ResolvedPaths,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    match action {
        WearablesCommand::List { json } => {
            let wearables: Vec<EnrolledWearableStatus> =
                match connect_online_or_offline(paths, stderr).await {
                    Ok(OnlineOrOffline::Online(mut client)) => {
                        match ipc_request_typed::<WearableListResponse, _>(
                            &mut client,
                            "wearable.list",
                            json!({}),
                            stderr,
                        )
                        .await
                        {
                            Ok(resp) => resp.wearables,
                            Err(code) => return code,
                        }
                    }
                    Ok(OnlineOrOffline::Offline) => match LedgerStore::open_read_only(&db_path) {
                        Ok(Some(store)) => match store.list_enrolled_wearables() {
                            Ok(enrolled) => enrolled
                                .into_iter()
                                .map(|w| EnrolledWearableStatus {
                                    wearable: w,
                                    status: StreamStatus::Disconnected,
                                    battery: None,
                                    body_location: None,
                                })
                                .collect(),
                            Err(err) => return report_storage_error(err, stderr),
                        },
                        Ok(None) => Vec::new(),
                        Err(err) => return report_storage_error(err, stderr),
                    },
                    Err(code) => return code,
                };

            if json {
                return write_json_line(&json!({ "wearables": wearables }), stdout, stderr);
            }

            if wearables.is_empty() {
                let _ = writeln!(
                    stdout,
                    "No enrolled Wearables found. Use `bpmute enroll` to add one."
                );
                return ExitCode::Success;
            }

            let _ = writeln!(stdout, "Enrolled Wearables ({} total):", wearables.len());
            for item in &wearables {
                let w = &item.wearable;
                let alias_part = w.alias.as_deref().unwrap_or("-");
                let name_part = w.advertised_name.as_deref().unwrap_or("-");
                let battery_part = match item.battery {
                    Some(pct) => format!("{pct}%"),
                    None => "-".to_string(),
                };
                let _ = writeln!(
                    stdout,
                    "  {} | {} ({}) | alias: {} | name: {} | status: {} | battery: {}",
                    w.id,
                    w.mac_masked,
                    w.address_type,
                    alias_part,
                    name_part,
                    item.status.display_label(),
                    battery_part
                );
            }
            ExitCode::Success
        }
        WearablesCommand::Rename { id, alias } => {
            match connect_online_or_offline(paths, stderr).await {
                Ok(OnlineOrOffline::Online(mut client)) => {
                    let params = WearableRenameParams {
                        id: id.clone(),
                        alias: Some(alias.clone()),
                    };
                    match ipc_request_params::<_, WearableMutationResponse, _>(
                        &mut client,
                        "wearable.rename",
                        &params,
                        stderr,
                    )
                    .await
                    {
                        Ok(resp) => {
                            let _ = writeln!(
                                stdout,
                                "Renamed Wearable {} ({}) -> {}",
                                resp.wearable.id,
                                resp.wearable.mac_masked,
                                resp.wearable.alias.as_deref().unwrap_or("<none>")
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
                        "error: no database found",
                        stderr,
                    ) {
                        Ok(store) => store,
                        Err(code) => return code,
                    };
                    match store.rename_wearable(&id, Some(&alias)) {
                        Ok(updated) => {
                            let _ = writeln!(
                                stdout,
                                "Renamed Wearable {} ({}) -> {}",
                                updated.id,
                                updated.mac_masked,
                                updated.alias.as_deref().unwrap_or("<none>")
                            );
                            ExitCode::Success
                        }
                        Err(err) => report_storage_error(err, stderr),
                    }
                }
                Err(code) => code,
            }
        }
        WearablesCommand::Connect { id } => match connect_online_or_offline(paths, stderr).await {
            Ok(OnlineOrOffline::Online(mut client)) => {
                let params = WearableConnectParams { id };
                match ipc_request_params::<_, serde_json::Value, _>(
                    &mut client,
                    "wearable.connect",
                    &params,
                    stderr,
                )
                .await
                {
                    Ok(val) => {
                        let msg = val["message"].as_str().unwrap_or("connection requested");
                        let _ = writeln!(stdout, "{msg}");
                        ExitCode::Success
                    }
                    Err(code) => code,
                }
            }
            Ok(OnlineOrOffline::Offline) => {
                let _ = writeln!(
                    stderr,
                    "error: daemon is offline; connecting a wearable requires a running daemon"
                );
                ExitCode::DaemonOffline
            }
            Err(code) => code,
        },
        WearablesCommand::Unenroll {
            id,
            forget_bluez,
            delete_sessions,
        } => match connect_online_or_offline(paths, stderr).await {
            Ok(OnlineOrOffline::Online(mut client)) => {
                let params = WearableUnenrollParams {
                    id,
                    forget_bluez,
                    delete_sessions,
                };
                match ipc_request_params::<_, WearableMutationResponse, _>(
                    &mut client,
                    "wearable.unenroll",
                    &params,
                    stderr,
                )
                .await
                {
                    Ok(resp) => {
                        let _ = writeln!(
                            stdout,
                            "Unenrolled Wearable {} ({})",
                            resp.wearable.id, resp.wearable.mac_masked
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
                    "error: no database found",
                    stderr,
                ) {
                    Ok(store) => store,
                    Err(code) => return code,
                };
                let now_ms = Utc::now().timestamp_millis();
                match store.unenroll_wearable(&id, delete_sessions, now_ms) {
                    Ok((w, _)) => {
                        if forget_bluez {
                            let _ = writeln!(
                                stderr,
                                "note: daemon is offline; bluez device pairing retained"
                            );
                        }
                        let _ = writeln!(stdout, "Unenrolled Wearable {} ({})", w.id, w.mac_masked);
                        ExitCode::Success
                    }
                    Err(err) => report_storage_error(err, stderr),
                }
            }
            Err(code) => code,
        },
        WearablesCommand::Enroll { address, alias } => {
            run_enroll(address.as_deref(), alias.as_deref(), paths, stdout, stderr).await
        }
    }
}

pub(crate) async fn run_enroll<WOut: Write, WErr: Write>(
    explicit_address: Option<&str>,
    alias: Option<&str>,
    paths: &ResolvedPaths,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let mut client = match IpcClient::connect(&paths.socket_path).await {
        Ok(c) => c,
        Err(IpcClientError::DaemonOffline { .. }) => {
            let _ = writeln!(
                stderr,
                "error: daemon is offline; wearable enrollment requires a running daemon"
            );
            return ExitCode::DaemonOffline;
        }
        Err(err) => return report_ipc_error(err, stderr),
    };

    let start_params = EnrollStartParams {
        timeout_sec: Some(30),
    };
    let start_res = match ipc_request_params::<_, EnrollStartResponse, _>(
        &mut client,
        "wearable.enroll_start",
        &start_params,
        stderr,
    )
    .await
    {
        Ok(res) => res,
        Err(code) => return code,
    };

    let target_mac = match explicit_address {
        Some(mac) => mac.to_string(),
        None => {
            if start_res.devices.is_empty() {
                let _ = writeln!(
                    stderr,
                    "error: no 0x180D Heart Rate devices discovered. Specify --address <MAC> or ensure wearable is advertising."
                );
                let _ = client.request("wearable.enroll_cancel", json!({})).await;
                return ExitCode::UsageOrConfigError;
            }
            let _ = writeln!(
                stdout,
                "Discovered {} Wearable(s):",
                start_res.devices.len()
            );
            for (idx, d) in start_res.devices.iter().enumerate() {
                let name = d.name.as_deref().unwrap_or("Unknown");
                let rssi_str = d
                    .rssi
                    .map(|r| format!("{r} dBm"))
                    .unwrap_or_else(|| "-".to_string());
                let _ = writeln!(
                    stdout,
                    "  [{}] {} ({}) - rssi: {}, paired: {}",
                    idx + 1,
                    d.address,
                    name,
                    rssi_str,
                    d.paired
                );
            }
            let _ = writeln!(
                stdout,
                "\nTo complete enrollment, run:\n  bpmute enroll --address <ADDRESS>{}\n",
                if alias.is_some() {
                    " --alias <ALIAS>"
                } else {
                    ""
                }
            );
            let _ = client.request("wearable.enroll_cancel", json!({})).await;
            return ExitCode::Success;
        }
    };

    let select_params = EnrollSelectParams {
        address: target_mac,
        alias: alias.map(str::to_owned),
    };
    let select_res = match ipc_request_params::<_, EnrollSelectResponse, _>(
        &mut client,
        "wearable.enroll_select",
        &select_params,
        stderr,
    )
    .await
    {
        Ok(res) => res,
        Err(code) => return code,
    };

    for warning in &select_res.warnings {
        let _ = writeln!(stderr, "warning: {warning}");
    }
    for hint in &select_res.hints {
        let _ = writeln!(stdout, "hint: {hint}");
    }

    let _ = writeln!(
        stdout,
        "Successfully enrolled wearable {} ({})",
        select_res.wearable.id, select_res.wearable.mac_masked
    );
    ExitCode::Success
}
