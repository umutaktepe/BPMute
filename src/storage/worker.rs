//! Dedicated OS-thread persistence worker owning SQLite WAL I/O.

use std::path::Path;
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};

use crate::calibration::CalibrationState;
use crate::sleep_engine::{Epoch, HrPacket};
use crate::storage::{
    AcceptShiftResult, CloseSessionRequest, EnrolledWearable, LedgerStore, OpenSessionRequest,
    SessionFullDetail, SessionListPage, SetFeedbackRequest, StorageError, WearableDescriptor,
};

enum DbCommand {
    Shutdown,
    UpsertWearable {
        desc: WearableDescriptor,
        now_ms: i64,
        reply: oneshot::Sender<Result<String, StorageError>>,
    },
    ListWearables {
        reply: oneshot::Sender<Result<Vec<EnrolledWearable>, StorageError>>,
    },
    RenameWearable {
        id_or_prefix: String,
        new_alias: Option<String>,
        reply: oneshot::Sender<Result<EnrolledWearable, StorageError>>,
    },
    UnenrollWearable {
        id_or_prefix: String,
        delete_sessions: bool,
        reply: oneshot::Sender<Result<EnrolledWearable, StorageError>>,
    },
    OpenSession {
        req: OpenSessionRequest,
        reply: oneshot::Sender<Result<String, StorageError>>,
    },
    AppendEpochAndRaw {
        session_id: String,
        epoch: Option<(u32, Epoch)>,
        raw_samples: Vec<(u32, HrPacket)>,
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
    CloseSessionAndRecompute {
        req: CloseSessionRequest,
        now_ms: i64,
        reply: oneshot::Sender<Result<(SessionFullDetail, CalibrationState), StorageError>>,
    },
    ListSessions {
        limit: u32,
        offset: u32,
        reply: oneshot::Sender<Result<SessionListPage, StorageError>>,
    },
    GetSession {
        id_or_prefix: String,
        reply: oneshot::Sender<Result<SessionFullDetail, StorageError>>,
    },
    SetFeedbackAndRecompute {
        req: SetFeedbackRequest,
        now_ms: i64,
        reply: oneshot::Sender<Result<(SessionFullDetail, CalibrationState), StorageError>>,
    },
    SetDay1SeedAndRecompute {
        sleep_hr: Option<f64>,
        now_ms: i64,
        reply: oneshot::Sender<Result<CalibrationState, StorageError>>,
    },
    AcceptShiftAndRecompute {
        now_ms: i64,
        reply: oneshot::Sender<Result<AcceptShiftResult, StorageError>>,
    },
    DismissShiftAndRecompute {
        now_ms: i64,
        reply: oneshot::Sender<Result<CalibrationState, StorageError>>,
    },
    FlushWal {
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
}

/// Clonable async handle for dispatching SQLite operations to [`PersistenceWorker`].
#[derive(Clone)]
pub struct PersistenceHandle {
    tx: mpsc::UnboundedSender<DbCommand>,
}

/// Dedicated background thread owning [`LedgerStore`].
pub struct PersistenceWorker {
    tx: Option<mpsc::UnboundedSender<DbCommand>>,
    join: Option<JoinHandle<()>>,
}

impl PersistenceWorker {
    /// Spawns the dedicated persistence worker thread, opens the SQLite WAL ledger,
    /// recovers any stale `'active'` sessions, and returns the initial [`CalibrationState`].
    pub fn spawn(
        db_path: &Path,
        lock_path: &Path,
        now_ms: i64,
    ) -> Result<(Self, PersistenceHandle, CalibrationState), StorageError> {
        let db = db_path.to_path_buf();
        let lock = lock_path.to_path_buf();
        let (tx, mut rx) = mpsc::unbounded_channel::<DbCommand>();
        let (init_tx, init_rx) =
            std::sync::mpsc::channel::<Result<CalibrationState, StorageError>>();

        let join = std::thread::Builder::new()
            .name("bpmute-ledger".to_string())
            .spawn(move || {
                let mut store = match LedgerStore::open_read_write(&db, &lock) {
                    Ok(s) => s,
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                        return;
                    }
                };
                if let Err(err) = store.recover_stale_sessions(now_ms) {
                    let _ = init_tx.send(Err(err));
                    return;
                }
                let initial_state = match store.get_calibration_state() {
                    Ok(st) => st,
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                        return;
                    }
                };
                if init_tx.send(Ok(initial_state)).is_err() {
                    return;
                }

                while let Some(cmd) = rx.blocking_recv() {
                    if matches!(cmd, DbCommand::Shutdown) {
                        break;
                    }
                    dispatch_command(&mut store, cmd);
                }
            })?;

        let initial_state = init_rx.recv().map_err(|_| StorageError::WorkerStopped)??;
        let handle = PersistenceHandle { tx: tx.clone() };
        Ok((
            Self {
                tx: Some(tx),
                join: Some(join),
            },
            handle,
            initial_state,
        ))
    }

    /// Signals shutdown and waits asynchronously on a blocking pool thread for exit.
    pub async fn shutdown_async(mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(DbCommand::Shutdown);
        }
        if let Some(j) = self.join.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _ = j.join();
            })
            .await;
        }
    }
}

impl Drop for PersistenceWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(DbCommand::Shutdown);
        }
    }
}

