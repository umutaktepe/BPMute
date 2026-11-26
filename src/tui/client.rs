use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use super::client_commands::{
    execute_command, refresh_calibration, refresh_config, refresh_sessions, refresh_status_event,
    refresh_storage_stats, refresh_wearables,
};
use super::event::{IpcEvent, TuiCommand, TuiEvent};
use crate::ipc::framing::{read_bounded_line, write_ndjson_line};
use crate::ipc::protocol::{
    AlertChangedEvent, CalibrationUpdatedEvent, ConfigSnapshotPayload, DaemonStatus,
    DiscoveredWearable, EnrollmentUpdateEvent, EpochUpdateEvent, GraceTickEvent, HrSampleEvent,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, PushEventEnvelope, RequestEnvelope, ResponseEnvelope,
    SessionClosedEvent, SessionDiscardedEvent, SessionStartedEvent, SnapshotEvent,
    WearablesUpdatedEvent,
};

#[derive(Deserialize)]
struct LegacyEnrollmentDiscovered {
    #[serde(alias = "devices")]
    wearables: Vec<DiscoveredWearable>,
}

pub(super) type PendingMap = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<ResponseEnvelope>>>>>;

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
            let pending: PendingMap = Arc::new(Mutex::new(Some(HashMap::new())));
            let (req_tx, mut req_rx) = mpsc::channel::<RequestEnvelope>(64);

            // Dedicated writer task
            let mut writer_task = tokio::spawn(async move {
                while let Some(envelope) = req_rx.recv().await {
                    if let Err(err) = write_ndjson_line(&mut write_half, &envelope).await {
                        tracing::warn!(error = %err, "TUI IPC writer disconnected");
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
                                            let mut guard = pending_clone.lock().await;
                                            if let Some(map) = guard.as_mut()
                                                && let Some(tx) = map.remove(&resp.id)
                                            {
                                                let _ = tx.send(resp);
                                            }
                                        }
                                    }
                                }
                                Ok(None) => break,
                                Err(err) => {
                                    tracing::warn!(error = %err, "TUI IPC reader disconnected");
                                    break;
                                }
                            }
                        }
                        _ = reader_shutdown.changed() => break,
                    }
                }
                *pending_clone.lock().await = None;
            });

            // Initial setup: subscribe, fetch status, wearables, calibration, sessions, config, stats
            let init_ok = tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut self.shutdown_rx) => false,
                ok = async {
                    let subscribe_res = send_request(
                        &req_tx,
                        &pending,
                        &self.next_id,
                        "daemon.subscribe",
                        json!({}),
                    )
                    .await;
                    if subscribe_res.is_err() {
                        return false;
                    }
                    refresh_status_event(&req_tx, &pending, &self.next_id, &self.event_tx, true)
                        .await
                        .is_ok()
                        && refresh_wearables(&req_tx, &pending, &self.next_id, &self.event_tx)
                            .await
                            .is_ok()
                        && refresh_calibration(&req_tx, &pending, &self.next_id, &self.event_tx)
                            .await
                            .is_ok()
                        && refresh_sessions(&req_tx, &pending, &self.next_id, &self.event_tx, true)
                            .await
                            .is_ok()
                        && refresh_config(&req_tx, &pending, &self.next_id, &self.event_tx)
                            .await
                            .is_ok()
                        && refresh_storage_stats(&req_tx, &pending, &self.next_id, &self.event_tx)
                            .await
                            .is_ok()
                } => ok,
            };

            if !init_ok {
                reader_task.abort();
                writer_task.abort();
                *pending.lock().await = None;
                let _ = self
                    .event_tx
                    .send(TuiEvent::ipc(IpcEvent::Disconnected))
                    .await;
                if *self.shutdown_rx.borrow() {
                    break;
                }
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
            *pending.lock().await = None;

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
            if let Ok(snapshot) = serde_json::from_value::<SnapshotEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::Snapshot(Box::new(snapshot))))
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
        "SessionStarted" => {
            if let Ok(ev) = serde_json::from_value::<SessionStartedEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::SessionStarted(ev)))
                    .await;
            }
        }
        "SessionClosed" => {
            if let Ok(ev) = serde_json::from_value::<SessionClosedEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::SessionClosed(Box::new(ev.session))))
                    .await;
            }
        }
        "SessionDiscarded" => {
            if let Ok(ev) = serde_json::from_value::<SessionDiscardedEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::SessionDiscarded(ev)))
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
            if let Ok(ev) = serde_json::from_value::<AlertChangedEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::AlertChanged(ev.alerts)))
                    .await;
            }
        }
        "CalibrationUpdated" => {
            if let Ok(ev) = serde_json::from_value::<CalibrationUpdatedEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::CalibrationUpdated(ev.calibration)))
                    .await;
            }
        }
        "WearablesUpdated" => {
            if let Ok(ev) = serde_json::from_value::<WearablesUpdatedEvent>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::WearablesUpdated(ev.wearables)))
                    .await;
            }
        }
        "ConfigUpdated" => {
            if let Ok(payload) = serde_json::from_value::<ConfigSnapshotPayload>(push.data) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::ConfigLoaded(payload.config)))
                    .await;
            }
        }
        "EnrollmentUpdate" => {
            if let Ok(update) = serde_json::from_value::<EnrollmentUpdateEvent>(push.data.clone()) {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::EnrollmentUpdate(update)))
                    .await;
            } else if let Ok(legacy) =
                serde_json::from_value::<LegacyEnrollmentDiscovered>(push.data)
            {
                let _ = event_tx
                    .send(TuiEvent::ipc(IpcEvent::EnrollmentDiscovered(
                        legacy.wearables,
                    )))
                    .await;
            }
        }
        _ => {}
    }
}

async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<bool>) {
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            return;
        }
    }
}

pub(super) async fn send_request(
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
        let mut guard = pending.lock().await;
        let Some(map) = guard.as_mut() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "IPC reader task closed",
            ));
        };
        map.insert(req_id, reply_tx);
    }

    if req_tx.send(envelope).await.is_err() {
        let mut guard = pending.lock().await;
        if let Some(map) = guard.as_mut() {
            map.remove(&req_id);
        }
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

pub(super) async fn send_typed_request<P: Serialize>(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    op: &str,
    params: &P,
) -> Result<ResponseEnvelope, std::io::Error> {
    let val = serde_json::to_value(params).unwrap_or_else(|_| json!({}));
    send_request(req_tx, pending, next_id, op, val).await
}

pub(super) async fn send_op_request(
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

pub(super) async fn send_typed_op_request<P: Serialize>(
    req_tx: &mpsc::Sender<RequestEnvelope>,
    pending: &PendingMap,
    next_id: &AtomicU64,
    op: &str,
    params: &P,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Result<ResponseEnvelope, std::io::Error> {
    let val = serde_json::to_value(params).unwrap_or_else(|_| json!({}));
    send_op_request(req_tx, pending, next_id, op, val, event_tx).await
}
