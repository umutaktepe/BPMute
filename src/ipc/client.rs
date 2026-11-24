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
    MAX_FRAME_BYTES, MAX_IPC_EPOCHS_PER_FRAME, PROTOCOL_VERSION, PushEventEnvelope,
    RequestEnvelope, ResponseEnvelope, SessionEpochsParams, SessionGetParams,
};
use crate::storage::{SessionEpochsPage, SessionFullDetail};

/// Errors produced by [`IpcClient`].
#[derive(Debug, Error)]
pub enum IpcClientError {
    #[error("daemon is offline or socket unreachable at {path}: {source}")]
    DaemonOffline {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("permission denied accessing daemon socket at {path}: {source}")]
    PermissionDenied {
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
                    io::ErrorKind::PermissionDenied => IpcClientError::PermissionDenied {
                        path: socket_path.display().to_string(),
                        source,
                    },
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

    /// Returns the next push event whose name matches `event_name`, skipping up to `max_skip`
    /// intervening push events (such as high-frequency `HrSample` stream ticks).
    pub async fn next_matching_event(
        &mut self,
        event_name: &str,
        max_skip: usize,
    ) -> Result<Option<PushEventEnvelope>, IpcClientError> {
        for _ in 0..=max_skip {
            let ev = self.next_event().await?;
            if ev.event == event_name {
                return Ok(Some(ev));
            }
        }
        Ok(None)
    }

    /// Fetches full [`SessionFullDetail`] including all `Epochs`, transparently paginating
    /// via `session.epochs` when the session exceeds `MAX_IPC_EPOCHS_PER_FRAME`.
    pub async fn get_session_full(
        &mut self,
        id: &str,
    ) -> Result<SessionFullDetail, IpcClientError> {
        let params = serde_json::to_value(SessionGetParams { id: id.to_string() })?;
        let val = self.request("session.get", params).await?;
        let mut detail: SessionFullDetail = serde_json::from_value(val)?;

        let mut paginator = EpochPaginator::new(&detail);
        while let Some(page_params) = paginator.next_params() {
            let val = serde_json::to_value(page_params)?;
            let page_val = self.request("session.epochs", val).await?;
            let page: SessionEpochsPage = serde_json::from_value(page_val)?;
            if !paginator.push_page(&mut detail, page) {
                break;
            }
        }

        Ok(detail)
    }
}

/// Coordinates paginated `session.epochs` fetches after an initial `session.get` response.
pub(crate) struct EpochPaginator {
    session_id: String,
    next_offset: Option<u32>,
}

impl EpochPaginator {
    pub(crate) fn new(detail: &SessionFullDetail) -> Self {
        let next_offset = (detail.epochs.len() >= MAX_IPC_EPOCHS_PER_FRAME as usize)
            .then_some(detail.epochs.len() as u32);
        Self {
            session_id: detail.session.id.clone(),
            next_offset,
        }
    }

    pub(crate) fn next_params(&self) -> Option<SessionEpochsParams> {
        self.next_offset.map(|offset| SessionEpochsParams {
            id: self.session_id.clone(),
            limit: Some(MAX_IPC_EPOCHS_PER_FRAME),
            offset: Some(offset),
        })
    }

    pub(crate) fn push_page(
        &mut self,
        detail: &mut SessionFullDetail,
        page: SessionEpochsPage,
    ) -> bool {
        if page.epochs.is_empty() {
            self.next_offset = None;
            return false;
        }
        detail.epochs.extend(page.epochs);
        if (detail.epochs.len() as u64) >= page.total_count {
            self.next_offset = None;
            return false;
        }
        self.next_offset = Some(detail.epochs.len() as u32);
        true
    }
}
