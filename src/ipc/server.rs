use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use crate::daemon::state::DaemonCore;

use super::framing::{FrameError, read_bounded_line, write_bounded_ndjson_line};
use super::protocol::{
    MAX_FRAME_BYTES, PROTOCOL_VERSION, PushEventEnvelope, RequestEnvelope, ResponseEnvelope,
    error_codes,
};
use super::pubsub::SubscriberQueue;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Serves a single multiplexed NDJSON client connection until disconnect, shutdown, or oversized frame.
pub async fn handle_connection(
    stream: UnixStream,
    core: Arc<DaemonCore>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let (resp_tx, mut resp_rx) = mpsc::channel::<ResponseEnvelope>(32);
    let (sub_watch_tx, mut sub_watch_rx) = watch::channel::<Option<Arc<SubscriberQueue>>>(None);
    let mut writer_shutdown_rx = shutdown_rx.clone();

    let writer_task = tokio::spawn(async move {
        loop {
            if *writer_shutdown_rx.borrow() {
                break;
            }
            let active_queue = sub_watch_rx.borrow().clone();
            tokio::select! {
                biased;
                _ = wait_for_shutdown(&mut writer_shutdown_rx) => break,
                maybe_resp = resp_rx.recv() => {
                    match maybe_resp {
                        Some(resp) => {
                            let ok = tokio::select! {
                                biased;
                                _ = wait_for_shutdown(&mut writer_shutdown_rx) => false,
                                res = write_server_response(&mut write_half, &resp, client_id) => res,
                            };
                            if !ok {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                changed = sub_watch_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                event = async {
                    match active_queue {
                        Some(ref q) => q.next_event().await,
                        None => std::future::pending::<PushEventEnvelope>().await,
                    }
                } => {
                    let ok = tokio::select! {
                        biased;
                        _ = wait_for_shutdown(&mut writer_shutdown_rx) => false,
                        res = write_server_push(&mut write_half, &event, client_id) => res,
                    };
                    if !ok {
                        break;
                    }
                }
            }
        }
    });

    let mut active_sub_id: Option<u64> = None;
    let mut op_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            Some(_) = op_tasks.join_next(), if !op_tasks.is_empty() => {}
            line_res = read_bounded_line(&mut reader, MAX_FRAME_BYTES) => {
                match line_res {
                    Ok(None) => break,
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        process_line(
                            &line,
                            &core,
                            &resp_tx,
                            &sub_watch_tx,
                            &mut active_sub_id,
                            client_id,
                            &mut op_tasks,
                        )
                        .await;
                    }
                    Err(FrameError::TooLarge { max_bytes }) => {
                        let _ = resp_tx
                            .send(ResponseEnvelope::err(
                                0,
                                error_codes::FRAME_TOO_LARGE,
                                format!("NDJSON frame exceeds maximum size of {max_bytes} bytes"),
                            ))
                            .await;
                        break;
                    }
                    Err(FrameError::InvalidUtf8) => {
                        let _ = resp_tx
                            .send(ResponseEnvelope::err(
                                0,
                                error_codes::INVALID_JSON,
                                "frame is not valid UTF-8",
                            ))
                            .await;
                    }
                    Err(FrameError::Io(_)) | Err(FrameError::Serialize(_)) => break,
                }
            }
        }
    }

    if let Some(sub_id) = active_sub_id.take() {
        core.broker.unsubscribe(sub_id);
    }
    core.on_client_disconnected(client_id);
    op_tasks.abort_all();
    while op_tasks.join_next().await.is_some() {}
    let _ = sub_watch_tx.send(None);
    drop(resp_tx);
    let _ = writer_task.await;
}

async fn process_line(
    line: &str,
    core: &Arc<DaemonCore>,
    resp_tx: &mpsc::Sender<ResponseEnvelope>,
    sub_watch_tx: &watch::Sender<Option<Arc<SubscriberQueue>>>,
    active_sub_id: &mut Option<u64>,
    client_id: u64,
    op_tasks: &mut tokio::task::JoinSet<()>,
) {
    let raw: Value = match serde_json::from_str(line) {
        Ok(val) => val,
        Err(err) => {
            let _ = resp_tx
                .send(ResponseEnvelope::err(
                    0,
                    error_codes::INVALID_JSON,
                    format!("malformed JSON: {err}"),
                ))
                .await;
            return;
        }
    };

    let req_id = raw.get("id").and_then(Value::as_u64).unwrap_or(0);
    let version = raw.get("v").and_then(Value::as_u64);
    if version != Some(u64::from(PROTOCOL_VERSION)) {
        let _ = resp_tx
            .send(ResponseEnvelope::err(
                req_id,
                error_codes::UNSUPPORTED_VERSION,
                format!("unsupported protocol version {version:?}; expected {PROTOCOL_VERSION}"),
            ))
            .await;
        return;
    }

    let req: RequestEnvelope = match serde_json::from_value(raw) {
        Ok(req) => req,
        Err(err) => {
            let _ = resp_tx
                .send(ResponseEnvelope::err(
                    req_id,
                    error_codes::INVALID_PARAMS,
                    format!("invalid request envelope: {err}"),
                ))
                .await;
            return;
        }
    };

    match req.op.as_str() {
        "daemon.subscribe" => {
            if let Some(old_id) = active_sub_id.take() {
                core.broker.unsubscribe(old_id);
            }
            core.refresh_playing_players_count().await;
            let (sub_id, queue, status) = core.subscribe();
            *active_sub_id = Some(sub_id);

            if resp_tx
                .send(ResponseEnvelope::ok(req.id, json!({ "subscribed": true })))
                .await
                .is_err()
            {
                core.broker.unsubscribe(sub_id);
                *active_sub_id = None;
                return;
            }

            let snapshot = core.make_snapshot_event(&status);
            queue.push_control(snapshot);
            let _ = sub_watch_tx.send(Some(queue));
        }
        "daemon.unsubscribe" => {
            if let Some(old_id) = active_sub_id.take() {
                core.broker.unsubscribe(old_id);
            }
            let _ = sub_watch_tx.send(None);
            let _ = resp_tx
                .send(ResponseEnvelope::ok(req.id, json!({ "subscribed": false })))
                .await;
        }
        op => {
            let core_cloned = Arc::clone(core);
            let resp_tx_cloned = resp_tx.clone();
            let op_owned = op.to_string();
            let req_id = req.id;
            let params = req.params;
            op_tasks.spawn(async move {
                let resp = match core_cloned.handle_op(&op_owned, params, client_id).await {
                    Ok(data) => ResponseEnvelope::ok(req_id, data),
                    Err(err) => ResponseEnvelope::err(req_id, err.code, err.message),
                };
                let _ = resp_tx_cloned.send(resp).await;
            });
        }
    }
}

async fn wait_for_shutdown(shutdown_rx: &mut watch::Receiver<bool>) {
    while !*shutdown_rx.borrow() {
        if shutdown_rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

async fn write_server_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &ResponseEnvelope,
    client_id: u64,
) -> bool {
    match write_bounded_ndjson_line(writer, resp, MAX_FRAME_BYTES).await {
        Ok(()) => true,
        Err(FrameError::TooLarge { max_bytes }) => {
            tracing::error!(
                client_id,
                req_id = resp.id,
                max_bytes,
                "IPC response exceeded MAX_FRAME_BYTES; sending FRAME_TOO_LARGE error"
            );
            let fallback = ResponseEnvelope::err(
                resp.id,
                error_codes::FRAME_TOO_LARGE,
                format!("IPC response exceeds maximum frame size of {max_bytes} bytes"),
            );
            write_bounded_ndjson_line(writer, &fallback, MAX_FRAME_BYTES)
                .await
                .is_ok()
        }
        Err(_) => false,
    }
}

async fn write_server_push<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    event: &PushEventEnvelope,
    client_id: u64,
) -> bool {
    match write_bounded_ndjson_line(writer, event, MAX_FRAME_BYTES).await {
        Ok(()) => true,
        Err(FrameError::TooLarge { max_bytes }) => {
            tracing::error!(
                client_id,
                event = %event.event,
                seq = event.seq,
                max_bytes,
                "IPC push event exceeded MAX_FRAME_BYTES; dropping oversized push event"
            );
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;

    #[tokio::test]
    async fn write_server_response_emits_frame_too_large_and_keeps_connection_alive() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut buf_reader = BufReader::new(reader);

        let huge = ResponseEnvelope::ok(7, json!({ "pad": "x".repeat(MAX_FRAME_BYTES + 128) }));
        assert!(write_server_response(&mut writer, &huge, 1).await);

        let next = ResponseEnvelope::ok(8, json!({ "pong": true }));
        assert!(write_server_response(&mut writer, &next, 1).await);
        drop(writer);

        let mut line1 = String::new();
        buf_reader.read_line(&mut line1).await.unwrap();
        let r1: ResponseEnvelope = serde_json::from_str(&line1).unwrap();
        assert_eq!(r1.id, 7);
        assert!(!r1.ok);
        assert_eq!(r1.error.unwrap().code, error_codes::FRAME_TOO_LARGE);

        let mut line2 = String::new();
        buf_reader.read_line(&mut line2).await.unwrap();
        let r2: ResponseEnvelope = serde_json::from_str(&line2).unwrap();
        assert_eq!(r2.id, 8);
        assert!(r2.ok);
        assert_eq!(r2.data.unwrap()["pong"], true);
    }

    #[tokio::test]
    async fn write_server_push_drops_oversized_event_without_poisoning_subscriber_stream() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let mut buf_reader = BufReader::new(reader);

        let huge_push = PushEventEnvelope {
            v: PROTOCOL_VERSION,
            event: "OversizedTest".to_string(),
            seq: 1,
            data: json!({ "pad": "x".repeat(MAX_FRAME_BYTES + 128) }),
        };
        assert!(write_server_push(&mut writer, &huge_push, 1).await);

        let normal_push = PushEventEnvelope {
            v: PROTOCOL_VERSION,
            event: "StateChanged".to_string(),
            seq: 2,
            data: json!({ "ok": true }),
        };
        assert!(write_server_push(&mut writer, &normal_push, 1).await);
        drop(writer);

        let mut line = String::new();
        buf_reader.read_line(&mut line).await.unwrap();
        let received: PushEventEnvelope = serde_json::from_str(&line).unwrap();
        assert_eq!(received.event, "StateChanged");
        assert_eq!(received.seq, 2);

        line.clear();
        assert_eq!(buf_reader.read_line(&mut line).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn handle_connection_exits_promptly_on_shutdown_with_full_subscriber_socket() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        use crate::actions::{FakeActionExecutor, FakeNotifier};
        use crate::config::{CliOverrides, LoadedConfig};
        use crate::daemon::state::{DaemonCoreInit, NoopEnrollmentDriver};
        use crate::daemon::{NullWearableSource, SystemClock};
        use crate::idle::FakeIdleProbe;
        use crate::ipc::pubsub::EventBroker;
        use crate::paths::{MapEnv, ResolvedPaths};
        use crate::storage::{PersistenceWorker, db_path_in_data_dir, lock_path_in_data_dir};

        let temp = tempfile::tempdir().unwrap();
        let paths = ResolvedPaths {
            config_file: temp.path().join("cfg/config.toml"),
            data_dir: temp.path().join("data"),
            socket_path: temp.path().join("rt/bpmute.sock"),
        };
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let env = Arc::new(MapEnv::new());
        let loaded =
            LoadedConfig::load(&paths.config_file, env.as_ref(), &CliOverrides::default()).unwrap();
        let (worker, persistence, initial_calibration) = PersistenceWorker::spawn(
            &db_path_in_data_dir(&paths.data_dir),
            &lock_path_in_data_dir(&paths.data_dir),
            1_000,
        )
        .unwrap();
        let (ctrl_tx, _ctrl_rx) = mpsc::channel(8);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let core = Arc::new(DaemonCore::new(DaemonCoreInit {
            paths,
            loaded_config: loaded,
            env,
            cli_overrides: CliOverrides::default(),
            clock: Arc::new(SystemClock),
            broker: Arc::new(EventBroker::default()),
            shutdown_rx: shutdown_rx.clone(),
            persistence,
            initial_calibration,
            action_executor: Arc::new(FakeActionExecutor::new()),
            notifier: Arc::new(FakeNotifier::new()),
            idle_probe: Arc::new(FakeIdleProbe::default()),
            enrollment_driver: Arc::new(NoopEnrollmentDriver),
            wearable_source: Arc::new(NullWearableSource),
            engine_ctrl_tx: ctrl_tx,
        }));

        let (server_stream, client_stream) = UnixStream::pair().unwrap();
        let handle_task = tokio::spawn(handle_connection(
            server_stream,
            Arc::clone(&core),
            shutdown_rx,
        ));

        let (client_read, mut client_write) = client_stream.into_split();
        let mut client_buf = BufReader::new(client_read);
        client_write
            .write_all(b"{\"id\":1,\"v\":1,\"op\":\"daemon.subscribe\",\"params\":{}}\n")
            .await
            .unwrap();
        let mut ack = String::new();
        client_buf.read_line(&mut ack).await.unwrap();
        assert!(
            ack.contains("\"ok\":true"),
            "unexpected subscribe ack: {ack}"
        );

        // Fill subscriber queue and kernel socket send buffer without reading from client_buf.
        let payload = json!({ "padding": "x".repeat(4096) });
        for _ in 0..512 {
            core.broker.publish_control("AlertChanged", payload.clone());
        }
        tokio::task::yield_now().await;

        shutdown_tx.send(true).unwrap();
        handle_task.await.unwrap();

        drop(client_buf);
        drop(client_write);
        drop(core);
        worker.shutdown_async().await;
    }
}