fn dispatch_command(store: &mut LedgerStore, cmd: DbCommand) {
    match cmd {
        DbCommand::Shutdown => {}
        DbCommand::UpsertWearable {
            desc,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.upsert_wearable(&desc, now_ms));
        }
        DbCommand::ListWearables { reply } => {
            let _ = reply.send(store.list_enrolled_wearables());
        }
        DbCommand::RenameWearable {
            id_or_prefix,
            new_alias,
            reply,
        } => {
            let _ = reply.send(store.rename_wearable(&id_or_prefix, new_alias.as_deref()));
        }
        DbCommand::UnenrollWearable {
            id_or_prefix,
            delete_sessions,
            reply,
        } => {
            let _ = reply.send(store.unenroll_wearable(&id_or_prefix, delete_sessions));
        }
        DbCommand::OpenSession { req, reply } => {
            let _ = reply.send(store.open_session(&req));
        }
        DbCommand::AppendEpochAndRaw {
            session_id,
            epoch,
            raw_samples,
            reply,
        } => {
            let ep_ref = epoch.as_ref().map(|(seq, ep)| (*seq, ep));
            let raw_refs: Vec<(u32, &HrPacket)> =
                raw_samples.iter().map(|(seq, pkt)| (*seq, pkt)).collect();
            let _ = reply.send(store.append_epoch_and_raw(&session_id, ep_ref, &raw_refs));
        }
        DbCommand::CloseSessionAndRecompute { req, now_ms, reply } => {
            let _ = reply.send(store.close_session_and_recompute(&req, now_ms));
        }
        DbCommand::ListSessions {
            limit,
            offset,
            reply,
        } => {
            let _ = reply.send(store.list_sessions(limit, offset));
        }
        DbCommand::GetSession {
            id_or_prefix,
            reply,
        } => {
            let _ = reply.send(store.get_session(&id_or_prefix));
        }
        DbCommand::SetFeedbackAndRecompute { req, now_ms, reply } => {
            let _ = reply.send(store.set_feedback_and_recompute(&req, now_ms));
        }
        DbCommand::SetDay1SeedAndRecompute {
            sleep_hr,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.set_day1_seed_and_recompute(sleep_hr, now_ms));
        }
        DbCommand::AcceptShiftAndRecompute { now_ms, reply } => {
            let _ = reply.send(store.accept_shift_and_recompute(now_ms));
        }
        DbCommand::DismissShiftAndRecompute { now_ms, reply } => {
            let _ = reply.send(store.dismiss_shift_and_recompute(now_ms));
        }
        DbCommand::FlushWal { reply } => {
            let _ = reply.send(store.flush_wal());
        }
    }
}

impl PersistenceHandle {
    pub async fn flush_wal(&self) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::FlushWal { reply }).await
    }
    async fn call<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, StorageError>>) -> DbCommand,
    ) -> Result<T, StorageError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(build(reply_tx))
            .map_err(|_| StorageError::WorkerStopped)?;
        reply_rx.await.map_err(|_| StorageError::WorkerStopped)?
    }

    pub async fn upsert_wearable(
        &self,
        desc: WearableDescriptor,
        now_ms: i64,
    ) -> Result<String, StorageError> {
        self.call(|reply| DbCommand::UpsertWearable {
            desc,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn list_wearables(&self) -> Result<Vec<EnrolledWearable>, StorageError> {
        self.call(|reply| DbCommand::ListWearables { reply }).await
    }

    pub async fn rename_wearable(
        &self,
        id_or_prefix: String,
        new_alias: Option<String>,
    ) -> Result<EnrolledWearable, StorageError> {
        self.call(|reply| DbCommand::RenameWearable {
            id_or_prefix,
            new_alias,
            reply,
        })
        .await
    }

    pub async fn unenroll_wearable(
        &self,
        id_or_prefix: String,
        delete_sessions: bool,
    ) -> Result<EnrolledWearable, StorageError> {
        self.call(|reply| DbCommand::UnenrollWearable {
            id_or_prefix,
            delete_sessions,
            reply,
        })
        .await
    }

    pub async fn open_session(&self, req: OpenSessionRequest) -> Result<String, StorageError> {
        self.call(|reply| DbCommand::OpenSession { req, reply })
            .await
    }

    pub async fn append_epoch_and_raw(
        &self,
        session_id: String,
        epoch: Option<(u32, Epoch)>,
        raw_samples: Vec<(u32, HrPacket)>,
    ) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::AppendEpochAndRaw {
            session_id,
            epoch,
            raw_samples,
            reply,
        })
        .await
    }

    pub async fn close_session_and_recompute(
        &self,
        req: CloseSessionRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::CloseSessionAndRecompute { req, now_ms, reply })
            .await
    }

    pub async fn list_sessions(
        &self,
        limit: u32,
        offset: u32,
    ) -> Result<SessionListPage, StorageError> {
        self.call(|reply| DbCommand::ListSessions {
            limit,
            offset,
            reply,
        })
        .await
    }

    pub async fn get_session(
        &self,
        id_or_prefix: String,
    ) -> Result<SessionFullDetail, StorageError> {
        self.call(|reply| DbCommand::GetSession {
            id_or_prefix,
            reply,
        })
        .await
    }

    pub async fn set_feedback_and_recompute(
        &self,
        req: SetFeedbackRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::SetFeedbackAndRecompute { req, now_ms, reply })
            .await
    }

    pub async fn set_day1_seed_and_recompute(
        &self,
        sleep_hr: Option<f64>,
        now_ms: i64,
    ) -> Result<CalibrationState, StorageError> {
        self.call(|reply| DbCommand::SetDay1SeedAndRecompute {
            sleep_hr,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn accept_shift_and_recompute(
        &self,
        now_ms: i64,
    ) -> Result<AcceptShiftResult, StorageError> {
        self.call(|reply| DbCommand::AcceptShiftAndRecompute { now_ms, reply })
            .await
    }

    pub async fn dismiss_shift_and_recompute(
        &self,
        now_ms: i64,
    ) -> Result<CalibrationState, StorageError> {
        self.call(|reply| DbCommand::DismissShiftAndRecompute { now_ms, reply })
            .await
    }
}
