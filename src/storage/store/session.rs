//! Monitoring Session persistence, epoch queries, raw samples, and session feedback.

use std::os::unix::fs::PermissionsExt;

use chrono::{Duration, Local, LocalResult, NaiveDateTime, TimeZone};
use rusqlite::{Connection, OptionalExtension, params};

use crate::calibration::{
    CalibrationState, FeedbackVerdict, SessionFeedbackRecord, SessionLedgerRecord,
    validate_sleep_hr_bpm,
};
use crate::config::{ClockTime, PatchField, PowerAction, Sensitivity};
use crate::logging::mask_mac;
use crate::sleep_engine::{
    ContactStatus, DetectorState, Epoch, EpochFeatures, HrPacket, SessionEndReason,
};
use crate::storage::schema::generate_uuid_v7_with;
use crate::storage::{
    CloseSessionRequest, OpenSessionRequest, SessionFullDetail, SessionListEntry, SessionListPage,
    SessionReplayBundle, SessionStatus, SetFeedbackRequest, StorageError, WearableInfo,
};

use super::recompute_and_persist_in_tx;
use super::{LedgerStore, resolve_table_id_prefix};

impl LedgerStore {
    /// Recovers stale `'active'` sessions left behind by an ungraceful daemon termination.
    pub fn recover_stale_sessions(&mut self, now_ms: i64) -> Result<usize, StorageError> {
        self.write_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT id, start_ts_ms, end_ts_ms FROM sessions WHERE status = 'active'",
            )?;
            let stale: Vec<(String, i64, i64)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(stmt);

            if stale.is_empty() {
                return Ok(0);
            }

            for (id, start_ts, end_ts) in &stale {
                let max_ep_ts: Option<i64> = tx
                    .query_row(
                        "SELECT MAX(ts_ms) FROM session_epochs WHERE session_id = ?1",
                        params![id],
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
                     WHERE id = ?1",
                    params![id, final_end_ts],
                )?;
            }

