//! Synchronous SQLite WAL ledger store with exclusive `flock` write coordination.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use uuid::ContextV7;

use crate::calibration::{
    BoundaryReason, CalibrationBoundaryRecord, CalibrationState, DismissedShift,
    LedgerRecomputeInput, SessionReplayInput, ShiftDirection, WearableSeedMeta,
    recompute_calibration,
};
use crate::config::RetentionConfig;
use crate::paths::ensure_private_dir;
use crate::storage::StorageError;
use crate::storage::lock::LedgerFileLock;
use crate::storage::schema::{
    db_exists, enforce_private_db_permissions, initialize_ro_connection, initialize_rw_connection,
};

mod bundle_io;
pub mod calibration;
pub mod maintenance;
pub mod session;
mod session_export;
mod session_lifecycle;
pub mod wearable;

use session::{
    SESSION_LEDGER_COLUMNS_COUNT, load_session_epochs, map_optional_session_feedback_row,
    map_session_ledger_row,
};

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

    fn write_tx_and_flush<T, F>(&mut self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<T, StorageError>,
    {
        let out = self.write_tx(f)?;
        self.flush_wal()?;
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
                s.pre_candidate_mu_hr, s.pre_candidate_sigma_hr, s.pre_candidate_ln_rmssd,
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
            let w_rr = row.get::<_, i64>(SESSION_LEDGER_COLUMNS_COUNT)? != 0;
            let w_contact = row.get::<_, i64>(SESSION_LEDGER_COLUMNS_COUNT + 1)? != 0;
            let feedback = map_optional_session_feedback_row(
                row,
                SESSION_LEDGER_COLUMNS_COUNT + 2,
                record.id.clone(),
            )?;
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
