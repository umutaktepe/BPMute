use std::sync::atomic::AtomicU64;

use serde_json::json;
use tokio::sync::mpsc;

use super::client::{
    PendingMap, send_op_request, send_request, send_typed_op_request, send_typed_request,
};
use super::event::{IpcEvent, TuiCommand, TuiEvent};
use crate::calibration::CalibrationState;
use crate::config::PatchField;
use crate::ipc::protocol::{
    AlertDismissParams, CalibrationSetBoundaryParams, ConfigSnapshotPayload, DaemonStatus,
    DataHardWipeParams, DiscoveredWearable, EnrollSelectParams, EnrollSelectResponse,
    EnrollStartParams, RequestEnvelope, SessionDeleteParams, SessionExportTraceParams,
    SessionGetParams, SessionListParams, SetFeedbackParams, SetSeedParams, SnoozeGraceParams,
    WearableConnectParams, WearableListResponse, WearableRenameParams, WearableUnenrollParams,
};
use crate::storage::{SessionEpochsPage, SessionFullDetail, SessionListPage, StorageStats};

pub(super) async fn execute_command(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    cmd: TuiCommand,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    match cmd {
        TuiCommand::Pause(params) => {
            send_typed_op_request(req_tx, pending, next_id, "monitor.pause", &params, event_tx)
                .await?;
        }
        TuiCommand::Resume => {
            send_op_request(
                req_tx,
                pending,
                next_id,
                "monitor.resume",
                json!({}),
                event_tx,
            )
            .await?;
        }
        TuiCommand::CancelGrace => {
            send_op_request(
                req_tx,
                pending,
                next_id,
                "monitor.cancel_grace",
                json!({}),
                event_tx,
            )
            .await?;
        }
        TuiCommand::SnoozeGrace(duration_sec) => {
            let params = SnoozeGraceParams {
                duration_sec,
                duration_min: None,
            };
            send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "monitor.snooze_grace",
                &params,
                event_tx,
            )
            .await?;
        }
        TuiCommand::EnrollStart(timeout) => {
            let params = EnrollStartParams {
                timeout_sec: Some(timeout.unwrap_or(30)),
            };
            let resp =
                send_typed_request(req_tx, pending, next_id, "wearable.enroll_start", &params)
                    .await?;
            if !resp.ok {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::OpCompleted {
                        op: "wearable.enroll_start".into(),
                        success: false,
                        message: resp.error.map(|e| e.message),
                    }))
                    .await;
            } else if let Some(data) = resp.data
                && let Some(devs) = data.get("wearables").or_else(|| data.get("devices"))
                && let Ok(discovered) =
                    serde_json::from_value::<Vec<DiscoveredWearable>>(devs.clone())
            {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::EnrollmentDiscovered(discovered)))
                    .await;
            }
        }
        TuiCommand::EnrollSelect { address, alias } => {
            let params = EnrollSelectParams { address, alias };
            let resp =
                send_typed_request(req_tx, pending, next_id, "wearable.enroll_select", &params)
                    .await?;
            let message = if resp.ok {
                resp.data
                    .as_ref()
                    .and_then(|d| serde_json::from_value::<EnrollSelectResponse>(d.clone()).ok())
                    .filter(|r| !r.warnings.is_empty())
                    .map(|r| r.warnings.join(" "))
            } else {
                resp.error.as_ref().map(|e| e.message.clone())
            };
            let _ = event_tx
                .send(TuiEvent::ipc(IpcEvent::OpCompleted {
                    op: "wearable.enroll_select".to_string(),
                    success: resp.ok,
                    message,
                }))
                .await;
            if resp.ok {
                refresh_wearables(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::EnrollCancel => {
            send_request(
                req_tx,
                pending,
                next_id,
                "wearable.enroll_cancel",
                json!({}),
            )
            .await?;
        }
        TuiCommand::EnrollAuthReply(params) => {
            send_typed_request(
                req_tx,
                pending,
                next_id,
                "wearable.enroll_auth_response",
                &params,
            )
            .await?;
        }
        TuiCommand::WearableRename { id, alias } => {
            let params = WearableRenameParams { id, alias };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.rename",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_wearables(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::WearableConnect { id } => {
            let params = WearableConnectParams { id };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.connect",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_wearables(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::WearableUnenroll {
            id,
            forget_bluez,
            delete_sessions,
        } => {
            let params = WearableUnenrollParams {
                id,
                forget_bluez,
                delete_sessions,
            };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.unenroll",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_wearables(req_tx, pending, next_id, event_tx).await?;
                if delete_sessions {
                    refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                    refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                    refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
                }
            }
        }
        TuiCommand::AcceptShift => {
            send_op_request(
                req_tx,
                pending,
                next_id,
                "calibration.accept_shift",
                json!({}),
                event_tx,
            )
            .await?;
        }
        TuiCommand::DismissShift => {
            send_op_request(
                req_tx,
                pending,
                next_id,
                "calibration.dismiss_shift",
                json!({}),
                event_tx,
            )
            .await?;
        }
        TuiCommand::DismissAlert { id } => {
            let params = AlertDismissParams { id };
            send_typed_request(req_tx, pending, next_id, "alert.dismiss", &params).await?;
        }
        TuiCommand::RefreshStatus => {
            refresh_status_event(req_tx, pending, next_id, event_tx, false).await?;
        }
        TuiCommand::RefreshSessions => {
            refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
        }
        TuiCommand::SessionGet { id } => {
            refresh_session_detail(req_tx, pending, next_id, &id, event_tx).await?;
        }
        TuiCommand::SessionSetFeedback {
            session_id,
            verdict,
            onset_hhmm,
            avg_sleep_hr,
            clear,
        } => {
            let params = if clear {
                SetFeedbackParams {
                    session_id: session_id.clone(),
                    verdict: PatchField::Unchanged,
                    onset_hhmm: PatchField::Unchanged,
                    avg_sleep_hr: PatchField::Unchanged,
                    clear: true,
                }
            } else {
                SetFeedbackParams {
                    session_id: session_id.clone(),
                    verdict: verdict.map_or(PatchField::Unchanged, PatchField::Set),
                    onset_hhmm: onset_hhmm.map_or(PatchField::Unchanged, PatchField::Set),
                    avg_sleep_hr: avg_sleep_hr.map_or(PatchField::Unchanged, PatchField::Set),
                    clear: false,
                }
            };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "session.set_feedback",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_session_detail(req_tx, pending, next_id, &session_id, event_tx).await?;
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::SessionExportTrace {
            id,
            path,
            format,
            no_anonymize,
        } => {
            let params = SessionExportTraceParams {
                id,
                path,
                format: Some(format),
                no_anonymize,
            };
            send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "session.export_trace",
                &params,
                event_tx,
            )
            .await?;
        }
        TuiCommand::SessionDelete { id } => {
            let params = SessionDeleteParams { id };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "session.delete",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::SessionEnd => {
            let resp = send_request(req_tx, pending, next_id, "session.end", json!({})).await?;
            let discarded = resp
                .data
                .as_ref()
                .and_then(|d| d.get("discarded"))
                .and_then(serde_json::Value::as_bool)
                == Some(true);
            let message = if resp.ok {
                discarded.then(|| "Incomplete Warmup session discarded".to_string())
            } else {
                resp.error.as_ref().map(|e| e.message.clone())
            };
            let _ = event_tx
                .send(TuiEvent::ipc(IpcEvent::OpCompleted {
                    op: "session.end".to_string(),
                    success: resp.ok,
                    message,
                }))
                .await;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::ConfigGet => {
            refresh_config(req_tx, pending, next_id, event_tx).await?;
        }
        TuiCommand::ConfigUpdate(patch) => {
            let resp =
                send_typed_op_request(req_tx, pending, next_id, "config.update", &patch, event_tx)
                    .await?;
            if resp.ok {
                refresh_config(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::ConfigReload => {
            let resp = send_op_request(
                req_tx,
                pending,
                next_id,
                "config.reload",
                json!({}),
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_config(req_tx, pending, next_id, event_tx).await?;
            } else if let Some(err) = resp.error {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::ConfigError(err.message)))
                    .await;
            }
        }
        TuiCommand::CalibrationSetSeed(seed) => {
            let params = SetSeedParams { sleep_hr: seed };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "calibration.set_seed",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::CalibrationSetBoundary {
            effective_ts_ms,
            seed_sleep_hr,
            reset_floors,
        } => {
            let params = CalibrationSetBoundaryParams {
                effective_ts_ms,
                seed_sleep_hr,
                reset_floors,
            };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "calibration.set_boundary",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::CalibrationUndoBoundary => {
            let resp = send_op_request(
                req_tx,
                pending,
                next_id,
                "calibration.undo_boundary",
                json!({}),
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::DataStats => {
            refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
        }
        TuiCommand::DataPurgeRaw => {
            let resp = send_op_request(
                req_tx,
                pending,
                next_id,
                "data.purge_raw",
                json!({}),
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::DataHardWipe {
            confirm: _,
            wipe_all,
        } => {
            let params = DataHardWipeParams { all: wipe_all };
            let resp = send_typed_op_request(
                req_tx,
                pending,
                next_id,
                "data.hard_wipe",
                &params,
                event_tx,
            )
            .await?;
            if resp.ok {
                refresh_sessions(req_tx, pending, next_id, event_tx, false).await?;
                refresh_wearables(req_tx, pending, next_id, event_tx).await?;
                refresh_calibration(req_tx, pending, next_id, event_tx).await?;
                refresh_storage_stats(req_tx, pending, next_id, event_tx).await?;
            }
        }
        TuiCommand::EditorHandoff => {}
    }
    Ok(())
}

pub(super) async fn refresh_sessions(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
    load_first_detail: bool,
) -> Result<(), std::io::Error> {
    let list_params = SessionListParams {
        limit: Some(100),
        offset: Some(0),
    };
    let resp = send_typed_request(req_tx, pending, next_id, "session.list", &list_params).await?;

    let sessions = if let Some(data) = resp.data
        && let Ok(page) = serde_json::from_value::<SessionListPage>(data)
    {
        page.sessions
    } else {
        Vec::new()
    };

    // Also fetch calibration boundaries from calibration.list_boundaries
    let bnd_resp = send_request(
        req_tx,
        pending,
        next_id,
        "calibration.list_boundaries",
        json!({}),
    )
    .await?;
    let boundaries = if let Some(data) = bnd_resp.data
        && let Ok(list) =
            serde_json::from_value::<Vec<crate::calibration::CalibrationBoundaryRecord>>(data)
    {
        list
    } else {
        Vec::new()
    };

    let first_id = sessions.first().map(|s| s.id.clone());

    let _ = event_tx
        .send(TuiEvent::ipc(IpcEvent::SessionsUpdated {
            sessions,
            boundaries,
        }))
        .await;

    // On initial connect, pre-load detail for the first session if available
    if load_first_detail && let Some(id) = first_id {
        let _ = refresh_session_detail(req_tx, pending, next_id, &id, event_tx).await;
    }

    Ok(())
}

pub(super) async fn refresh_session_detail(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    id: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    let params = SessionGetParams { id: id.to_string() };
    let resp = send_typed_request(req_tx, pending, next_id, "session.get", &params).await?;

    let Some(data) = resp.data else {
        if let Some(err) = resp.error {
            let _ = event_tx
                .send(TuiEvent::ipc(IpcEvent::OpCompleted {
                    op: "session.get".to_string(),
                    success: false,
                    message: Some(err.message),
                }))
                .await;
        }
        return Ok(());
    };
    let Ok(mut detail) = serde_json::from_value::<SessionFullDetail>(data) else {
        return Ok(());
    };

    let mut paginator = crate::ipc::client::EpochPaginator::new(&detail);
    while let Some(page_params) = paginator.next_params() {
        let page_resp =
            send_typed_request(req_tx, pending, next_id, "session.epochs", &page_params).await?;
        let page_res: Result<SessionEpochsPage, String> = if !page_resp.ok {
            Err(page_resp
                .error
                .map(|e| e.message)
                .unwrap_or_else(|| "session.epochs failed".to_string()))
        } else if let Some(page_data) = page_resp.data {
            serde_json::from_value::<SessionEpochsPage>(page_data)
                .map_err(|e| format!("invalid session.epochs payload: {e}"))
        } else {
            Err("session.epochs returned empty payload".to_string())
        };
        let page = match page_res {
            Ok(page) => page,
            Err(msg) => {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::OpCompleted {
                        op: "session.epochs".to_string(),
                        success: false,
                        message: Some(msg),
                    }))
                    .await;
                return Ok(());
            }
        };
        if !paginator.push_page(&mut detail, page) {
            break;
        }
    }

    let _ = event_tx
        .send(TuiEvent::ipc(IpcEvent::SessionDetailLoaded(Box::new(
            detail,
        ))))
        .await;
    Ok(())
}

pub(super) async fn refresh_config(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    let resp = send_request(req_tx, pending, next_id, "config.get", json!({})).await?;
    if let Some(data) = resp.data
        && let Ok(payload) = serde_json::from_value::<ConfigSnapshotPayload>(data)
    {
        let _ = event_tx
            .send(TuiEvent::ipc(IpcEvent::ConfigLoaded(payload.config)))
            .await;
    }
    Ok(())
}

pub(super) async fn refresh_storage_stats(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    let resp = send_request(req_tx, pending, next_id, "data.stats", json!({})).await?;
    if let Some(data) = resp.data
        && let Ok(stats) = serde_json::from_value::<StorageStats>(data)
    {
        let _ = event_tx
            .send(TuiEvent::ipc(IpcEvent::StorageStatsLoaded(stats)))
            .await;
    }
    Ok(())
}

pub(super) async fn refresh_status_event(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
    initial: bool,
) -> Result<(), std::io::Error> {
    let status_resp = send_request(req_tx, pending, next_id, "daemon.status", json!({})).await?;
    if let Some(data) = status_resp.data
        && let Ok(status) = serde_json::from_value::<DaemonStatus>(data)
    {
        let ev = if initial {
            IpcEvent::Connected(status)
        } else {
            IpcEvent::StateChanged(status)
        };
        let _ = event_tx.send(TuiEvent::ipc(ev)).await;
    }
    Ok(())
}

pub(super) async fn refresh_wearables(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    let resp = send_request(req_tx, pending, next_id, "wearable.list", json!({})).await?;
    if let Some(data) = resp.data
        && let Ok(list) = serde_json::from_value::<WearableListResponse>(data)
    {
        let _ = event_tx
            .send(TuiEvent::ipc(IpcEvent::WearablesUpdated(list.wearables)))
            .await;
    }
    Ok(())
}

pub(super) async fn refresh_calibration(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    let resp = send_request(req_tx, pending, next_id, "calibration.status", json!({})).await?;
    if let Some(data) = resp.data
        && let Ok(cal) = serde_json::from_value::<CalibrationState>(data)
    {
        let _ = event_tx
            .send(TuiEvent::ipc(IpcEvent::CalibrationUpdated(cal)))
            .await;
    }
    Ok(())
}
