use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use super::event::{IpcEvent, TuiCommand, TuiEvent};
use crate::calibration::CalibrationState;
use crate::ipc::framing::{read_bounded_line, write_ndjson_line};
use crate::ipc::protocol::{
    DaemonAlert, DaemonStatus, DiscoveredWearable, EnrolledWearableStatus, EnrollmentUpdateEvent,
    EpochUpdateEvent, GraceTickEvent, HrSampleEvent, MAX_FRAME_BYTES, PROTOCOL_VERSION,
    PushEventEnvelope, RequestEnvelope, ResponseEnvelope,
};

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<ResponseEnvelope>>>>;

pub struct TuiIpcBridge {
    socket_path: PathBuf,
    event_tx: mpsc::Sender<TuiEvent>,
    cmd_rx: mpsc::Receiver<TuiCommand>,
    shutdown_rx: watch::Receiver<bool>,
    next_id: Arc<AtomicU64>,
}

impl TuiIpcBridge {
    pub fn new(
        socket_path: PathBuf,
        event_tx: mpsc::Sender<TuiEvent>,
        cmd_rx: mpsc::Receiver<TuiCommand>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            socket_path,
            event_tx,
            cmd_rx,
            shutdown_rx,
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn run(mut self) {
        while !*self.shutdown_rx.borrow() {
            // Attempt connection
            let stream = match UnixStream::connect(&self.socket_path).await {
                Ok(s) => s,
                Err(_) => {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(1000)) => continue,
                        _ = self.shutdown_rx.changed() => break,
                    }
                }
            };

            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
            let (req_tx, mut req_rx) = mpsc::channel::<RequestEnvelope>(64);

            // Dedicated writer task
            let mut writer_task = tokio::spawn(async move {
                while let Some(envelope) = req_rx.recv().await {
                    if write_ndjson_line(&mut write_half, &envelope).await.is_err() {
                        break;
                    }
                }
            });

            // Background reader task
            let pending_clone = Arc::clone(&pending);
            let event_tx_clone = self.event_tx.clone();
            let mut reader_shutdown = self.shutdown_rx.clone();

            let mut reader_task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        line_res = read_bounded_line(&mut reader, MAX_FRAME_BYTES) => {
                            match line_res {
                                Ok(Some(line)) => {
                                    if let Ok(raw) = serde_json::from_str::<Value>(&line) {
                                        if raw.get("event").is_some() {
                                            if let Ok(push) = serde_json::from_value::<PushEventEnvelope>(raw) {
                                                handle_incoming_push(push, &event_tx_clone).await;
                                            }
                                        } else if let Ok(resp) = serde_json::from_value::<ResponseEnvelope>(raw) {
                                            let mut map = pending_clone.lock().await;
                                            if let Some(tx) = map.remove(&resp.id) {
                                                let _ = tx.send(resp);
                                            }
                                        }
                                    }
                                }
                                _ => break,
                            }
                        }
                        _ = reader_shutdown.changed() => break,
                    }
                }
                pending_clone.lock().await.clear();
            });

            // Initial setup: subscribe, fetch status, wearables, and calibration priors
            let subscribe_res = send_request(
                &req_tx,
                &pending,
                &self.next_id,
                "daemon.subscribe",
                json!({}),
            )
            .await;

            if subscribe_res.is_err() {
                reader_task.abort();
                writer_task.abort();
                let _ = self
                    .event_tx
                    .send(TuiEvent::ipc(IpcEvent::Disconnected))
                    .await;
                continue;
            }

            if refresh_status_event(&req_tx, &pending, &self.next_id, &self.event_tx, true)
                .await
                .is_err()
                || refresh_wearables(&req_tx, &pending, &self.next_id, &self.event_tx)
                    .await
                    .is_err()
                || refresh_calibration(&req_tx, &pending, &self.next_id, &self.event_tx)
                    .await
                    .is_err()
            {
                reader_task.abort();
                writer_task.abort();
                let _ = self
                    .event_tx
                    .send(TuiEvent::ipc(IpcEvent::Disconnected))
                    .await;
                continue;
            }

            // Concurrent command loop
            let mut cmd_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
            let mut conn_active = true;
            while conn_active && !*self.shutdown_rx.borrow() {
                tokio::select! {
                    cmd_opt = self.cmd_rx.recv() => {
                        let Some(cmd) = cmd_opt else { break; };
                        let req_tx_c = req_tx.clone();
                        let pending_c = Arc::clone(&pending);
                        let next_id_c = Arc::clone(&self.next_id);
                        let event_tx_c = self.event_tx.clone();
                        cmd_tasks.spawn(async move {
                            let _ = execute_command(
                                &req_tx_c,
                                &pending_c,
                                &next_id_c,
                                cmd,
                                &event_tx_c,
                            ).await;
                        });
                    }
                    Some(_) = cmd_tasks.join_next(), if !cmd_tasks.is_empty() => {}
                    _ = &mut reader_task => {
                        conn_active = false;
                    }
                    _ = &mut writer_task => {
                        conn_active = false;
                    }
                    _ = self.shutdown_rx.changed() => {
                        conn_active = false;
                    }
                }
            }

            cmd_tasks.abort_all();
            while cmd_tasks.join_next().await.is_some() {}
            reader_task.abort();
            writer_task.abort();
            pending.lock().await.clear();

            let _ = self
                .event_tx
                .send(TuiEvent::ipc(IpcEvent::Disconnected))
                .await;

            if *self.shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                _ = self.shutdown_rx.changed() => break,
            }
        }
    }
}