            recompute_and_persist_in_tx(tx, now_ms)?;
            Ok(stale.len())
        })
    }

    /// Persists a newly opened `'active'` `Monitoring Session` and returns its UUIDv7 ID.
    pub fn open_session(&mut self, req: &OpenSessionRequest) -> Result<String, StorageError> {
        let session_id = generate_uuid_v7_with(&self.uuid_ctx, req.start_ts_ms);
        self.write_tx(|tx| {
            let exists: Option<String> = tx
                .query_row(
                    "SELECT id FROM wearables WHERE id = ?1",
                    params![req.wearable_id],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(StorageError::NotFound(format!(
                    "wearable '{}' not found",
                    req.wearable_id
                )));
            }

            tx.execute(
                "INSERT INTO sessions (
                     id, wearable_id, status, start_ts_ms, end_ts_ms, end_reason,
                     detection_sensitivity
                 ) VALUES (?1, ?2, 'active', ?3, ?3, NULL, ?4)",
                params![
                    session_id,
                    req.wearable_id,
                    req.start_ts_ms,
                    req.detection_sensitivity.as_str()
                ],
            )?;
            Ok(session_id)
        })
    }

    /// Appends one `15 s` `Epoch` and/or buffered `1 Hz` `raw_samples` to an active session.
    pub fn append_epoch_and_raw(
        &mut self,
        session_id: &str,
        epoch: Option<(u32, &Epoch)>,
        raw_samples: &[(u32, &HrPacket)],
    ) -> Result<(), StorageError> {
        if epoch.is_none() && raw_samples.is_empty() {
            return Ok(());
        }

        let ep_id = epoch.map(|(_, ep)| generate_uuid_v7_with(&self.uuid_ctx, ep.ts_ms));
        let raw_ids: Vec<String> = raw_samples
            .iter()
            .map(|(_, pkt)| generate_uuid_v7_with(&self.uuid_ctx, pkt.ts_ms))
            .collect();

        self.write_tx(|tx| {
            let status: Option<String> = tx
                .query_row(
                    "SELECT status FROM sessions WHERE id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .optional()?;
            if status.is_none() {
                return Err(StorageError::NotFound(format!(
                    "session '{session_id}' not found"
                )));
            }

            if let (Some((seq, ep)), Some(ep_id)) = (epoch, ep_id) {
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO session_epochs (
                         id, session_id, seq, ts_ms, hr_bpm_60s, z_hr, hr_slope_4m, r_var_120s,
                         ln_rmssd_60s, delta_ln_rmssd_60s, rmssd_sdnn_120s, hrv_valid,
                         rr_artifact_frac, coverage, flatline_stale, accumulators_frozen,
                         baseline_mu_hr, baseline_sigma_hr, baseline_frozen,
                         p_active_wake, p_quiet_wake, p_sleep, state
                     ) VALUES (
                         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                         ?9, ?10, ?11, ?12,
                         ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19,
                         ?20, ?21, ?22, ?23
                     )",
                    params![
                        ep_id,
                        session_id,
                        seq,
                        ep.ts_ms,
                        ep.features.hr_bpm_60s,
                        ep.features.z_hr,
                        ep.features.hr_slope_4m,
                        ep.features.r_var_120s,
                        ep.features.ln_rmssd_60s,
                        ep.features.delta_ln_rmssd_60s,
                        ep.features.rmssd_sdnn_120s,
                        i32::from(ep.hrv_valid),
                        ep.rr_artifact_frac,
                        ep.coverage,
                        i32::from(ep.flatline_stale),
                        i32::from(ep.accumulators_frozen),
                        ep.baseline_mu_hr,
                        ep.baseline_sigma_hr,
                        i32::from(ep.baseline_frozen),
                        ep.p_active_wake,
                        ep.p_quiet_wake,
                        ep.p_sleep,
                        ep.state.as_str(),
                    ],
                )?;
                if inserted > 0 {
                    tx.execute(
                        "UPDATE sessions
                         SET epochs_count = epochs_count + 1,
                             end_ts_ms = MAX(end_ts_ms, ?2),
                             had_rr = had_rr OR ?3
                         WHERE id = ?1",
                        params![
                            session_id,
                            ep.ts_ms,
                            i32::from(ep.hrv_valid || ep.features.ln_rmssd_60s.is_some())
                        ],
                    )?;
                }
            }

            if !raw_samples.is_empty() {
                let mut raw_stmt = tx.prepare(
                    "INSERT OR IGNORE INTO raw_samples (
                         id, session_id, seq, ts_ms, bpm, rr_ms_json, contact_status
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )?;
                let mut inserted_count = 0_u32;
                let mut max_raw_ts = i64::MIN;
                let mut saw_rr = false;
                for ((seq, pkt), raw_id) in raw_samples.iter().zip(raw_ids.iter()) {
                    let rr_json = serde_json::to_string(&pkt.rr_ms)?;
                    let inserted = raw_stmt.execute(params![
                        raw_id,
                        session_id,
                        seq,
                        pkt.ts_ms,
                        pkt.bpm,
                        rr_json,
                        pkt.contact_status.as_str(),
                    ])?;
                    if inserted > 0 {
                        inserted_count += 1;
                        max_raw_ts = max_raw_ts.max(pkt.ts_ms);
                        saw_rr |= !pkt.rr_ms.is_empty();
                    }
                }
                drop(raw_stmt);

                if inserted_count > 0 {
                    tx.execute(
                        "UPDATE sessions
                         SET raw_samples_count = raw_samples_count + ?2,
                             end_ts_ms = MAX(end_ts_ms, ?3),
                             had_rr = had_rr OR ?4
                         WHERE id = ?1",
                        params![session_id, inserted_count, max_raw_ts, i32::from(saw_rr)],
                    )?;
                }
            }

            Ok(())
        })
    }

    /// Finalizes a closed `Monitoring Session` and deterministically recomputes `calibration_state`.
    pub fn close_session_and_recompute(
        &mut self,
        req: &CloseSessionRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        let retention = self.retention_config.clone();
        self.write_tx(move |tx| {
            let exists: Option<String> = tx
                .query_row(
                    "SELECT status FROM sessions WHERE id = ?1",
                    params![req.session_id],
                    |r| r.get(0),
                )
                .optional()?;
            if exists.is_none() {
                return Err(StorageError::NotFound(format!(
                    "session '{}' not found",
                    req.session_id
                )));
            }

            let s = &req.summary;
            let delta_hr_onset = s
                .sleep_mu_hr
                .or(s.onset_hr_bpm)
                .zip(s.warmup_mu_hr)
                .map(|(sleep_or_onset, wake)| sleep_or_onset - wake);

            tx.execute(
                "UPDATE sessions
                 SET status = 'closed',
                     start_ts_ms = ?2,
                     end_ts_ms = ?3,
                     end_reason = ?4,
                     had_rr = had_rr OR ?5,
                     warmup_mu_hr = ?6,
                     warmup_sigma_hr = ?7,
                     warmup_ln_rmssd = ?8,
                     onset_candidate_ts_ms = ?9,
                     onset_confirmed_ts_ms = ?10,
                     sleeping_ts_ms = ?11,
                     sleep_mu_hr = ?12,
                     onset_hr_bpm = ?13,
                     delta_hr_onset = ?14,
                     sleep_ln_rmssd = ?15,
                     sleep_hr_var_ratio = ?16,
                     typical_sqi = ?17,
                     post_trigger_sleep_sec = ?18,
                     trigger_power_action = ?19,
                     rapid_resume_within_5m = ?20,
                     grace_cancel_count = ?21,
                     rapid_wake_fp_count = ?22,
                     snooze_count = ?23,
                     auto_anomalous = ?24
                 WHERE id = ?1",
                params![
                    req.session_id,
                    s.start_ts_ms,
                    s.end_ts_ms,
                    s.end_reason.as_str(),
                    i32::from(s.had_rr),
                    s.warmup_mu_hr,
                    s.warmup_sigma_hr,
                    s.warmup_ln_rmssd,
                    s.candidate_ts_ms,
                    s.confirmed_ts_ms,
                    s.sleeping_ts_ms,
                    s.sleep_mu_hr,
                    s.onset_hr_bpm,
                    delta_hr_onset,
                    s.sleep_ln_rmssd,
                    s.observed_hr_var_ratio,
                    s.typical_sqi,
                    s.post_trigger_sleep_sec,
                    req.trigger_power_action.as_str(),
                    i32::from(req.rapid_resume_within_5m),
                    i32::from(s.grace_cancel_count),
                    i32::from(s.rapid_wake_fp_count),
                    i32::from(s.snooze_count),
                    i32::from(s.anomalous),
                ],
            )?;

            let state = recompute_and_persist_in_tx(tx, now_ms)?;
            let _ = super::prune_retention_in_tx(tx, &retention, now_ms)?;
            let detail = load_session_detail_in_conn(tx, &req.session_id)?;
            Ok((detail, state))
        })
    }

    /// Lists sessions newest-first with pagination.
    pub fn list_sessions(&self, limit: u32, offset: u32) -> Result<SessionListPage, StorageError> {
        if limit == 0 {
            return Err(StorageError::Validation(
                "sessions list limit must be >= 1".to_string(),
            ));
        }

        let total_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;

        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.wearable_id, w.alias, w.mac, s.status,
                    s.start_ts_ms, s.end_ts_ms, s.end_reason, s.detection_sensitivity,
                    s.had_rr, s.warmup_mu_hr, s.sleep_mu_hr, s.onset_confirmed_ts_ms,
                    s.sleeping_ts_ms, s.auto_anomalous, s.grace_cancel_count, s.rapid_wake_fp_count,
                    s.rapid_resume_within_5m, s.epochs_count, s.raw_samples_count,
                    f.id, f.verdict, f.manual_onset_ts_ms, f.manual_avg_sleep_hr, f.notes, f.updated_at_ms
             FROM sessions s
             JOIN wearables w ON w.id = s.wearable_id
             LEFT JOIN session_feedback f ON f.session_id = s.id
             ORDER BY s.start_ts_ms DESC, s.id DESC
             LIMIT ?1 OFFSET ?2",
        )?;

        let rows = stmt
            .query_map(params![limit, offset], |row| {
                let id: String = row.get(0)?;
                let mac: String = row.get(3)?;
                let status_str: String = row.get(4)?;
                let end_reason_str: Option<String> = row.get(7)?;
                let sens_str: String = row.get(8)?;
                let grace_fp: i64 = row.get(15)?;
                let rapid_fp: i64 = row.get(16)?;
                let rapid_resume: i64 = row.get(17)?;
                let fp_count = u8::from(grace_fp > 0 || rapid_fp > 0 || rapid_resume != 0);
                let feedback = map_optional_session_feedback_row(row, 20, id.clone())?;

                Ok(SessionListEntry {
                    id,
                    wearable_id: row.get(1)?,
                    wearable_alias: row.get(2)?,
                    wearable_mac_masked: mask_mac(&mac),
                    status: status_str.parse().unwrap_or(SessionStatus::Closed),
                    start_ts_ms: row.get(5)?,
                    end_ts_ms: row.get(6)?,
                    end_reason: end_reason_str.and_then(|s| s.parse().ok()),
                    detection_sensitivity: sens_str.parse().unwrap_or(Sensitivity::Balanced),
                    had_rr: row.get::<_, i64>(9)? != 0,
                    warmup_mu_hr: row.get(10)?,
                    sleep_mu_hr: row.get(11)?,
                    onset_confirmed_ts_ms: row.get(12)?,
                    sleeping_ts_ms: row.get(13)?,
                    auto_anomalous: row.get::<_, i64>(14)? != 0,
                    fp_count,
                    epochs_count: row.get::<_, u32>(18)?,
                    raw_samples_count: row.get::<_, u32>(19)?,
                    feedback,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(SessionListPage {
            sessions: rows,
            total_count: total_count.max(0) as u64,
            limit,
            offset,
        })
    }

    /// Loads detailed session inspection data by exact UUIDv7 or unambiguous prefix.
    pub fn get_session(&self, id_or_prefix: &str) -> Result<SessionFullDetail, StorageError> {
        let session_id = resolve_session_id(&self.conn, id_or_prefix)?;
        load_session_detail_in_conn(&self.conn, &session_id)
    }

    /// Loads stored raw samples and/or 15-second `Epochs` read-only for `bpmute replay --session`.
    pub fn load_session_for_replay(
        &self,
        id_or_prefix: &str,
    ) -> Result<SessionReplayBundle, StorageError> {
        let session_id = resolve_session_id(&self.conn, id_or_prefix)?;
        let (record, _status) = load_session_record(&self.conn, &session_id)?;
        let raw_samples = load_raw_samples(&self.conn, &session_id)?;
        let epochs = load_session_epochs(&self.conn, &session_id)?;
        let state = self.get_calibration_state()?;
        let wearable_floor = state.wearable_floor(&record.wearable_id);
        let priors = state.to_priors();

        Ok(SessionReplayBundle {
            session: record,
            wearable_floor,
            priors,
            raw_samples,
            epochs,
        })
    }

    /// Edits feedback for a closed session and recomputes `calibration_state` atomically.
    pub fn set_feedback_and_recompute(
        &mut self,
        req: &SetFeedbackRequest,
        now_ms: i64,
    ) -> Result<(SessionFullDetail, CalibrationState), StorageError> {
        if let PatchField::Set(hr) = req.avg_sleep_hr {
            validate_sleep_hr_bpm("avg_sleep_hr", hr).map_err(StorageError::Validation)?;
        }

        let new_fb_id = generate_uuid_v7_with(&self.uuid_ctx, now_ms);
        self.write_tx(|tx| {
            let session_id = resolve_session_id(tx, &req.session_id_or_prefix)?;
            let (status, start_ts_ms, end_ts_ms): (String, i64, i64) = tx.query_row(
                "SELECT status, start_ts_ms, end_ts_ms FROM sessions WHERE id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            if status != "closed" {
                return Err(StorageError::PreconditionFailed(
                    "cannot record feedback for an active Monitoring Session".to_string(),
                ));
            }

            if req.clear_all {
                tx.execute(
                    "DELETE FROM session_feedback WHERE session_id = ?1",
                    params![session_id],
                )?;
            } else {
                let existing = load_session_feedback(tx, &session_id)?;
                let mut fb = existing.unwrap_or_else(|| SessionFeedbackRecord {
                    id: new_fb_id,
                    session_id: session_id.clone(),
                    verdict: None,
                    manual_onset_ts_ms: None,
                    manual_avg_sleep_hr: None,
                    notes: None,
                    updated_at_ms: now_ms,
                });

                match req.verdict {
                    PatchField::Unchanged => {}
                    PatchField::Clear => fb.verdict = None,
                    PatchField::Set(v) => fb.verdict = Some(v),
                }
                match req.onset_hhmm {
                    PatchField::Unchanged => {}
                    PatchField::Clear => fb.manual_onset_ts_ms = None,
                    PatchField::Set(clock) => {
                        let resolved = resolve_onset_hhmm_ms(start_ts_ms, end_ts_ms, clock)?;
                        fb.manual_onset_ts_ms = Some(resolved);
                    }
                }
                match req.avg_sleep_hr {
                    PatchField::Unchanged => {}
                    PatchField::Clear => fb.manual_avg_sleep_hr = None,
                    PatchField::Set(hr) => fb.manual_avg_sleep_hr = Some(hr),
                }
                fb.updated_at_ms = now_ms;

                if fb.verdict.is_none()
                    && fb.manual_onset_ts_ms.is_none()
                    && fb.manual_avg_sleep_hr.is_none()
                    && fb.notes.is_none()
                {
                    tx.execute(
                        "DELETE FROM session_feedback WHERE session_id = ?1",
                        params![session_id],
                    )?;
                } else {
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
                            fb.verdict.map(FeedbackVerdict::as_str),
                            fb.manual_onset_ts_ms,
                            fb.manual_avg_sleep_hr,
                            fb.notes,
                            fb.updated_at_ms,
                        ],
                    )?;
                }
            }

            let state = recompute_and_persist_in_tx(tx, now_ms)?;
            let detail = load_session_detail_in_conn(tx, &session_id)?;
            Ok((detail, state))
        })
    }

    /// Deletes a closed `Monitoring Session` and deterministically recomputes `calibration_state`.
    /// Rejects deletion of an active session with `StorageError::PreconditionFailed` (exit code `4`).
    pub fn delete_session_and_recompute(
        &mut self,
        id_or_prefix: &str,
        now_ms: i64,
    ) -> Result<(String, CalibrationState), StorageError> {
        self.write_tx(|tx| {
            let session_id = resolve_session_id(tx, id_or_prefix)?;
            let status: String = tx.query_row(
                "SELECT status FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            if status == "active" {
                return Err(StorageError::PreconditionFailed(
                    "cannot delete an active session".to_string(),
                ));
            }

            tx.execute("DELETE FROM sessions WHERE id = ?1", params![session_id])?;
            let _ = tx.execute("PRAGMA incremental_vacuum(500);", []);
            let state = recompute_and_persist_in_tx(tx, now_ms)?;
            Ok((session_id, state))
        })
    }

    /// Exports a session trace to canonical `CSV` or `JSONL` format directly replayable by `bpmute replay`.
    /// Anonymizes by default (stripping Wearable MAC/name and shifting timestamps so `session.start_ts_ms` is `0`).
    pub fn export_trace(
        &self,
        id_or_prefix: &str,
        dest_path: &std::path::Path,
        format: crate::replay::TraceFormat,
        anonymize: bool,
    ) -> Result<usize, StorageError> {
        let session_id = resolve_session_id(&self.conn, id_or_prefix)?;
        let start_ts_ms: i64 = self.conn.query_row(
            "SELECT start_ts_ms FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;

        let raw_samples = load_raw_samples(&self.conn, &session_id)?;
        let epochs = if raw_samples.is_empty() {
            load_session_epochs(&self.conn, &session_id)?
        } else {
            Vec::new()
        };

        if raw_samples.is_empty() && epochs.is_empty() {
            return Err(StorageError::NotFound(format!(
                "no trace data available for session '{session_id}'"
            )));
        }

        if let Some(parent) = dest_path.parent()
            && !parent.as_os_str().is_empty()
        {
            crate::paths::ensure_private_dir(parent)?;
        }

        use std::fs::OpenOptions;
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dest_path)?;

        let mut count = 0;
        let time_shift = if anonymize { start_ts_ms } else { 0 };

        match format {
            crate::replay::TraceFormat::Csv => {
                writeln!(file, "ts_ms,bpm,rr_ms,contact_status,event")?;
                if !raw_samples.is_empty() {
                    for pkt in &raw_samples {
                        let ts = pkt.ts_ms - time_shift;
                        let rr_str = if pkt.rr_ms.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "\"{}\"",
                                pkt.rr_ms
                                    .iter()
                                    .map(|v| format!("{v:.1}"))
                                    .collect::<Vec<_>>()
                                    .join(";")
                            )
                        };
                        writeln!(
                            file,
                            "{},{:.1},{},{},",
                            ts,
                            pkt.bpm,
                            rr_str,
                            pkt.contact_status.as_str()
                        )?;
                        count += 1;
                    }
                } else {
                    for ep in &epochs {
                        let ts = ep.ts_ms - time_shift;
                        writeln!(file, "{},{:.1},,,", ts, ep.features.hr_bpm_60s)?;
                        count += 1;
                    }
                }
            }
            crate::replay::TraceFormat::Jsonl => {
                if !raw_samples.is_empty() {
                    for pkt in &raw_samples {
                        let ts = pkt.ts_ms - time_shift;
                        let row = serde_json::json!({
                            "ts_ms": ts,
                            "bpm": pkt.bpm,
                            "rr_ms": pkt.rr_ms,
                            "contact_status": pkt.contact_status.as_str(),
                        });
                        serde_json::to_writer(&mut file, &row)?;
                        writeln!(file)?;
                        count += 1;
                    }
                } else {
                    for ep in &epochs {
                        let ts = ep.ts_ms - time_shift;
                        let row = serde_json::json!({
                            "ts_ms": ts,
                            "bpm": ep.features.hr_bpm_60s,
                        });
                        serde_json::to_writer(&mut file, &row)?;
                        writeln!(file)?;
                        count += 1;
                    }
                }
            }
            crate::replay::TraceFormat::SqliteLedger => {
                return Err(StorageError::Validation(
                    "sqlite_ledger format cannot be exported as a trace file".to_string(),
                ));
            }
        }

        let _ = std::fs::set_permissions(dest_path, std::fs::Permissions::from_mode(0o600));
        Ok(count)
    }
}

