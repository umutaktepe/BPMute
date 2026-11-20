use rusqlite::OptionalExtension;
use rusqlite::params;

use super::LedgerStore;
use super::{load_ledger_recompute_input, recompute_and_persist_in_tx};
use crate::calibration::{
    BoundaryReason, CalibrationBoundaryRecord, CalibrationState, recompute_calibration,
    validate_sleep_hr_bpm,
};
use crate::storage::schema::generate_uuid_v7_with;
use crate::storage::{AcceptShiftResult, StorageError};

impl LedgerStore {
    /// Returns the cached `CalibrationState` if present, or lazily recomputes it.
    pub fn get_calibration_state(&self) -> Result<CalibrationState, StorageError> {
        let cached: Option<String> = self
            .conn
            .query_row(
                "SELECT state_json FROM calibration_state WHERE singleton_id = 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(json) = cached {
            let state: CalibrationState = serde_json::from_str(&json)?;
            return Ok(state);
        }
        self.recompute_from_scratch()
    }

    /// Deterministically recomputes `CalibrationState` directly from ledger tables (read-only).
    pub fn recompute_from_scratch(&self) -> Result<CalibrationState, StorageError> {
        let input = load_ledger_recompute_input(&self.conn)?;
        Ok(recompute_calibration(&input))
    }

