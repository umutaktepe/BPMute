use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::framing::{FrameError, read_bounded_line, write_ndjson_line};
use super::protocol::{
    MAX_FRAME_BYTES, PROTOCOL_VERSION, PushEventEnvelope, RequestEnvelope, ResponseEnvelope,
};

/// Errors produced by [`IpcClient`].
#[derive(Debug, Error)]
pub enum IpcClientError {
    #[error("daemon is offline or socket unreachable at {path}: {source}")]
    DaemonOffline {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("IPC server returned error [{code}]: {message}")]
    Server { code: String, message: String },
    #[error("IPC framing error: {0}")]
    Frame(#[from] FrameError),
    #[error("invalid JSON response from daemon: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("daemon closed IPC connection unexpectedly")]
    ConnectionClosed,
}

impl IpcClientError {
    /// Returns the server error code if this error originated from an IPC error response.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Server { code, .. } => Some(code.as_str()),
            _ => None,
        }
    }
}

/// Async multiplexed NDJSON client for communicating with `bpmute daemon`.
pub struct IpcClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: AtomicU64,
    pending_events: VecDeque<PushEventEnvelope>,
}

impl IpcClient {
    /// Connects to the daemon Unix domain socket at `socket_path`.
    pub async fn connect(socket_path: &Path) -> Result<Self, IpcClientError> {
        let stream =
            UnixStream::connect(socket_path)
                .await
                .map_err(|source| match source.kind() {
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
                        IpcClientError::DaemonOffline {
                            path: socket_path.display().to_string(),
                            source,
                        }
                    }
                    _ => IpcClientError::Frame(FrameError::Io(source)),
                })?;

        let (read_half, write_half) = stream.into_split();
        Ok(Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: AtomicU64::new(1),
            pending_events: VecDeque::new(),
        })
    }

    /// Sends a request and waits for its matching response while buffering any interleaved push events.
    pub async fn request(&mut self, op: &str, params: Value) -> Result<Value, IpcClientError> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = RequestEnvelope {
            v: PROTOCOL_VERSION,
            id: req_id,
            op: op.to_string(),
            params,
        };
        write_ndjson_line(&mut self.writer, &envelope).await?;

        loop {
            let Some(line) = read_bounded_line(&mut self.reader, MAX_FRAME_BYTES).await? else {
                return Err(IpcClientError::ConnectionClosed);
            };
            let raw: Value = serde_json::from_str(&line)?;
            if raw.get("event").is_some() {
                let event: PushEventEnvelope = serde_json::from_value(raw)?;
                self.pending_events.push_back(event);
                continue;
            }

            let resp: ResponseEnvelope = serde_json::from_value(raw)?;
            if resp.id != req_id {
                continue;
            }
            if resp.ok {
                return Ok(resp.data.unwrap_or_else(|| json!({})));
            }
            let err = resp.error.unwrap_or(super::protocol::ErrorPayload {
                code: "INTERNAL_ERROR".to_string(),
                message: "daemon returned failure without error details".to_string(),
            });
            return Err(IpcClientError::Server {
                code: err.code,
                message: err.message,
            });
        }
    }

    /// Returns the next buffered or incoming push event from a subscribed stream.
    pub async fn next_event(&mut self) -> Result<PushEventEnvelope, IpcClientError> {
        if let Some(ev) = self.pending_events.pop_front() {
            return Ok(ev);
        }

        loop {
            let Some(line) = read_bounded_line(&mut self.reader, MAX_FRAME_BYTES).await? else {
                return Err(IpcClientError::ConnectionClosed);
            };
            let raw: Value = serde_json::from_str(&line)?;
            if raw.get("event").is_some() {
                return Ok(serde_json::from_value(raw)?);
            }
        }
    }
}