pub(crate) fn resolve_session_id(
    conn: &Connection,
    id_or_prefix: &str,
) -> Result<String, StorageError> {
    resolve_table_id_prefix(conn, "sessions", "session", id_or_prefix)
}

pub(crate) fn load_session_record(
    conn: &Connection,
    session_id: &str,
) -> Result<(SessionLedgerRecord, SessionStatus), StorageError> {
    conn.query_row(
        "SELECT id, wearable_id, start_ts_ms, end_ts_ms, end_reason, detection_sensitivity,
                had_rr, warmup_mu_hr, warmup_sigma_hr, warmup_ln_rmssd,
                onset_candidate_ts_ms, onset_confirmed_ts_ms, sleeping_ts_ms,
                sleep_mu_hr, onset_hr_bpm, delta_hr_onset, sleep_ln_rmssd,
                sleep_hr_var_ratio, typical_sqi, post_trigger_sleep_sec,
                trigger_power_action, rapid_resume_within_5m, grace_cancel_count,
                rapid_wake_fp_count, snooze_count, auto_anomalous,
                epochs_count, raw_samples_count, status
         FROM sessions WHERE id = ?1",
        params![session_id],
        |row| {
            let record = map_session_ledger_row(row)?;
            let status_str: String = row.get(28)?;
            let status = status_str.parse().unwrap_or(SessionStatus::Closed);
            Ok((record, status))
        },
    )
    .optional()?
    .ok_or_else(|| StorageError::NotFound(format!("session '{session_id}' not found")))
}

