//! Monitoring Session persistence, epoch queries, raw samples, and session feedback.

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
        self.write_tx_and_flush(move |tx| {
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
            let delta_hr_onset = s.compute_delta_hr_onset();

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
                     pre_candidate_mu_hr = ?9,
                     pre_candidate_sigma_hr = ?10,
                     pre_candidate_ln_rmssd = ?11,
                     onset_candidate_ts_ms = ?12,
                     onset_confirmed_ts_ms = ?13,
                     sleeping_ts_ms = ?14,
                     sleep_mu_hr = ?15,
                     onset_hr_bpm = ?16,
                     delta_hr_onset = ?17,
                     sleep_ln_rmssd = ?18,
                     sleep_hr_var_ratio = ?19,
                     typical_sqi = ?20,
                     post_trigger_sleep_sec = ?21,
                     trigger_power_action = ?22,
                     rapid_resume_within_5m = ?23,
                     grace_cancel_count = ?24,
                     rapid_wake_fp_count = ?25,
                     snooze_count = ?26,
                     auto_anomalous = ?27
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
                    s.pre_candidate_mu_hr,
                    s.pre_candidate_sigma_hr,
                    s.pre_candidate_ln_rmssd,
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
            let _pruned = super::maintenance::prune_retention_in_tx(tx, &retention, now_ms)?;
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
        self.write_tx_and_flush(|tx| {
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
        let (session_id, state) = self.write_tx(|tx| {
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
            let state = recompute_and_persist_in_tx(tx, now_ms)?;
            Ok((session_id, state))
        })?;

        self.reclaim_freelist_pages()?;
        self.flush_wal()?;
        Ok((session_id, state))
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
                pre_candidate_mu_hr, pre_candidate_sigma_hr, pre_candidate_ln_rmssd,
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
            let status_str: String = row.get(31)?;
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
    let power_str: String = row.get(23)?;
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
        pre_candidate_mu_hr: row.get(10)?,
        pre_candidate_sigma_hr: row.get(11)?,
        pre_candidate_ln_rmssd: row.get(12)?,
        onset_candidate_ts_ms: row.get(13)?,
        onset_confirmed_ts_ms: row.get(14)?,
        sleeping_ts_ms: row.get(15)?,
        sleep_mu_hr: row.get(16)?,
        onset_hr_bpm: row.get(17)?,
        delta_hr_onset: row.get(18)?,
        sleep_ln_rmssd: row.get(19)?,
        sleep_hr_var_ratio: row.get(20)?,
        typical_sqi: row.get(21)?,
        post_trigger_sleep_sec: row.get(22)?,
        trigger_power_action: power_str.parse().unwrap_or(PowerAction::None),
        rapid_resume_within_5m: row.get::<_, i64>(24)? != 0,
        grace_cancel_count: row.get::<_, u8>(25)?,
        rapid_wake_fp_count: row.get::<_, u8>(26)?,
        snooze_count: row.get::<_, u8>(27)?,
        auto_anomalous: row.get::<_, i64>(28)? != 0,
        epochs_count: row.get(29)?,
        raw_samples_count: row.get(30)?,
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
    crate::calibration::types::resolve_onset_hhmm_ms(start_ts_ms, end_ts_ms, onset)
        .map_err(StorageError::Validation)
}
