//! Synchronous SQLite WAL ledger store with exclusive `flock` write coordination.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use uuid::ContextV7;

use crate::calibration::{
    BoundaryReason, CalibrationBoundaryRecord, CalibrationState, DismissedShift,
    LedgerRecomputeInput, SessionFeedbackRecord, SessionReplayInput, ShiftDirection,
    WearableSeedMeta, recompute_calibration,
};
use crate::config::{RawSamplesRetention, RetentionConfig, SessionEpochsRetention};
use crate::paths::ensure_private_dir;
use crate::storage::bundle::*;
use crate::storage::lock::LedgerFileLock;
use crate::storage::schema::{
    backups_dir_in_data_dir, db_exists, enforce_private_db_permissions, initialize_ro_connection,
    initialize_rw_connection,
};
use crate::storage::{PruneSummary, StorageError, StorageStats};

pub mod calibration;
pub mod session;
pub mod wearable;

use session::{load_session_epochs, map_optional_session_feedback_row, map_session_ledger_row};

/// Synchronous SQLite WAL store backing both the persistence worker and offline CLI commands.
pub struct LedgerStore {
    conn: Connection,
    db_path: PathBuf,
    lock_path: PathBuf,
    uuid_ctx: ContextV7,
    retention_config: RetentionConfig,
}

