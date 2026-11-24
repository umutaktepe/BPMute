//! Dedicated OS-thread persistence worker owning SQLite WAL I/O.

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use tokio::sync::{mpsc, oneshot};

use crate::calibration::{CalibrationBoundaryRecord, CalibrationState};
use crate::config::{PowerAction, RetentionConfig};
use crate::replay::TraceFormat;
use crate::sleep_engine::{Epoch, HrPacket};
use crate::storage::{
    AcceptShiftResult, CloseSessionRequest, EnrolledWearable, ExportBundleOptions, ImportMode,
    ImportSummary, LedgerStore, OpenSessionRequest, PruneSummary, SessionEpochsPage,
    SessionFullDetail, SessionListPage, SetFeedbackRequest, StorageError, StorageStats,
    WearableDescriptor,
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
        now_ms: i64,
        reply: oneshot::Sender<Result<(EnrolledWearable, Option<CalibrationState>), StorageError>>,
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
        include_epochs: bool,
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
    GetSessionWithEpochLimit {
        id_or_prefix: String,
        max_epochs: u32,
        reply: oneshot::Sender<Result<SessionFullDetail, StorageError>>,
    },
    GetSessionEpochs {
        id_or_prefix: String,
        limit: u32,
        offset: u32,
        reply: oneshot::Sender<Result<SessionEpochsPage, StorageError>>,
    },
    DeleteSessionAndRecompute {
        id_or_prefix: String,
        now_ms: i64,
        reply: oneshot::Sender<Result<(String, CalibrationState), StorageError>>,
    },
    ExportTrace {
        id_or_prefix: String,
        dest_path: PathBuf,
        format: TraceFormat,
        anonymize: bool,
        reply: oneshot::Sender<Result<usize, StorageError>>,
    },
    SetFeedbackAndRecompute {
        req: SetFeedbackRequest,
        now_ms: i64,
        include_epochs: bool,
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
    SetCalibrationBoundaryAndRecompute {
        effective_ts_ms: Option<i64>,
        seed_sleep_hr: Option<f64>,
        reset_floors: bool,
        now_ms: i64,
        reply: oneshot::Sender<Result<(CalibrationBoundaryRecord, CalibrationState), StorageError>>,
    },
    UndoCalibrationBoundaryAndRecompute {
        now_ms: i64,
        reply: oneshot::Sender<Result<(CalibrationBoundaryRecord, CalibrationState), StorageError>>,
    },
    PurgeRawSamples {
        reply: oneshot::Sender<Result<usize, StorageError>>,
    },
    HardWipe {
        wipe_all: bool,
        now_ms: i64,
        reply: oneshot::Sender<Result<CalibrationState, StorageError>>,
    },
    BackupSqlite {
        dest_path: PathBuf,
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
    GetStorageStats {
        reply: oneshot::Sender<Result<StorageStats, StorageError>>,
    },
    ListCalibrationBoundaries {
        reply: oneshot::Sender<Result<Vec<CalibrationBoundaryRecord>, StorageError>>,
    },
    ExportBundle {
        dest_path: PathBuf,
        options: ExportBundleOptions,
        now_ms: i64,
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
    ImportBundle {
        src_path: PathBuf,
        mode: ImportMode,
        force: bool,
        now_ms: i64,
        reply: oneshot::Sender<Result<(ImportSummary, CalibrationState), StorageError>>,
    },
    SetRetentionConfig {
        config: RetentionConfig,
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
    PruneRetention {
        now_ms: i64,
        reply: oneshot::Sender<Result<PruneSummary, StorageError>>,
    },
    FlushWal {
        reply: oneshot::Sender<Result<(), StorageError>>,
    },
    MarkSessionSleepTrigger {
        session_id: String,
        sleeping_ts_ms: i64,
        trigger_power_action: PowerAction,
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
                if let Err(err) = store.prune_retention(now_ms) {
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
                let _ = store.flush_wal();
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

    /// Signals shutdown and blocks synchronously until the worker thread exits.
    pub fn shutdown_and_join(mut self) -> Result<(), StorageError> {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(DbCommand::Shutdown);
        }
        if let Some(j) = self.join.take() {
            j.join().map_err(|_| StorageError::WorkerStopped)?;
        }
        Ok(())
    }
}

impl Drop for PersistenceWorker {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(DbCommand::Shutdown);
        }
        if let Some(j) = self.join.take() {
            let _ = j.join();
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
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.unenroll_wearable(&id_or_prefix, delete_sessions, now_ms));
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
        DbCommand::CloseSessionAndRecompute {
            req,
            now_ms,
            include_epochs,
            reply,
        } => {
            let res = if include_epochs {
                store.close_session_and_recompute(&req, now_ms)
            } else {
                store.close_session_slim_and_recompute(&req, now_ms)
            };
            let _ = reply.send(res);
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
        DbCommand::GetSessionWithEpochLimit {
            id_or_prefix,
            max_epochs,
            reply,
        } => {
            let _ = reply.send(store.get_session_with_epoch_limit(&id_or_prefix, max_epochs));
        }
        DbCommand::GetSessionEpochs {
            id_or_prefix,
            limit,
            offset,
            reply,
        } => {
            let _ = reply.send(store.get_session_epochs(&id_or_prefix, limit, offset));
        }
        DbCommand::SetFeedbackAndRecompute {
            req,
            now_ms,
            include_epochs,
            reply,
        } => {
            let res = if include_epochs {
                store.set_feedback_and_recompute(&req, now_ms)
            } else {
                store.set_feedback_slim_and_recompute(&req, now_ms)
            };
            let _ = reply.send(res);
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
        DbCommand::DeleteSessionAndRecompute {
            id_or_prefix,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.delete_session_and_recompute(&id_or_prefix, now_ms));
        }
        DbCommand::ExportTrace {
            id_or_prefix,
            dest_path,
            format,
            anonymize,
            reply,
        } => {
            let _ = reply.send(store.export_trace(&id_or_prefix, &dest_path, format, anonymize));
        }
        DbCommand::SetCalibrationBoundaryAndRecompute {
            effective_ts_ms,
            seed_sleep_hr,
            reset_floors,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.set_calibration_boundary(
                effective_ts_ms,
                seed_sleep_hr,
                reset_floors,
                now_ms,
            ));
        }
        DbCommand::UndoCalibrationBoundaryAndRecompute { now_ms, reply } => {
            let _ = reply.send(store.undo_calibration_boundary(now_ms));
        }
        DbCommand::PurgeRawSamples { reply } => {
            let _ = reply.send(store.purge_raw_samples());
        }
        DbCommand::HardWipe {
            wipe_all,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.hard_wipe(wipe_all, now_ms));
        }
        DbCommand::BackupSqlite { dest_path, reply } => {
            let _ = reply.send(store.backup_sqlite(&dest_path));
        }
        DbCommand::GetStorageStats { reply } => {
            let _ = reply.send(store.get_storage_stats());
        }
        DbCommand::ListCalibrationBoundaries { reply } => {
            let _ = reply.send(store.list_calibration_boundaries());
        }
        DbCommand::ExportBundle {
            dest_path,
            options,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.export_bundle(&dest_path, options, now_ms));
        }
        DbCommand::ImportBundle {
            src_path,
            mode,
            force,
            now_ms,
            reply,
        } => {
            let _ = reply.send(store.import_bundle(&src_path, mode, force, now_ms));
        }
        DbCommand::SetRetentionConfig { config, reply } => {
            store.set_retention_config(config);
            let _ = reply.send(Ok(()));
        }
        DbCommand::PruneRetention { now_ms, reply } => {
            let _ = reply.send(store.prune_retention(now_ms));
        }
        DbCommand::FlushWal { reply } => {
            let _ = reply.send(store.flush_wal());
        }
        DbCommand::MarkSessionSleepTrigger {
            session_id,
            sleeping_ts_ms,
            trigger_power_action,
            reply,
        } => {
            let _ = reply.send(store.mark_session_sleep_trigger(
                &session_id,
                sleeping_ts_ms,
                trigger_power_action,
            ));
        }
    }
}

impl PersistenceHandle {
    pub async fn flush_wal(&self) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::FlushWal { reply }).await
    }

    pub async fn mark_session_sleep_trigger(
        &self,
        session_id: String,
        sleeping_ts_ms: i64,
        trigger_power_action: PowerAction,
    ) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::MarkSessionSleepTrigger {
            session_id,
            sleeping_ts_ms,
            trigger_power_action,
            reply,
        })
        .await
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
        now_ms: i64,
    ) -> Result<(EnrolledWearable, Option<CalibrationState>), StorageError> {
        self.call(|reply| DbCommand::UnenrollWearable {
            id_or_prefix,
            delete_sessions,
            now_ms,
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
        self.call(|reply| DbCommand::CloseSessionAndRecompute {
            req,
            now_ms,
            include_epochs: true,
            reply,
        })
        .await
    }

    pub(crate) async fn close_session_slim_and_recompute(
        &self,
        req: CloseSessionRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::CloseSessionAndRecompute {
            req,
            now_ms,
            include_epochs: false,
            reply,
        })
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

    pub async fn get_session_with_epoch_limit(
        &self,
        id_or_prefix: String,
        max_epochs: u32,
    ) -> Result<SessionFullDetail, StorageError> {
        self.call(|reply| DbCommand::GetSessionWithEpochLimit {
            id_or_prefix,
            max_epochs,
            reply,
        })
        .await
    }

    pub async fn get_session_epochs(
        &self,
        id_or_prefix: String,
        limit: u32,
        offset: u32,
    ) -> Result<SessionEpochsPage, StorageError> {
        self.call(|reply| DbCommand::GetSessionEpochs {
            id_or_prefix,
            limit,
            offset,
            reply,
        })
        .await
    }

    pub async fn set_feedback_and_recompute(
        &self,
        req: SetFeedbackRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::SetFeedbackAndRecompute {
            req,
            now_ms,
            include_epochs: true,
            reply,
        })
        .await
    }

    pub(crate) async fn set_feedback_slim_and_recompute(
        &self,
        req: SetFeedbackRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::SetFeedbackAndRecompute {
            req,
            now_ms,
            include_epochs: false,
            reply,
        })
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

    pub async fn delete_session_and_recompute(
        &self,
        id_or_prefix: String,
        now_ms: i64,
    ) -> Result<(String, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::DeleteSessionAndRecompute {
            id_or_prefix,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn export_trace(
        &self,
        id_or_prefix: String,
        dest_path: PathBuf,
        format: TraceFormat,
        anonymize: bool,
    ) -> Result<usize, StorageError> {
        self.call(|reply| DbCommand::ExportTrace {
            id_or_prefix,
            dest_path,
            format,
            anonymize,
            reply,
        })
        .await
    }

    pub async fn set_calibration_boundary_and_recompute(
        &self,
        effective_ts_ms: Option<i64>,
        seed_sleep_hr: Option<f64>,
        reset_floors: bool,
        now_ms: i64,
    ) -> Result<(CalibrationBoundaryRecord, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::SetCalibrationBoundaryAndRecompute {
            effective_ts_ms,
            seed_sleep_hr,
            reset_floors,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn undo_calibration_boundary_and_recompute(
        &self,
        now_ms: i64,
    ) -> Result<(CalibrationBoundaryRecord, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::UndoCalibrationBoundaryAndRecompute { now_ms, reply })
            .await
    }

    pub async fn purge_raw_samples(&self) -> Result<usize, StorageError> {
        self.call(|reply| DbCommand::PurgeRawSamples { reply })
            .await
    }

    pub async fn hard_wipe(
        &self,
        wipe_all: bool,
        now_ms: i64,
    ) -> Result<CalibrationState, StorageError> {
        self.call(|reply| DbCommand::HardWipe {
            wipe_all,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn backup_sqlite(&self, dest_path: PathBuf) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::BackupSqlite { dest_path, reply })
            .await
    }

    pub async fn get_storage_stats(&self) -> Result<StorageStats, StorageError> {
        self.call(|reply| DbCommand::GetStorageStats { reply })
            .await
    }

    pub async fn list_calibration_boundaries(
        &self,
    ) -> Result<Vec<CalibrationBoundaryRecord>, StorageError> {
        self.call(|reply| DbCommand::ListCalibrationBoundaries { reply })
            .await
    }

    pub async fn export_bundle(
        &self,
        dest_path: PathBuf,
        options: ExportBundleOptions,
        now_ms: i64,
    ) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::ExportBundle {
            dest_path,
            options,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn import_bundle(
        &self,
        src_path: PathBuf,
        mode: ImportMode,
        force: bool,
        now_ms: i64,
    ) -> Result<(ImportSummary, CalibrationState), StorageError> {
        self.call(|reply| DbCommand::ImportBundle {
            src_path,
            mode,
            force,
            now_ms,
            reply,
        })
        .await
    }

    pub async fn set_retention_config(&self, config: RetentionConfig) -> Result<(), StorageError> {
        self.call(|reply| DbCommand::SetRetentionConfig { config, reply })
            .await
    }

    pub async fn prune_retention(&self, now_ms: i64) -> Result<PruneSummary, StorageError> {
        self.call(|reply| DbCommand::PruneRetention { now_ms, reply })
            .await
    }
}