pub(crate) fn load_session_detail_in_conn(
    conn: &Connection,
    session_id: &str,
) -> Result<SessionFullDetail, StorageError> {
    let (session, status) = load_session_record(conn, session_id)?;
    let wearable = conn.query_row(
        "SELECT id, mac, alias, advertised_name, supports_rr, supports_contact, hr_var_floor, typical_sqi
         FROM wearables WHERE id = ?1",
        params![session.wearable_id],
        |row| {
            let mac: String = row.get(1)?;
            Ok(WearableInfo {
                id: row.get(0)?,
                mac_masked: mask_mac(&mac),
                alias: row.get(2)?,
                advertised_name: row.get(3)?,
                supports_rr: row.get::<_, i64>(4)? != 0,
                supports_contact: row.get::<_, i64>(5)? != 0,
                hr_var_floor: row.get(6)?,
                typical_sqi: row.get(7)?,
            })
        },
    )?;
    let feedback = load_session_feedback(conn, session_id)?;
    let epochs = load_session_epochs(conn, session_id)?;
    let raw_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM raw_samples WHERE session_id = ?1",
        params![session_id],
        |r| r.get(0),
    )?;

    Ok(SessionFullDetail {
        session,
        status,
        wearable,
        feedback,
        epochs,
        has_raw_samples: raw_count > 0,
    })
}