async fn handle_incoming_push(push: PushEventEnvelope, event_tx: &mpsc::Sender<TuiEvent>) {
    match push.event.as_str() {
        "Snapshot" => {
            let status = push
                .data
                .get("status")
                .and_then(|v| serde_json::from_value::<DaemonStatus>(v.clone()).ok());
            let hr_ring = push
                .data
                .get("hr_ring_buffer")
                .and_then(|v| serde_json::from_value::<Vec<HrSampleEvent>>(v.clone()).ok())
                .unwrap_or_default();
            let alerts = push
                .data
                .get("alerts")
                .and_then(|v| serde_json::from_value::<Vec<DaemonAlert>>(v.clone()).ok())
                .unwrap_or_default();

            if let Some(st) = status {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::Snapshot {
                        status: st,
                        hr_ring_buffer: hr_ring,
                        alerts,
                    }))
                    .await;
            }
        }
        "HrSample" => {
            if let Ok(sample) = serde_json::from_value::<HrSampleEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::HrSample(sample)))
                    .await;
            }
        }
        "GraceTick" => {
            if let Ok(tick) = serde_json::from_value::<GraceTickEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::GraceTick(tick)))
                    .await;
            }
        }
        "EpochUpdate" => {
            if let Ok(epoch) = serde_json::from_value::<EpochUpdateEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::EpochUpdate(epoch)))
                    .await;
            }
        }
        "StateChanged" => {
            if let Ok(status) = serde_json::from_value::<DaemonStatus>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::StateChanged(status)))
                    .await;
            }
        }
        "AlertChanged" => {
            if let Some(alerts_val) = push.data.get("alerts")
                && let Ok(alerts) = serde_json::from_value::<Vec<DaemonAlert>>(alerts_val.clone())
            {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::AlertChanged(alerts)))
                    .await;
            }
        }
        "CalibrationUpdated" => {
            if let Some(cal_val) = push.data.get("calibration")
                && let Ok(cal) = serde_json::from_value::<CalibrationState>(cal_val.clone())
            {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::CalibrationUpdated(cal)))
                    .await;
            }
        }
        "EnrollmentUpdate" => {
            if let Ok(update) = serde_json::from_value::<EnrollmentUpdateEvent>(push.data.clone()) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::EnrollmentUpdate(update)))
                    .await;
            } else if let Some(devices_val) = push
                .data
                .get("wearables")
                .or_else(|| push.data.get("devices"))
                && let Ok(devices) =
                    serde_json::from_value::<Vec<DiscoveredWearable>>(devices_val.clone())
            {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::EnrollmentDiscovered(devices)))
                    .await;
            }
        }
        _ => {}
    }
}

