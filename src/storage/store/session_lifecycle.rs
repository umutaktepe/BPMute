//! Active Monitoring Session recovery and pre-power-action trigger persistence.

use rusqlite::{OptionalExtension, params};

use crate::config::PowerAction;
use crate::storage::StorageError;

use super::{LedgerStore, recompute_and_persist_in_tx};

impl LedgerStore {
    /// Recovers stale `'active'` sessions left behind by an ungraceful daemon termination.
    pub fn recover_stale_sessions(&mut self, now_ms: i64) -> Result<usize, StorageError> {
        let recovered = self.write_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT pk, start_ts_ms, end_ts_ms FROM sessions WHERE status = 'active'",
            )?;
            let stale: Vec<(i64, i64, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            if stale.is_empty() {
                return Ok(0);
            }

            for (session_pk, start_ts, end_ts) in &stale {
                let max_ep_ts: Option<i64> = tx
                    .query_row(
                        "SELECT MAX(ts_ms) FROM session_epochs WHERE session_pk = ?1",
                        params![session_pk],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                let final_end_ts = (*end_ts).max(*start_ts).max(max_ep_ts.unwrap_or(*start_ts));
                tx.execute(
                    "UPDATE sessions
                     SET status = 'closed',
                         end_ts_ms = ?2,
                         end_reason = COALESCE(end_reason, 'stream_ended')
                     WHERE pk = ?1",
                    params![session_pk, final_end_ts],
                )?;
            }

            recompute_and_persist_in_tx(tx, now_ms)?;
            Ok(stale.len())
        })?;
        if recovered > 0 {
            self.flush_wal()?;
        }
        Ok(recovered)
    }

    /// Persists pre-trigger `sleeping_ts_ms` and `trigger_power_action` on an active session
    /// and flushes the WAL so an immediate suspend or power-off cannot drop trigger metadata.
    pub fn mark_session_sleep_trigger(
        &mut self,
        session_id: &str,
        sleeping_ts_ms: i64,
        trigger_power_action: PowerAction,
    ) -> Result<(), StorageError> {
        self.write_tx_and_flush(|tx| {
            tx.execute(
                "UPDATE sessions
                 SET sleeping_ts_ms = COALESCE(sleeping_ts_ms, ?2),
                     trigger_power_action = ?3
                 WHERE id = ?1 AND status = 'active'",
                params![session_id, sleeping_ts_ms, trigger_power_action.as_str()],
            )?;
            Ok(())
        })
    }

    /// Deletes an active incomplete-Warmup `Monitoring Session` without altering calibration.
    pub fn discard_incomplete_session(&mut self, session_id: &str) -> Result<bool, StorageError> {
        let deleted = self.write_tx(|tx| {
            let rows = tx.execute(
                "DELETE FROM sessions WHERE id = ?1 AND status = 'active'",
                params![session_id],
            )?;
            Ok(rows > 0)
        })?;
        if deleted {
            self.reclaim_freelist_pages()?;
        }
        self.flush_wal()?;
        Ok(deleted)
    }
}