pub(crate) fn load_session_feedback(
    conn: &Connection,
    session_id: &str,
) -> Result<Option<SessionFeedbackRecord>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT id, verdict, manual_onset_ts_ms, manual_avg_sleep_hr, notes, updated_at_ms
             FROM session_feedback WHERE session_id = ?1",
            params![session_id],
            |row| map_optional_session_feedback_row(row, 0, session_id.to_string()),
        )
        .optional()?
        .flatten())
}

pub(crate) fn load_session_epochs(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<Epoch>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT ts_ms, hr_bpm_60s, z_hr, hr_slope_4m, r_var_120s,
                ln_rmssd_60s, delta_ln_rmssd_60s, rmssd_sdnn_120s, hrv_valid,
                rr_artifact_frac, coverage, flatline_stale, accumulators_frozen,
                baseline_mu_hr, baseline_sigma_hr, baseline_frozen,
                p_active_wake, p_quiet_wake, p_sleep, state
         FROM session_epochs
         WHERE session_id = ?1
         ORDER BY seq ASC",
    )?;
    let epochs = stmt
        .query_map(params![session_id], |row| {
            let state_str: String = row.get(19)?;
            Ok(Epoch {
                session_index: 1,
                ts_ms: row.get(0)?,
                features: EpochFeatures {
                    hr_bpm_60s: row.get(1)?,
                    z_hr: row.get(2)?,
                    hr_slope_4m: row.get(3)?,
                    r_var_120s: row.get(4)?,
                    ln_rmssd_60s: row.get(5)?,
                    delta_ln_rmssd_60s: row.get(6)?,
                    rmssd_sdnn_120s: row.get(7)?,
                },
                hrv_valid: row.get::<_, i64>(8)? != 0,
                rr_artifact_frac: row.get(9)?,
                coverage: row.get(10)?,
                flatline_stale: row.get::<_, i64>(11)? != 0,
                accumulators_frozen: row.get::<_, i64>(12)? != 0,
                baseline_mu_hr: row.get(13)?,
                baseline_sigma_hr: row.get(14)?,
                baseline_frozen: row.get::<_, i64>(15)? != 0,
                p_active_wake: row.get(16)?,
                p_quiet_wake: row.get(17)?,
                p_sleep: row.get(18)?,
                state: state_str.parse().unwrap_or(DetectorState::Idle),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(epochs)
}

pub(crate) fn load_raw_samples(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<HrPacket>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT ts_ms, bpm, rr_ms_json, contact_status
         FROM raw_samples
         WHERE session_id = ?1
         ORDER BY seq ASC",
    )?;
    let rows = stmt
        .query_map(params![session_id], |row| {
            let ts_ms: i64 = row.get(0)?;
            let bpm: f64 = row.get(1)?;
            let rr_json: String = row.get(2)?;
            let contact_str: String = row.get(3)?;
            Ok((ts_ms, bpm, rr_json, contact_str))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut packets = Vec::with_capacity(rows.len());
    for (ts_ms, bpm, rr_json, contact_str) in rows {
        let rr_ms: Vec<f64> = serde_json::from_str(&rr_json)?;
        packets.push(HrPacket {
            ts_ms,
            bpm,
            rr_ms,
            contact_status: contact_str.parse().unwrap_or(ContactStatus::Unsupported),
        });
    }
    Ok(packets)
}

pub(crate) fn map_session_ledger_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SessionLedgerRecord> {
    let end_reason_str: Option<String> = row.get(4)?;
    let sens_str: String = row.get(5)?;
    let power_str: String = row.get(20)?;
    Ok(SessionLedgerRecord {
        id: row.get(0)?,
        wearable_id: row.get(1)?,
        start_ts_ms: row.get(2)?,
        end_ts_ms: row.get(3)?,
        end_reason: end_reason_str
            .and_then(|s| s.parse().ok())
            .unwrap_or(SessionEndReason::StreamEnded),
        detection_sensitivity: sens_str.parse().unwrap_or(Sensitivity::Balanced),
        had_rr: row.get::<_, i64>(6)? != 0,
        warmup_mu_hr: row.get(7)?,
        warmup_sigma_hr: row.get(8)?,
        warmup_ln_rmssd: row.get(9)?,
        onset_candidate_ts_ms: row.get(10)?,
        onset_confirmed_ts_ms: row.get(11)?,
        sleeping_ts_ms: row.get(12)?,
        sleep_mu_hr: row.get(13)?,
        onset_hr_bpm: row.get(14)?,
        delta_hr_onset: row.get(15)?,
        sleep_ln_rmssd: row.get(16)?,
        sleep_hr_var_ratio: row.get(17)?,
        typical_sqi: row.get(18)?,
        post_trigger_sleep_sec: row.get(19)?,
        trigger_power_action: power_str.parse().unwrap_or(PowerAction::None),
        rapid_resume_within_5m: row.get::<_, i64>(21)? != 0,
        grace_cancel_count: row.get::<_, u8>(22)?,
        rapid_wake_fp_count: row.get::<_, u8>(23)?,
        snooze_count: row.get::<_, u8>(24)?,
        auto_anomalous: row.get::<_, i64>(25)? != 0,
        epochs_count: row.get(26)?,
        raw_samples_count: row.get(27)?,
    })
}

pub(crate) fn map_optional_session_feedback_row(
    row: &rusqlite::Row<'_>,
    start_col: usize,
    session_id: String,
) -> rusqlite::Result<Option<SessionFeedbackRecord>> {
    let fb_id: Option<String> = row.get(start_col)?;
    let Some(id) = fb_id else {
        return Ok(None);
    };
    let verdict_str: Option<String> = row.get(start_col + 1)?;
    Ok(Some(SessionFeedbackRecord {
        id,
        session_id,
        verdict: verdict_str.and_then(|s| s.parse().ok()),
        manual_onset_ts_ms: row.get(start_col + 2)?,
        manual_avg_sleep_hr: row.get(start_col + 3)?,
        notes: row.get(start_col + 4)?,
        updated_at_ms: row.get(start_col + 5)?,
    }))
}

pub(crate) fn resolve_onset_hhmm_ms(
    start_ts_ms: i64,
    end_ts_ms: i64,
    onset: ClockTime,
) -> Result<i64, StorageError> {
    let start_local = match Local.timestamp_millis_opt(start_ts_ms) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(dt, _) => dt,
        LocalResult::None => {
            return Err(StorageError::Validation(
                "session start timestamp out of local range".to_string(),
            ));
        }
    };
    let end_local = match Local.timestamp_millis_opt(end_ts_ms) {
        LocalResult::Single(dt) => dt,
        LocalResult::Ambiguous(dt, _) => dt,
        LocalResult::None => start_local + Duration::hours(12),
    };

    let target_time = onset.to_naive_time();
    let mut date = start_local
        .date_naive()
        .pred_opt()
        .unwrap_or_else(|| start_local.date_naive());
    let limit_date = end_local
        .date_naive()
        .succ_opt()
        .unwrap_or_else(|| end_local.date_naive());

    while date <= limit_date {
        let naive = NaiveDateTime::new(date, target_time);
        let candidate_dt = match Local.from_local_datetime(&naive) {
            LocalResult::Single(dt) => Some(dt),
            LocalResult::Ambiguous(earliest, latest) => {
                let e_ms = earliest.timestamp_millis();
                if e_ms >= start_ts_ms && e_ms <= end_ts_ms {
                    Some(earliest)
                } else {
                    Some(latest)
                }
            }
            LocalResult::None => None,
        };
        if let Some(dt) = candidate_dt {
            let ts = dt.timestamp_millis();
            if ts >= start_ts_ms && ts <= end_ts_ms {
                return Ok(ts);
            }
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }

    Err(StorageError::Validation(format!(
        "manual onset time {onset} falls outside session bounds"
    )))
}