async fn send_request(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    op: &str,
    params: Value,
) -> Result<ResponseEnvelope, std::io::Error> {
    let req_id = next_id.fetch_add(1, Ordering::Relaxed);
    let envelope = RequestEnvelope {
        v: PROTOCOL_VERSION,
        id: req_id,
        op: op.to_string(),
        params,
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    {
        let mut map = pending.lock().await;
        map.insert(req_id, reply_tx);
    }

    if req_tx.send(envelope).await.is_err() {
        let mut map = pending.lock().await;
        map.remove(&req_id);
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "IPC writer task closed",
        ));
    }

    match reply_rx.await {
        Ok(resp) => Ok(resp),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "reply channel closed",
        )),
    }
}

async fn send_op_request(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    op: &str,
    params: Value,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<ResponseEnvelope, std::io::Error> {
    let resp = send_request(req_tx, pending, next_id, op, params).await?;
    let _ = event_tx
        .send(TuiEvent::ipc(IpcEvent::OpCompleted {
            op: op.to_string(),
            success: resp.ok,
            message: resp.error.as_ref().map(|e| e.message.clone()),
        }))
        .await;
    Ok(resp)
}

async fn execute_command(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    cmd: TuiCommand,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    match cmd {
        TuiCommand::Pause(params) => {
            let val = serde_json::to_value(params).unwrap_or_else(|_| json!({}));
            send_op_request(req_tx, pending, next_id, "monitor.pause", val, event_tx).await?;
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
        TuiCommand::SnoozeGrace(duration) => {
            let params = match duration {
                Some(d) => json!({ "duration_sec": d }),
                None => json!({}),
            };
            send_op_request(
                req_tx,
                pending,
                next_id,
                "monitor.snooze_grace",
                params,
                event_tx,
            )
            .await?;
        }
        TuiCommand::EnrollStart(timeout) => {
            let params = json!({ "timeout_sec": timeout.unwrap_or(30) });
            let resp =
                send_request(req_tx, pending, next_id, "wearable.enroll_start", params).await?;
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
            let params = json!({ "address": address, "alias": alias });
            send_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.enroll_select",
                params,
                event_tx,
            )
            .await?;
            refresh_wearables(req_tx, pending, next_id, event_tx).await?;
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
            let val = serde_json::to_value(params).unwrap_or_else(|_| json!({}));
            send_request(
                req_tx,
                pending,
                next_id,
                "wearable.enroll_auth_response",
                val,
            )
            .await?;
        }
        TuiCommand::WearableRename { id, alias } => {
            let params = json!({ "id": id, "alias": alias });
            send_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.rename",
                params,
                event_tx,
            )
            .await?;
            refresh_wearables(req_tx, pending, next_id, event_tx).await?;
        }
        TuiCommand::WearableConnect { id } => {
            let params = json!({ "id": id });
            send_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.connect",
                params,
                event_tx,
            )
            .await?;
            refresh_wearables(req_tx, pending, next_id, event_tx).await?;
        }
        TuiCommand::WearableUnenroll {
            id,
            forget_bluez,
            delete_sessions,
        } => {
            let params = json!({
                "id": id,
                "forget_bluez": forget_bluez,
                "delete_sessions": delete_sessions,
            });
            send_op_request(
                req_tx,
                pending,
                next_id,
                "wearable.unenroll",
                params,
                event_tx,
            )
            .await?;
            refresh_wearables(req_tx, pending, next_id, event_tx).await?;
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
            send_request(
                req_tx,
                pending,
                next_id,
                "alert.dismiss",
                json!({ "id": id }),
            )
            .await?;
        }
        TuiCommand::RefreshStatus => {
            refresh_status_event(req_tx, pending, next_id, event_tx, false).await?;
        }
    }
    Ok(())
}

async fn refresh_status_event(
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

async fn refresh_wearables(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<(), std::io::Error> {
    let resp = send_request(req_tx, pending, next_id, "wearable.list", json!({})).await?;
    if let Some(data) = resp.data
        && let Some(list) = data.get("wearables")
        && let Ok(w_list) = serde_json::from_value::<Vec<EnrolledWearableStatus>>(list.clone())
    {
        let _ = event_tx
            .send(TuiEvent::ipc(IpcEvent::WearablesUpdated(w_list)))
            .await;
    }
    Ok(())
}

async fn refresh_calibration(
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