impl LedgerStore {
    /// Opens (and initializes if needed) a read-write SQLite WAL ledger with `0700`/`0600` modes.
    pub fn open_read_write(db_path: &Path, lock_path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = db_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        let _lock = LedgerFileLock::acquire_exclusive(lock_path)?;
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        initialize_rw_connection(&conn, Some(db_path))?;
        enforce_private_db_permissions(db_path)?;

        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
            lock_path: lock_path.to_path_buf(),
            uuid_ctx: ContextV7::new(),
            retention_config: RetentionConfig::default(),
        })
    }

    /// Opens an existing SQLite ledger strictly read-only, returning `Ok(None)` if absent.
    pub fn open_read_only(db_path: &Path) -> Result<Option<Self>, StorageError> {
        if !db_exists(db_path)? {
            return Ok(None);
        }
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        initialize_ro_connection(&conn)?;
        Ok(Some(Self {
            conn,
            db_path: db_path.to_path_buf(),
            lock_path: PathBuf::new(),
            uuid_ctx: ContextV7::new(),
            retention_config: RetentionConfig::default(),
        }))
    }

    /// Executes `PRAGMA wal_checkpoint(TRUNCATE)` for atomic synchronization before sleep.
    pub fn flush_wal(&self) -> Result<(), StorageError> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// Sets the active retention policy for three-tier SQLite pruning.
    pub fn set_retention_config(&mut self, config: RetentionConfig) {
        self.retention_config = config;
    }

    /// Prunes expired raw samples and session epochs according to active retention settings.
    pub fn prune_retention(&mut self, now_ms: i64) -> Result<PruneSummary, StorageError> {
        let retention = self.retention_config.clone();
        self.write_tx(move |tx| prune_retention_in_tx(tx, &retention, now_ms))
    }

    /// Purges and zero-overwrites all `raw_samples` for closed sessions and reclaims storage via incremental vacuum.
    pub fn purge_raw_samples(&mut self) -> Result<usize, StorageError> {
        self.write_tx(|tx| {
            let mut deleted = 0;
            loop {
                let count = tx.execute(
                    "DELETE FROM raw_samples WHERE rowid IN (
                        SELECT rowid FROM raw_samples
                        WHERE session_id IN (SELECT id FROM sessions WHERE status = 'closed')
                        LIMIT 500
                    )",
                    [],
                )?;
                deleted += count;
                if count == 0 {
                    break;
                }
            }
            tx.execute(
                "UPDATE sessions SET raw_samples_count = 0 WHERE status = 'closed'",
                [],
            )?;
            let _ = tx.execute("PRAGMA incremental_vacuum(500);", []);
            Ok(deleted)
        })
    }

    /// Hard wipes biometric tables, resets calibration to `Cold Start`, and runs `VACUUM`.
    pub fn hard_wipe(&mut self, wipe_all: bool, now_ms: i64) -> Result<(), StorageError> {
        self.write_tx(|tx| {
            tx.execute("DELETE FROM raw_samples", [])?;
            tx.execute("DELETE FROM session_epochs", [])?;
            tx.execute("DELETE FROM session_feedback", [])?;
            tx.execute("DELETE FROM sessions", [])?;
            tx.execute("DELETE FROM calibration_boundaries", [])?;
            tx.execute("DELETE FROM calibration_meta", [])?;
            if wipe_all {
                tx.execute("DELETE FROM wearables", [])?;
            }
            recompute_and_persist_in_tx(tx, now_ms)?;
            Ok(())
        })?;

        let _lock = if !self.lock_path.as_os_str().is_empty() {
            Some(LedgerFileLock::acquire_exclusive(&self.lock_path)?)
        } else {
            None
        };
        self.conn.execute_batch(
            "PRAGMA secure_delete = ON;
             VACUUM;
             PRAGMA auto_vacuum = INCREMENTAL;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
        enforce_private_db_permissions(&self.db_path)?;
        Ok(())
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
        if let Ok(m) = std::fs::metadata(&wal_path) {
            db_size_bytes += m.len();
        }
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
        let raw_samples_count =
            self.conn
                .query_row("SELECT COUNT(*) FROM raw_samples", [], |r| {
                    r.get::<_, i64>(0)
                })? as usize;
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

    /// Exports versioned JSON `Ledger Bundle` to `dest_path` with `0600` permissions.
    pub fn export_bundle(
        &self,
        dest_path: &Path,
        options: ExportBundleOptions,
        now_ms: i64,
    ) -> Result<(), StorageError> {
        let wearables = if options.include_wearables {
            self.list_enrolled_wearables()?
        } else {
            Vec::new()
        };

        let mut s_stmt = self.conn.prepare(
            "SELECT id, wearable_id, status, start_ts_ms, end_ts_ms, end_reason,
                    detection_sensitivity, had_rr, warmup_mu_hr, warmup_sigma_hr,
                    warmup_ln_rmssd, onset_candidate_ts_ms, onset_confirmed_ts_ms,
                    sleeping_ts_ms, sleep_mu_hr, onset_hr_bpm, delta_hr_onset,
                    sleep_ln_rmssd, sleep_hr_var_ratio, typical_sqi, post_trigger_sleep_sec,
                    trigger_power_action, rapid_resume_within_5m, grace_cancel_count,
                    rapid_wake_fp_count, snooze_count, auto_anomalous, epochs_count, raw_samples_count
             FROM sessions ORDER BY start_ts_ms ASC, id ASC",
        )?;
        let sessions = s_stmt
            .query_map([], |row| {
                Ok(SessionBundleRecord {
                    id: row.get(0)?,
                    wearable_id: row.get(1)?,
                    status: row.get(2)?,
                    start_ts_ms: row.get(3)?,
                    end_ts_ms: row.get(4)?,
                    end_reason: row.get(5)?,
                    detection_sensitivity: row.get(6)?,
                    had_rr: row.get::<_, i64>(7)? != 0,
                    warmup_mu_hr: row.get(8)?,
                    warmup_sigma_hr: row.get(9)?,
                    warmup_ln_rmssd: row.get(10)?,
                    onset_candidate_ts_ms: row.get(11)?,
                    onset_confirmed_ts_ms: row.get(12)?,
                    sleeping_ts_ms: row.get(13)?,
                    sleep_mu_hr: row.get(14)?,
                    onset_hr_bpm: row.get(15)?,
                    delta_hr_onset: row.get(16)?,
                    sleep_ln_rmssd: row.get(17)?,
                    sleep_hr_var_ratio: row.get(18)?,
                    typical_sqi: row.get(19)?,
                    post_trigger_sleep_sec: row.get(20)?,
                    trigger_power_action: row.get(21)?,
                    rapid_resume_within_5m: row.get::<_, i64>(22)? != 0,
                    grace_cancel_count: row.get(23)?,
                    rapid_wake_fp_count: row.get(24)?,
                    snooze_count: row.get(25)?,
                    auto_anomalous: row.get::<_, i64>(26)? != 0,
                    epochs_count: row.get(27)?,
                    raw_samples_count: row.get(28)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let session_epochs = if options.include_epochs {
            let mut ep_stmt = self.conn.prepare(
                "SELECT id, session_id, seq, ts_ms, hr_bpm_60s, z_hr, hr_slope_4m,
                        r_var_120s, ln_rmssd_60s, delta_ln_rmssd_60s, rmssd_sdnn_120s,
                        hrv_valid, rr_artifact_frac, coverage, flatline_stale,
                        accumulators_frozen, baseline_mu_hr, baseline_sigma_hr,
                        baseline_frozen, p_active_wake, p_quiet_wake, p_sleep, state
                 FROM session_epochs ORDER BY session_id ASC, seq ASC",
            )?;
            ep_stmt
                .query_map([], |row| {
                    Ok(SessionEpochBundleRecord {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        seq: row.get(2)?,
                        ts_ms: row.get(3)?,
                        hr_bpm_60s: row.get(4)?,
                        z_hr: row.get(5)?,
                        hr_slope_4m: row.get(6)?,
                        r_var_120s: row.get(7)?,
                        ln_rmssd_60s: row.get(8)?,
                        delta_ln_rmssd_60s: row.get(9)?,
                        rmssd_sdnn_120s: row.get(10)?,
                        hrv_valid: row.get::<_, i64>(11)? != 0,
                        rr_artifact_frac: row.get(12)?,
                        coverage: row.get(13)?,
                        flatline_stale: row.get::<_, i64>(14)? != 0,
                        accumulators_frozen: row.get::<_, i64>(15)? != 0,
                        baseline_mu_hr: row.get(16)?,
                        baseline_sigma_hr: row.get(17)?,
                        baseline_frozen: row.get::<_, i64>(18)? != 0,
                        p_active_wake: row.get(19)?,
                        p_quiet_wake: row.get(20)?,
                        p_sleep: row.get(21)?,
                        state: row.get(22)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        let raw_samples = if options.include_raw {
            let mut raw_stmt = self.conn.prepare(
                "SELECT id, session_id, seq, ts_ms, bpm, rr_ms_json, contact_status
                 FROM raw_samples ORDER BY session_id ASC, seq ASC",
            )?;
            raw_stmt
                .query_map([], |row| {
                    Ok(RawSampleBundleRecord {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        seq: row.get(2)?,
                        ts_ms: row.get(3)?,
                        bpm: row.get(4)?,
                        rr_ms_json: row.get(5)?,
                        contact_status: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        let mut fb_stmt = self.conn.prepare(
            "SELECT id, session_id, verdict, manual_onset_ts_ms, manual_avg_sleep_hr, notes, updated_at_ms
             FROM session_feedback ORDER BY session_id ASC",
        )?;
        let session_feedback = fb_stmt
            .query_map([], |row| {
                let verdict_str: Option<String> = row.get(2)?;
                let verdict = verdict_str.and_then(|s| s.parse().ok());
                Ok(SessionFeedbackRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    verdict,
                    manual_onset_ts_ms: row.get(3)?,
                    manual_avg_sleep_hr: row.get(4)?,
                    notes: row.get(5)?,
                    updated_at_ms: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut cb_stmt = self.conn.prepare(
            "SELECT id, effective_ts_ms, reason, seed_sleep_hr, reset_floors, created_at_ms, reverted_at_ms
             FROM calibration_boundaries ORDER BY effective_ts_ms ASC, created_at_ms ASC, id ASC",
        )?;
        let calibration_boundaries = cb_stmt
            .query_map([], |row| {
                let reason_str: String = row.get(2)?;
                Ok(CalibrationBoundaryRecord {
                    id: row.get(0)?,
                    effective_ts_ms: row.get(1)?,
                    reason: reason_str
                        .parse()
                        .unwrap_or(BoundaryReason::ManualRebaseline),
                    seed_sleep_hr: row.get(3)?,
                    reset_floors: row.get::<_, i64>(4)? != 0,
                    created_at_ms: row.get(5)?,
                    reverted_at_ms: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let calibration_meta = self
            .conn
            .query_row(
                "SELECT day1_seed_sleep_hr, day1_seed_updated_at_ms, dismissed_shift_anchor_ts_ms, dismissed_shift_direction
                 FROM calibration_meta WHERE singleton_id = 1",
                [],
                |row| {
                    Ok(CalibrationMetaBundleRecord {
                        day1_seed_sleep_hr: row.get(0)?,
                        day1_seed_updated_at_ms: row.get(1)?,
                        dismissed_shift_anchor_ts_ms: row.get(2)?,
                        dismissed_shift_direction: row.get(3)?,
                    })
                },
            )
            .optional()?;

        let bundle = LedgerBundle {
            schema_version: CURRENT_BUNDLE_SCHEMA_VERSION,
            exported_at_ms: now_ms,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            wearables,
            sessions,
            session_epochs,
            raw_samples,
            session_feedback,
            calibration_boundaries,
            calibration_meta,
        };

        if let Some(parent) = dest_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        use std::fs::OpenOptions;
        use std::os::unix::fs::OpenOptionsExt;
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dest_path)?;
        serde_json::to_writer_pretty(file, &bundle)?;
        let _ = std::fs::set_permissions(dest_path, std::fs::Permissions::from_mode(0o600));
        Ok(())
    }

    /// Imports a versioned JSON `Ledger Bundle` with idempotent merge or pre-replace rollback backup.
    pub fn import_bundle(
        &mut self,
        src_path: &Path,
        mode: ImportMode,
        force: bool,
        now_ms: i64,
    ) -> Result<(ImportSummary, CalibrationState), StorageError> {
        let content = std::fs::read_to_string(src_path)?;
        let bundle: LedgerBundle = serde_json::from_str(&content)?;
        validate_bundle_payload(&bundle)?;

        let mut rollback_backup_path = None;
        if mode == ImportMode::Replace {
            let data_dir = self
                .db_path
                .parent()
                .ok_or_else(|| StorageError::Validation("invalid db_path".to_string()))?;
            let backups_dir = backups_dir_in_data_dir(data_dir);
            ensure_private_dir(&backups_dir)?;
            let bkp_file = backups_dir.join(format!("bpmute-pre-replace-{now_ms}.db"));
            self.backup_sqlite(&bkp_file)?;
            rollback_backup_path = Some(bkp_file.to_string_lossy().to_string());
        }

        let mut summary = ImportSummary {
            mode: match mode {
                ImportMode::Merge => "merge".to_string(),
                ImportMode::Replace => "replace".to_string(),
            },
            rollback_backup_path,
            ..Default::default()
        };

        let state = self.write_tx(|tx| {
            if mode == ImportMode::Replace {
                tx.execute("DELETE FROM raw_samples", [])?;
                tx.execute("DELETE FROM session_epochs", [])?;
                tx.execute("DELETE FROM session_feedback", [])?;
                tx.execute("DELETE FROM sessions", [])?;
                tx.execute("DELETE FROM calibration_boundaries", [])?;
                tx.execute("DELETE FROM calibration_meta", [])?;
                if !bundle.wearables.is_empty() {
                    tx.execute("DELETE FROM wearables", [])?;
                }
            }

            // Import Wearables
            for w in &bundle.wearables {
                let inserted = tx.execute(
                    "INSERT INTO wearables (
                         id, mac, address_type, alias, advertised_name, supports_rr,
                         supports_contact, hr_var_floor, typical_sqi, enrolled_at_ms,
                         last_seen_at_ms, active
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                     ON CONFLICT(id) DO UPDATE SET
                         alias = COALESCE(excluded.alias, wearables.alias),
                         advertised_name = COALESCE(excluded.advertised_name, wearables.advertised_name),
                         last_seen_at_ms = MAX(wearables.last_seen_at_ms, excluded.last_seen_at_ms)
                     ON CONFLICT(mac, address_type) DO UPDATE SET
                         alias = COALESCE(excluded.alias, wearables.alias),
                         last_seen_at_ms = MAX(wearables.last_seen_at_ms, excluded.last_seen_at_ms)",
                    params![
                        w.id,
                        w.mac,
                        w.address_type,
                        w.alias,
                        w.advertised_name,
                        if w.supports_rr { 1 } else { 0 },
                        if w.supports_contact { 1 } else { 0 },
                        w.hr_var_floor,
                        w.typical_sqi,
                        w.enrolled_at_ms,
                        w.last_seen_at_ms,
                        if w.active { 1 } else { 0 },
                    ],
                )?;
                if inserted > 0 {
                    summary.wearables_imported += 1;
                }
            }

            // Import Sessions
            for s in &bundle.sessions {
                let inserted = tx.execute(
                    "INSERT INTO sessions (
                         id, wearable_id, status, start_ts_ms, end_ts_ms, end_reason,
                         detection_sensitivity, had_rr, warmup_mu_hr, warmup_sigma_hr,
                         warmup_ln_rmssd, onset_candidate_ts_ms, onset_confirmed_ts_ms,
                         sleeping_ts_ms, sleep_mu_hr, onset_hr_bpm, delta_hr_onset,
                         sleep_ln_rmssd, sleep_hr_var_ratio, typical_sqi, post_trigger_sleep_sec,
                         trigger_power_action, rapid_resume_within_5m, grace_cancel_count,
                         rapid_wake_fp_count, snooze_count, auto_anomalous, epochs_count, raw_samples_count
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)
                     ON CONFLICT(id) DO NOTHING",
                    params![
                        s.id,
                        s.wearable_id,
                        s.status,
                        s.start_ts_ms,
                        s.end_ts_ms,
                        s.end_reason,
                        s.detection_sensitivity,
                        if s.had_rr { 1 } else { 0 },
                        s.warmup_mu_hr,
                        s.warmup_sigma_hr,
                        s.warmup_ln_rmssd,
                        s.onset_candidate_ts_ms,
                        s.onset_confirmed_ts_ms,
                        s.sleeping_ts_ms,
                        s.sleep_mu_hr,
                        s.onset_hr_bpm,
                        s.delta_hr_onset,
                        s.sleep_ln_rmssd,
                        s.sleep_hr_var_ratio,
                        s.typical_sqi,
                        s.post_trigger_sleep_sec,
                        s.trigger_power_action,
                        if s.rapid_resume_within_5m { 1 } else { 0 },
                        s.grace_cancel_count,
                        s.rapid_wake_fp_count,
                        s.snooze_count,
                        if s.auto_anomalous { 1 } else { 0 },
                        s.epochs_count,
                        s.raw_samples_count,
                    ],
                )?;
                if inserted > 0 {
                    summary.sessions_imported += 1;
                }
            }

            // Import Feedback (preserving newer local feedback unless force)
            for fb in &bundle.session_feedback {
                let existing_updated: Option<i64> = tx
                    .query_row(
                        "SELECT updated_at_ms FROM session_feedback WHERE session_id = ?1",
                        params![fb.session_id],
                        |r| r.get(0),
                    )
                    .optional()?;

                let should_write = match existing_updated {
                    None => true,
                    Some(local_ts) => force || fb.updated_at_ms > local_ts,
                };

                if should_write {
                    tx.execute(
                        "INSERT INTO session_feedback (
                             id, session_id, verdict, manual_onset_ts_ms, manual_avg_sleep_hr, notes, updated_at_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(session_id) DO UPDATE SET
                             verdict = excluded.verdict,
                             manual_onset_ts_ms = excluded.manual_onset_ts_ms,
                             manual_avg_sleep_hr = excluded.manual_avg_sleep_hr,
                             notes = excluded.notes,
                             updated_at_ms = excluded.updated_at_ms",
                        params![
                            fb.id,
                            fb.session_id,
                            fb.verdict.map(|v| v.as_str()),
                            fb.manual_onset_ts_ms,
                            fb.manual_avg_sleep_hr,
                            fb.notes,
                            fb.updated_at_ms,
                        ],
                    )?;
                    summary.feedback_imported += 1;
                }
            }

            // Import Epochs
            for ep in &bundle.session_epochs {
                let inserted = tx.execute(
                    "INSERT INTO session_epochs (
                         id, session_id, seq, ts_ms, hr_bpm_60s, z_hr, hr_slope_4m,
                         r_var_120s, ln_rmssd_60s, delta_ln_rmssd_60s, rmssd_sdnn_120s,
                         hrv_valid, rr_artifact_frac, coverage, flatline_stale,
                         accumulators_frozen, baseline_mu_hr, baseline_sigma_hr,
                         baseline_frozen, p_active_wake, p_quiet_wake, p_sleep, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
                     ON CONFLICT(session_id, seq) DO NOTHING",
                    params![
                        ep.id,
                        ep.session_id,
                        ep.seq,
                        ep.ts_ms,
                        ep.hr_bpm_60s,
                        ep.z_hr,
                        ep.hr_slope_4m,
                        ep.r_var_120s,
                        ep.ln_rmssd_60s,
                        ep.delta_ln_rmssd_60s,
                        ep.rmssd_sdnn_120s,
                        if ep.hrv_valid { 1 } else { 0 },
                        ep.rr_artifact_frac,
                        ep.coverage,
                        if ep.flatline_stale { 1 } else { 0 },
                        if ep.accumulators_frozen { 1 } else { 0 },
                        ep.baseline_mu_hr,
                        ep.baseline_sigma_hr,
                        if ep.baseline_frozen { 1 } else { 0 },
                        ep.p_active_wake,
                        ep.p_quiet_wake,
                        ep.p_sleep,
                        ep.state,
                    ],
                )?;
                if inserted > 0 {
                    summary.epochs_imported += 1;
                }
            }

            // Import Raw Samples
            for raw in &bundle.raw_samples {
                let inserted = tx.execute(
                    "INSERT INTO raw_samples (
                         id, session_id, seq, ts_ms, bpm, rr_ms_json, contact_status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(session_id, seq) DO NOTHING",
                    params![
                        raw.id,
                        raw.session_id,
                        raw.seq,
                        raw.ts_ms,
                        raw.bpm,
                        raw.rr_ms_json,
                        raw.contact_status,
                    ],
                )?;
                if inserted > 0 {
                    summary.raw_samples_imported += 1;
                }
            }

            // Import Calibration Boundaries
            for cb in &bundle.calibration_boundaries {
                let inserted = tx.execute(
                    "INSERT INTO calibration_boundaries (
                         id, effective_ts_ms, reason, seed_sleep_hr, reset_floors,
                         created_at_ms, reverted_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(id) DO NOTHING",
                    params![
                        cb.id,
                        cb.effective_ts_ms,
                        cb.reason.as_str(),
                        cb.seed_sleep_hr,
                        if cb.reset_floors { 1 } else { 0 },
                        cb.created_at_ms,
                        cb.reverted_at_ms,
                    ],
                )?;
                if inserted > 0 {
                    summary.boundaries_imported += 1;
                }
            }

            // Import Calibration Meta
            if let Some(ref meta) = bundle.calibration_meta {
                tx.execute(
                    "INSERT INTO calibration_meta (
                         singleton_id, day1_seed_sleep_hr, day1_seed_updated_at_ms,
                         dismissed_shift_anchor_ts_ms, dismissed_shift_direction
                     ) VALUES (1, ?1, ?2, ?3, ?4)
                     ON CONFLICT(singleton_id) DO UPDATE SET
                         day1_seed_sleep_hr = COALESCE(excluded.day1_seed_sleep_hr, calibration_meta.day1_seed_sleep_hr),
                         day1_seed_updated_at_ms = COALESCE(excluded.day1_seed_updated_at_ms, calibration_meta.day1_seed_updated_at_ms)",
                    params![
                        meta.day1_seed_sleep_hr,
                        meta.day1_seed_updated_at_ms,
                        meta.dismissed_shift_anchor_ts_ms,
                        meta.dismissed_shift_direction,
                    ],
                )?;
            }

            recompute_and_persist_in_tx(tx, now_ms)
        })?;

        Ok((summary, state))
    }

    fn write_tx<T, F>(&mut self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StorageError>,
    {
        if self.lock_path.as_os_str().is_empty() {
            return Err(StorageError::Validation(
                "cannot perform write transaction on read-only LedgerStore".to_string(),
            ));
        }
        let _lock = LedgerFileLock::acquire_exclusive(&self.lock_path)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        enforce_private_db_permissions(&self.db_path)?;
        Ok(out)
    }
}

pub(crate) fn recompute_and_persist_in_tx(
    tx: &Connection,
    now_ms: i64,
) -> Result<CalibrationState, StorageError> {
    let input = load_ledger_recompute_input(tx)?;
    let state = recompute_calibration(&input);

    let mut update_w = tx.prepare(
        "UPDATE wearables
         SET hr_var_floor = ?2,
             supports_rr = ?3,
             supports_contact = ?4,
             typical_sqi = ?5
         WHERE id = ?1",
    )?;
    for w in state.wearables.values() {
        update_w.execute(params![
            w.wearable_id,
            w.hr_var_floor,
            i32::from(w.supports_rr),
            i32::from(w.supports_contact),
            w.typical_sqi
        ])?;
    }
    drop(update_w);

    let state_json = serde_json::to_string(&state)?;
    tx.execute(
        "INSERT INTO calibration_state (singleton_id, algorithm_version, computed_at_ms, state_json)
         VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(singleton_id) DO UPDATE SET
             algorithm_version = excluded.algorithm_version,
             computed_at_ms = excluded.computed_at_ms,
             state_json = excluded.state_json",
        params![state.algorithm_version, now_ms, state_json],
    )?;

    Ok(state)
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
            loop {
                let count = tx.execute(
                    "DELETE FROM raw_samples WHERE rowid IN (
                        SELECT rowid FROM raw_samples
                        WHERE session_id IN (SELECT id FROM sessions WHERE status = 'closed')
                          AND ts_ms < ?1
                        LIMIT 500
                    )",
                    params![cutoff_ms],
                )?;
                raw_pruned += count;
                if count == 0 {
                    break;
                }
            }
        }
        RawSamplesRetention::Unlimited => {}
    }

    match retention.session_epochs_days {
        SessionEpochsRetention::Days(days) => {
            let cutoff_ms = now_ms - (days as i64 * 86_400_000);
            loop {
                let count = tx.execute(
                    "DELETE FROM session_epochs WHERE rowid IN (
                        SELECT rowid FROM session_epochs
                        WHERE session_id IN (SELECT id FROM sessions WHERE status = 'closed')
                          AND ts_ms < ?1
                        LIMIT 500
                    )",
                    params![cutoff_ms],
                )?;
                epochs_pruned += count;
                if count == 0 {
                    break;
                }
            }
        }
        SessionEpochsRetention::Unlimited => {}
    }

    if raw_pruned > 0 || epochs_pruned > 0 {
        tx.execute(
            "UPDATE sessions
             SET raw_samples_count = (SELECT COUNT(*) FROM raw_samples WHERE session_id = sessions.id),
                 epochs_count = (SELECT COUNT(*) FROM session_epochs WHERE session_id = sessions.id)
             WHERE status = 'closed'",
            [],
        )?;
        let _ = tx.execute("PRAGMA incremental_vacuum(500);", []);
    }

    Ok(PruneSummary {
        raw_samples_pruned: raw_pruned,
        session_epochs_pruned: epochs_pruned,
    })
}

pub(crate) fn load_ledger_recompute_input(
    conn: &Connection,
) -> Result<LedgerRecomputeInput, StorageError> {
    let (day1_seed_sleep_hr, dismissed_shift) = conn
        .query_row(
            "SELECT day1_seed_sleep_hr, dismissed_shift_anchor_ts_ms, dismissed_shift_direction
             FROM calibration_meta WHERE singleton_id = 1",
            [],
            |row| {
                let seed: Option<f64> = row.get(0)?;
                let anchor: Option<i64> = row.get(1)?;
                let dir_str: Option<String> = row.get(2)?;
                let dismissed = match (anchor, dir_str) {
                    (Some(a), Some(d)) => {
                        d.parse::<ShiftDirection>()
                            .ok()
                            .map(|direction| DismissedShift {
                                anchor_ts_ms: a,
                                direction,
                            })
                    }
                    _ => None,
                };
                Ok((seed, dismissed))
            },
        )
        .optional()?
        .unwrap_or((None, None));

    let mut b_stmt = conn.prepare(
        "SELECT id, effective_ts_ms, reason, seed_sleep_hr, reset_floors, created_at_ms, reverted_at_ms
         FROM calibration_boundaries
         ORDER BY effective_ts_ms ASC, created_at_ms ASC, id ASC",
    )?;
    let boundaries = b_stmt
        .query_map([], |row| {
            let reason_str: String = row.get(2)?;
            Ok(CalibrationBoundaryRecord {
                id: row.get(0)?,
                effective_ts_ms: row.get(1)?,
                reason: reason_str
                    .parse()
                    .unwrap_or(BoundaryReason::ManualRebaseline),
                seed_sleep_hr: row.get(3)?,
                reset_floors: row.get::<_, i64>(4)? != 0,
                created_at_ms: row.get(5)?,
                reverted_at_ms: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut w_stmt =
        conn.prepare("SELECT id, supports_rr, supports_contact FROM wearables ORDER BY id ASC")?;
    let wearables = w_stmt
        .query_map([], |row| {
            Ok(WearableSeedMeta {
                wearable_id: row.get(0)?,
                supports_rr: row.get::<_, i64>(1)? != 0,
                supports_contact: row.get::<_, i64>(2)? != 0,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut s_stmt = conn.prepare(
        "SELECT s.id, s.wearable_id, s.start_ts_ms, s.end_ts_ms, s.end_reason, s.detection_sensitivity,
                s.had_rr, s.warmup_mu_hr, s.warmup_sigma_hr, s.warmup_ln_rmssd,
                s.onset_candidate_ts_ms, s.onset_confirmed_ts_ms, s.sleeping_ts_ms,
                s.sleep_mu_hr, s.onset_hr_bpm, s.delta_hr_onset, s.sleep_ln_rmssd,
                s.sleep_hr_var_ratio, s.typical_sqi, s.post_trigger_sleep_sec,
                s.trigger_power_action, s.rapid_resume_within_5m, s.grace_cancel_count,
                s.rapid_wake_fp_count, s.snooze_count, s.auto_anomalous,
                s.epochs_count, s.raw_samples_count,
                w.supports_rr, w.supports_contact,
                f.id, f.verdict, f.manual_onset_ts_ms, f.manual_avg_sleep_hr, f.notes, f.updated_at_ms
         FROM sessions s
         JOIN wearables w ON w.id = s.wearable_id
         LEFT JOIN session_feedback f ON f.session_id = s.id
         WHERE s.status = 'closed'
         ORDER BY s.start_ts_ms ASC, s.id ASC",
    )?;

    let session_rows = s_stmt
        .query_map([], |row| {
            let record = map_session_ledger_row(row)?;
            let w_rr = row.get::<_, i64>(28)? != 0;
            let w_contact = row.get::<_, i64>(29)? != 0;
            let feedback = map_optional_session_feedback_row(row, 30, record.id.clone())?;
            Ok((record, feedback, w_rr, w_contact))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut sessions = Vec::with_capacity(session_rows.len());
    for (record, feedback, wearable_supports_rr, wearable_supports_contact) in session_rows {
        let epochs = if feedback
            .as_ref()
            .is_some_and(|f| f.manual_onset_ts_ms.is_some())
        {
            load_session_epochs(conn, &record.id)?
        } else {
            Vec::new()
        };
        sessions.push(SessionReplayInput {
            session: record,
            feedback,
            epochs,
            wearable_supports_rr,
            wearable_supports_contact,
        });
    }

    Ok(LedgerRecomputeInput {
        day1_seed_sleep_hr,
        boundaries,
        dismissed_shift,
        wearables,
        sessions,
    })
}

pub(crate) fn resolve_table_id_prefix(
    conn: &Connection,
    table: &str,
    entity_name: &str,
    id_or_prefix: &str,
) -> Result<String, StorageError> {
    let trimmed = id_or_prefix.trim();
    if trimmed.is_empty() {
        return Err(StorageError::Validation(format!(
            "{entity_name} ID or prefix must not be empty"
        )));
    }

    let sql_exact = format!("SELECT id FROM {table} WHERE id = ?1");
    let exact: Option<String> = conn
        .query_row(&sql_exact, params![trimmed], |r| r.get(0))
        .optional()?;
    if let Some(id) = exact {
        return Ok(id);
    }

    let escaped = trimmed
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("{escaped}%");
    let sql_prefix = format!("SELECT id FROM {table} WHERE id LIKE ?1 ESCAPE '\\' LIMIT 2");
    let mut stmt = conn.prepare(&sql_prefix)?;
    let matches: Vec<String> = stmt
        .query_map(params![pattern], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;

    match matches.len() {
        0 => Err(StorageError::NotFound(format!(
            "{entity_name} '{trimmed}' not found"
        ))),
        1 => Ok(matches.into_iter().next().expect("single match present")),
        _ => Err(StorageError::AmbiguousId(format!(
            "{entity_name} prefix '{trimmed}' matches multiple {entity_name}s; provide more characters"
        ))),
    }
}