    /// Sets or clears the Day-1 sleeping-HR seed and recomputes `calibration_state`.
    pub fn set_day1_seed_and_recompute(
        &mut self,
        sleep_hr: Option<f64>,
        now_ms: i64,
    ) -> Result<CalibrationState, StorageError> {
        if let Some(hr) = sleep_hr {
            validate_sleep_hr_bpm("Day-1 sleeping HR seed", hr)
                .map_err(StorageError::Validation)?;
        }

        self.write_tx(|tx| {
            let active_boundary_id: Option<String> = tx
                .query_row(
                    "SELECT id FROM calibration_boundaries
                     WHERE reverted_at_ms IS NULL
                     ORDER BY effective_ts_ms DESC, created_at_ms DESC, id DESC
                     LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;

            if let Some(boundary_id) = active_boundary_id {
                tx.execute(
                    "UPDATE calibration_boundaries SET seed_sleep_hr = ?2 WHERE id = ?1",
                    params![boundary_id, sleep_hr],
                )?;
            } else {
                tx.execute(
                    "INSERT INTO calibration_meta (singleton_id, day1_seed_sleep_hr, day1_seed_updated_at_ms)
                     VALUES (1, ?1, ?2)
                     ON CONFLICT(singleton_id) DO UPDATE SET
                         day1_seed_sleep_hr = excluded.day1_seed_sleep_hr,
                         day1_seed_updated_at_ms = excluded.day1_seed_updated_at_ms",
                    params![sleep_hr, now_ms],
                )?;
            }

            recompute_and_persist_in_tx(tx, now_ms)
        })
    }

    /// Accepts a pending 5-session shift by inserting a reversible `Calibration Boundary`.
    pub fn accept_shift_and_recompute(
        &mut self,
        now_ms: i64,
    ) -> Result<AcceptShiftResult, StorageError> {
        let boundary_id = generate_uuid_v7_with(&self.uuid_ctx, now_ms);
        self.write_tx(|tx| {
            let current = recompute_calibration(&load_ledger_recompute_input(tx)?);
            let alert = current.pending_shift_alert.ok_or_else(|| {
                StorageError::PreconditionFailed(
                    "no pending 5-session persistent-shift alert to accept".to_string(),
                )
            })?;

            let boundary = CalibrationBoundaryRecord {
                id: boundary_id,
                effective_ts_ms: alert.first_session_start_ts_ms,
                reason: BoundaryReason::PersistentShift,
                seed_sleep_hr: None,
                reset_floors: false,
                created_at_ms: now_ms,
                reverted_at_ms: None,
            };

            tx.execute(
                "INSERT INTO calibration_boundaries (
                     id, effective_ts_ms, reason, seed_sleep_hr, reset_floors, created_at_ms, reverted_at_ms
                 ) VALUES (?1, ?2, ?3, NULL, 0, ?4, NULL)",
                params![
                    boundary.id,
                    boundary.effective_ts_ms,
                    boundary.reason.as_str(),
                    boundary.created_at_ms
                ],
            )?;

            tx.execute(
                "INSERT INTO calibration_meta (
                     singleton_id, dismissed_shift_anchor_ts_ms, dismissed_shift_direction
                 ) VALUES (1, NULL, NULL)
                 ON CONFLICT(singleton_id) DO UPDATE SET
                     dismissed_shift_anchor_ts_ms = NULL,
                     dismissed_shift_direction = NULL",
                [],
            )?;

            let calibration = recompute_and_persist_in_tx(tx, now_ms)?;
            Ok(AcceptShiftResult {
                boundary,
                calibration,
            })
        })
    }

    /// Dismisses a pending 5-session shift prompt (`Keep ignoring`).
    pub fn dismiss_shift_and_recompute(
        &mut self,
        now_ms: i64,
    ) -> Result<CalibrationState, StorageError> {
        self.write_tx(|tx| {
            let current = recompute_calibration(&load_ledger_recompute_input(tx)?);
            let alert = current.pending_shift_alert.ok_or_else(|| {
                StorageError::PreconditionFailed(
                    "no pending 5-session persistent-shift alert to dismiss".to_string(),
                )
            })?;

            tx.execute(
                "INSERT INTO calibration_meta (
                     singleton_id, dismissed_shift_anchor_ts_ms, dismissed_shift_direction
                 ) VALUES (1, ?1, ?2)
                 ON CONFLICT(singleton_id) DO UPDATE SET
                     dismissed_shift_anchor_ts_ms = excluded.dismissed_shift_anchor_ts_ms,
                     dismissed_shift_direction = excluded.dismissed_shift_direction",
                params![alert.first_session_start_ts_ms, alert.direction.as_str()],
            )?;

            recompute_and_persist_in_tx(tx, now_ms)
        })
    }

    /// Sets a manual reversible `Calibration Boundary` resetting personal physiology to `Cold Start`.
    pub fn set_calibration_boundary(
        &mut self,
        effective_ts_ms: Option<i64>,
        seed_sleep_hr: Option<f64>,
        reset_floors: bool,
        now_ms: i64,
    ) -> Result<(CalibrationBoundaryRecord, CalibrationState), StorageError> {
        if let Some(hr) = seed_sleep_hr {
            validate_sleep_hr_bpm("Day-1 sleeping HR seed", hr)
                .map_err(StorageError::Validation)?;
        }
        let boundary_id = generate_uuid_v7_with(&self.uuid_ctx, now_ms);
        let eff_ts = effective_ts_ms.unwrap_or(now_ms);

        self.write_tx(|tx| {
            let boundary = CalibrationBoundaryRecord {
                id: boundary_id,
                effective_ts_ms: eff_ts,
                reason: BoundaryReason::ManualRebaseline,
                seed_sleep_hr,
                reset_floors,
                created_at_ms: now_ms,
                reverted_at_ms: None,
            };

            tx.execute(
                "INSERT INTO calibration_boundaries (
                     id, effective_ts_ms, reason, seed_sleep_hr, reset_floors, created_at_ms, reverted_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![
                    boundary.id,
                    boundary.effective_ts_ms,
                    boundary.reason.as_str(),
                    boundary.seed_sleep_hr,
                    if boundary.reset_floors { 1 } else { 0 },
                    boundary.created_at_ms,
                ],
            )?;

            let state = recompute_and_persist_in_tx(tx, now_ms)?;
            Ok((boundary, state))
        })
    }

    /// Undoes the latest active `Calibration Boundary`, restoring prior calibration state.
    pub fn undo_calibration_boundary(
        &mut self,
        now_ms: i64,
    ) -> Result<(CalibrationBoundaryRecord, CalibrationState), StorageError> {
        self.write_tx(|tx| {
            let active_boundary: Option<CalibrationBoundaryRecord> = tx
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

            let mut boundary = active_boundary.ok_or_else(|| {
                StorageError::PreconditionFailed("no active calibration boundary to undo".to_string())
            })?;

            tx.execute(
                "UPDATE calibration_boundaries SET reverted_at_ms = ?2 WHERE id = ?1",
                params![boundary.id, now_ms],
            )?;
            boundary.reverted_at_ms = Some(now_ms);

            let state = recompute_and_persist_in_tx(tx, now_ms)?;
            Ok((boundary, state))
        })
    }

    /// Lists all calibration boundaries ordered newest-first by effective timestamp.
    pub fn list_calibration_boundaries(
        &self,
    ) -> Result<Vec<CalibrationBoundaryRecord>, StorageError> {
        let mut cb_stmt = self.conn.prepare(
            "SELECT id, effective_ts_ms, reason, seed_sleep_hr, reset_floors, created_at_ms, reverted_at_ms
             FROM calibration_boundaries
             ORDER BY effective_ts_ms DESC, created_at_ms DESC, id DESC",
        )?;
        let boundaries = cb_stmt
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
        Ok(boundaries)
    }
}
