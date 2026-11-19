use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use crate::daemon::state::DaemonCore;

use super::framing::{FrameError, read_bounded_line, write_ndjson_line};
use super::protocol::{
    MAX_FRAME_BYTES, PROTOCOL_VERSION, PushEventEnvelope, RequestEnvelope, ResponseEnvelope,
    error_codes,
};
use super::pubsub::SubscriberQueue;

/// Serves a single multiplexed NDJSON client connection until disconnect, shutdown, or oversized frame.
pub async fn handle_connection(
    stream: UnixStream,
    core: Arc<DaemonCore>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let (resp_tx, mut resp_rx) = mpsc::channel::<ResponseEnvelope>(32);
    let (sub_watch_tx, mut sub_watch_rx) = watch::channel::<Option<Arc<SubscriberQueue>>>(None);

    let writer_task = tokio::spawn(async move {
        loop {
            let active_queue = sub_watch_rx.borrow().clone();
            tokio::select! {
                biased;
                maybe_resp = resp_rx.recv() => {
                    match maybe_resp {
                        Some(resp) => {
                            if write_ndjson_line(&mut write_half, &resp).await.is_err() {
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
                    if write_ndjson_line(&mut write_half, &event).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut active_sub_id: Option<u64> = None;

    loop {
        tokio::select! {
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
            line_res = read_bounded_line(&mut reader, MAX_FRAME_BYTES) => {
                match line_res {
                    Ok(None) => break,
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        process_line(&line, &core, &resp_tx, &sub_watch_tx, &mut active_sub_id).await;
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
        other => {
            let resp = match core.handle_op(other, req.params) {
                Ok(data) => ResponseEnvelope::ok(req.id, data),
                Err(err) => ResponseEnvelope::err(req.id, err.code, err.message),
            };
            let _ = resp_tx.send(resp).await;
        }
    }
}
