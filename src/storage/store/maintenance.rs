use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use super::LedgerStore;
use super::recompute_and_persist_in_tx;
use crate::calibration::{BoundaryReason, CalibrationBoundaryRecord, CalibrationState};
use crate::config::{RawSamplesRetention, RetentionConfig, SessionEpochsRetention};
use crate::paths::ensure_private_dir;
use crate::storage::lock::LedgerFileLock;
use crate::storage::schema::{enforce_private_db_permissions, vacuum_and_truncate_wal};
use crate::storage::{PruneSummary, StorageError, StorageStats};

impl LedgerStore {
    /// Executes `PRAGMA wal_checkpoint(TRUNCATE)` for atomic synchronization before sleep.
    pub fn flush_wal(&self) -> Result<(), StorageError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        enforce_private_db_permissions(&self.db_path)?;
        Ok(())
    }

    /// Reclaims freed SQLite freelist pages to the operating system via incremental vacuum.
    pub(crate) fn reclaim_freelist_pages(&self) -> Result<(), StorageError> {
        let mut stmt = self.conn.prepare("PRAGMA incremental_vacuum;")?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        drop(rows);
        drop(stmt);
        enforce_private_db_permissions(&self.db_path)?;
        Ok(())
    }

    /// Sets the active retention policy for three-tier SQLite pruning.
    pub fn set_retention_config(&mut self, config: RetentionConfig) {
        self.retention_config = config;
    }

    /// Prunes expired raw samples and session epochs according to active retention settings.
    pub fn prune_retention(&mut self, now_ms: i64) -> Result<PruneSummary, StorageError> {
        let retention = self.retention_config.clone();
        let summary = self.write_tx(move |tx| prune_retention_in_tx(tx, &retention, now_ms))?;
        if summary.raw_samples_pruned > 0 || summary.session_epochs_pruned > 0 {
            self.reclaim_freelist_pages()?;
            self.flush_wal()?;
        }
        Ok(summary)
    }

    /// Purges and zero-overwrites all `raw_sample_chunks` for closed sessions and reclaims storage via incremental vacuum.
    pub fn purge_raw_samples(&mut self) -> Result<usize, StorageError> {
        let deleted = self.write_tx(|tx| {
            let deleted_samples = delete_raw_chunks_batched(tx, None)?;
            if deleted_samples > 0 {
                tx.execute(
                    "UPDATE sessions SET raw_samples_count = 0
                     WHERE status = 'closed' AND raw_samples_count != 0",
                    [],
                )?;
            }
            Ok(deleted_samples)
        })?;
        if deleted > 0 {
            self.reclaim_freelist_pages()?;
        }
        self.flush_wal()?;
        Ok(deleted)
    }

    /// Hard wipes biometric tables, resets calibration to `Cold Start`, and runs `VACUUM` + WAL truncation.
    pub fn hard_wipe(
        &mut self,
        wipe_all: bool,
        now_ms: i64,
    ) -> Result<CalibrationState, StorageError> {
        let cal_state = self.write_tx(|tx| {
            tx.execute("DELETE FROM raw_sample_chunks", [])?;
            tx.execute("DELETE FROM session_epochs", [])?;
            tx.execute("DELETE FROM session_feedback", [])?;
            tx.execute("DELETE FROM sessions", [])?;
            tx.execute("DELETE FROM calibration_boundaries", [])?;
            tx.execute("DELETE FROM calibration_meta", [])?;
            if wipe_all {
                tx.execute("DELETE FROM wearables", [])?;
            }
            recompute_and_persist_in_tx(tx, now_ms)
        })?;

        let _lock = if !self.lock_path.as_os_str().is_empty() {
            Some(LedgerFileLock::acquire_exclusive(&self.lock_path)?)
        } else {
            None
        };
        // Set auto_vacuum before VACUUM so incremental mode is guaranteed, then truncate WAL
        // so VACUUM's rewritten pages are flushed to the main DB and deleted data cannot linger in -wal.
        vacuum_and_truncate_wal(&self.conn)?;
        enforce_private_db_permissions(&self.db_path)?;
        Ok(cal_state)
    }

    /// Creates a WAL-safe `0600` SQLite online hot backup.
    pub fn backup_sqlite(&self, dest_path: &Path) -> Result<(), StorageError> {
        if let Some(parent) = dest_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        let mut dst = Connection::open(dest_path)?;
        let backup = rusqlite::backup::Backup::new(&self.conn, &mut dst)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(50), None)?;
        drop(backup);
        drop(dst);

        enforce_private_db_permissions(dest_path)?;
        Ok(())
    }

    /// Reports database size, row counts, active retention, and calibration boundaries.
    pub fn get_storage_stats(&self) -> Result<StorageStats, StorageError> {
        let mut db_size_bytes = std::fs::metadata(&self.db_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let wal_path = PathBuf::from(format!("{}-wal", self.db_path.display()));
        let wal_size_bytes = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        db_size_bytes += wal_size_bytes;
        let shm_path = PathBuf::from(format!("{}-shm", self.db_path.display()));
        if let Ok(m) = std::fs::metadata(&shm_path) {
            db_size_bytes += m.len();
        }

        let db_size_formatted = if db_size_bytes < 1024 {
            format!("{db_size_bytes} B")
        } else if db_size_bytes < 1_048_576 {
            format!("{:.1} KiB", db_size_bytes as f64 / 1024.0)
        } else {
            format!("{:.2} MiB", db_size_bytes as f64 / 1_048_576.0)
        };

        let sessions_count = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))?
            as usize;
        let session_epochs_count =
            self.conn
                .query_row("SELECT COUNT(*) FROM session_epochs", [], |r| {
                    r.get::<_, i64>(0)
                })? as usize;
        let raw_samples_count = self.conn.query_row(
            "SELECT COALESCE(SUM(sample_count), 0) FROM raw_sample_chunks",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize;
        let session_feedback_count =
            self.conn
                .query_row("SELECT COUNT(*) FROM session_feedback", [], |r| {
                    r.get::<_, i64>(0)
                })? as usize;
        let calibration_boundaries_count =
            self.conn
                .query_row("SELECT COUNT(*) FROM calibration_boundaries", [], |r| {
                    r.get::<_, i64>(0)
                })? as usize;
        let wearables_count = self
            .conn
            .query_row("SELECT COUNT(*) FROM wearables", [], |r| r.get::<_, i64>(0))?
            as usize;

        let state = self.get_calibration_state()?;
        let active_boundary: Option<CalibrationBoundaryRecord> = self
            .conn
            .query_row(
                "SELECT id, effective_ts_ms, reason, seed_sleep_hr, reset_floors, created_at_ms, reverted_at_ms
                 FROM calibration_boundaries
                 WHERE reverted_at_ms IS NULL
                 ORDER BY effective_ts_ms DESC, created_at_ms DESC, id DESC
                 LIMIT 1",
                [],
                |row| {
                    let reason_str: String = row.get(2)?;
                    Ok(CalibrationBoundaryRecord {
                        id: row.get(0)?,
                        effective_ts_ms: row.get(1)?,
                        reason: reason_str.parse().unwrap_or(BoundaryReason::ManualRebaseline),
                        seed_sleep_hr: row.get(3)?,
                        reset_floors: row.get::<_, i64>(4)? != 0,
                        created_at_ms: row.get(5)?,
                        reverted_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()?;

        Ok(StorageStats {
            db_size_bytes,
            wal_size_bytes,
            db_size_formatted,
            sessions_count,
            session_epochs_count,
            raw_samples_count,
            session_feedback_count,
            calibration_boundaries_count,
            wearables_count,
            raw_samples_retention: self.retention_config.raw_samples_days.to_string(),
            session_epochs_retention: self.retention_config.session_epochs_days.to_string(),
            calibration_maturity: state.maturity,
            active_calibration_boundary: active_boundary,
        })
    }
}

fn delete_raw_chunks_batched(
    tx: &Connection,
    cutoff_ms: Option<i64>,
) -> Result<usize, StorageError> {
    let mut deleted_samples = 0usize;
    loop {
        let batch: Vec<(i64, i64, i64)> = {
            let mut stmt = tx.prepare_cached(
                "SELECT c.session_pk, c.seq, c.sample_count
                 FROM raw_sample_chunks c
                 JOIN sessions s ON s.pk = c.session_pk
                 WHERE s.status = 'closed' AND (?1 IS NULL OR c.end_ts_ms < ?1)
                 LIMIT 500",
            )?;
            stmt.query_map(params![cutoff_ms], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        if batch.is_empty() {
            break;
        }
        let mut del =
            tx.prepare_cached("DELETE FROM raw_sample_chunks WHERE session_pk = ?1 AND seq = ?2")?;
        for (session_pk, seq, sample_count) in batch {
            let changed = del.execute(params![session_pk, seq])?;
            if changed > 0 {
                deleted_samples += sample_count.max(0) as usize;
            }
        }
    }
    Ok(deleted_samples)
}

pub(crate) fn prune_retention_in_tx(
    tx: &Connection,
    retention: &RetentionConfig,
    now_ms: i64,
) -> Result<PruneSummary, StorageError> {
    let mut raw_pruned = 0;
    let mut epochs_pruned = 0;

    match retention.raw_samples_days {
        RawSamplesRetention::Disabled => {}
        RawSamplesRetention::Days(days) => {
            let cutoff_ms = now_ms - (days as i64 * 86_400_000);
            raw_pruned = delete_raw_chunks_batched(tx, Some(cutoff_ms))?;
        }
        RawSamplesRetention::Unlimited => {}
    }

    match retention.session_epochs_days {
        SessionEpochsRetention::Days(days) => {
            let cutoff_ms = now_ms - (days as i64 * 86_400_000);
            loop {
                let batch: Vec<(i64, i64)> = {
                    let mut stmt = tx.prepare_cached(
                        "SELECT e.session_pk, e.seq
                         FROM session_epochs e
                         JOIN sessions s ON s.pk = e.session_pk
                         WHERE s.status = 'closed' AND e.ts_ms < ?1
                         LIMIT 500",
                    )?;
                    stmt.query_map(params![cutoff_ms], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<Result<Vec<_>, _>>()?
                };
                if batch.is_empty() {
                    break;
                }
                let mut del = tx.prepare_cached(
                    "DELETE FROM session_epochs WHERE session_pk = ?1 AND seq = ?2",
                )?;
                for (session_pk, seq) in batch {
                    epochs_pruned += del.execute(params![session_pk, seq])?;
                }
            }
        }
        SessionEpochsRetention::Unlimited => {}
    }

    if raw_pruned > 0 || epochs_pruned > 0 {
        tx.execute(
            "UPDATE sessions
             SET raw_samples_count = COALESCE((SELECT SUM(sample_count) FROM raw_sample_chunks WHERE session_pk = sessions.pk), 0),
                 epochs_count = (SELECT COUNT(*) FROM session_epochs WHERE session_pk = sessions.pk)
             WHERE status = 'closed'",
            [],
        )?;
        let _ = tx.execute("PRAGMA incremental_vacuum;", []);
    }

    Ok(PruneSummary {
        raw_samples_pruned: raw_pruned,
        session_epochs_pruned: epochs_pruned,
    })
}
