//! Portable `Ledger Bundle` export and import (`Merge` / `Replace`) against SQLite `v2`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::OpenOptions;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::{OptionalExtension, params};

use crate::calibration::{
    BoundaryReason, CalibrationBoundaryRecord, CalibrationState, SessionFeedbackRecord,
};
use crate::paths::ensure_private_dir;
use crate::sleep_engine::{ContactStatus, HrPacket};
use crate::storage::StorageError;
use crate::storage::bundle::*;
use crate::storage::raw_codec::{
    RAW_CHUNK_MAX_SAMPLES, decode_raw_chunk, encode_raw_chunk_with_bounds,
};
use crate::storage::schema::backups_dir_in_data_dir;

use super::{LedgerStore, recompute_and_persist_in_tx};

impl LedgerStore {
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
                    warmup_ln_rmssd, pre_candidate_mu_hr, pre_candidate_sigma_hr,
                    pre_candidate_ln_rmssd, onset_candidate_ts_ms, onset_confirmed_ts_ms,
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
                    pre_candidate_mu_hr: row.get(11)?,
                    pre_candidate_sigma_hr: row.get(12)?,
                    pre_candidate_ln_rmssd: row.get(13)?,
                    onset_candidate_ts_ms: row.get(14)?,
                    onset_confirmed_ts_ms: row.get(15)?,
                    sleeping_ts_ms: row.get(16)?,
                    sleep_mu_hr: row.get(17)?,
                    onset_hr_bpm: row.get(18)?,
                    delta_hr_onset: row.get(19)?,
                    sleep_ln_rmssd: row.get(20)?,
                    sleep_hr_var_ratio: row.get(21)?,
                    typical_sqi: row.get(22)?,
                    post_trigger_sleep_sec: row.get(23)?,
                    trigger_power_action: row.get(24)?,
                    rapid_resume_within_5m: row.get::<_, i64>(25)? != 0,
                    grace_cancel_count: row.get(26)?,
                    rapid_wake_fp_count: row.get(27)?,
                    snooze_count: row.get(28)?,
                    auto_anomalous: row.get::<_, i64>(29)? != 0,
                    epochs_count: row.get(30)?,
                    raw_samples_count: row.get(31)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let session_epochs = if options.include_epochs {
            let mut ep_stmt = self.conn.prepare(
                "SELECT s.id, e.seq, e.ts_ms, e.hr_bpm_60s, e.z_hr, e.hr_slope_4m,
                        e.r_var_120s, e.ln_rmssd_60s, e.delta_ln_rmssd_60s, e.rmssd_sdnn_120s,
                        e.hrv_valid, e.rr_artifact_frac, e.coverage, e.flatline_stale,
                        e.accumulators_frozen, e.baseline_mu_hr, e.baseline_sigma_hr,
                        e.baseline_frozen, e.p_active_wake, e.p_quiet_wake, e.p_sleep, e.state
                 FROM session_epochs e
                 JOIN sessions s ON s.pk = e.session_pk
                 ORDER BY s.id ASC, e.seq ASC",
            )?;
            ep_stmt
                .query_map([], |row| {
                    let ts_ms: i64 = row.get(2)?;
                    Ok(SessionEpochBundleRecord {
                        id: None,
                        session_id: row.get(0)?,
                        seq: row.get(1)?,
                        ts_ms,
                        hr_bpm_60s: row.get(3)?,
                        z_hr: row.get(4)?,
                        hr_slope_4m: row.get(5)?,
                        r_var_120s: row.get(6)?,
                        ln_rmssd_60s: row.get(7)?,
                        delta_ln_rmssd_60s: row.get(8)?,
                        rmssd_sdnn_120s: row.get(9)?,
                        hrv_valid: row.get::<_, i64>(10)? != 0,
                        rr_artifact_frac: row.get(11)?,
                        coverage: row.get(12)?,
                        flatline_stale: row.get::<_, i64>(13)? != 0,
                        accumulators_frozen: row.get::<_, i64>(14)? != 0,
                        baseline_mu_hr: row.get(15)?,
                        baseline_sigma_hr: row.get(16)?,
                        baseline_frozen: row.get::<_, i64>(17)? != 0,
                        p_active_wake: row.get(18)?,
                        p_quiet_wake: row.get(19)?,
                        p_sleep: row.get(20)?,
                        state: row.get(21)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };

        let raw_samples = if options.include_raw {
            let mut raw_stmt = self.conn.prepare(
                "SELECT s.id, c.seq, c.payload_blob
                 FROM raw_sample_chunks c
                 JOIN sessions s ON s.pk = c.session_pk
                 ORDER BY s.id ASC, c.seq ASC",
            )?;
            let chunks = raw_stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut records = Vec::new();
            let mut cur_sid = String::new();
            let mut next_seq = 0_u32;
            for (sid, chunk_seq, blob) in chunks {
                if sid != cur_sid {
                    cur_sid = sid.clone();
                    next_seq = chunk_seq;
                } else {
                    next_seq = next_seq.max(chunk_seq);
                }
                let packets = decode_raw_chunk(&blob)?;
                for pkt in packets {
                    records.push(RawSampleBundleRecord {
                        id: None,
                        session_id: sid.clone(),
                        seq: next_seq,
                        ts_ms: pkt.ts_ms,
                        bpm: pkt.bpm,
                        rr_ms_json: serde_json::to_string(&pkt.rr_ms)?,
                        contact_status: pkt.contact_status.as_str().to_string(),
                    });
                    next_seq = next_seq.saturating_add(1);
                }
            }
            records
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
                tx.execute("DELETE FROM raw_sample_chunks", [])?;
                tx.execute("DELETE FROM session_epochs", [])?;
                tx.execute("DELETE FROM session_feedback", [])?;
                tx.execute("DELETE FROM sessions", [])?;
                tx.execute("DELETE FROM calibration_boundaries", [])?;
                tx.execute("DELETE FROM calibration_meta", [])?;
                if !bundle.wearables.is_empty() {
                    tx.execute("DELETE FROM wearables", [])?;
                }
            }

            // Import Wearables (track MAC+address_type remapping so sessions merge cleanly across machines)
            let mut wearable_id_remap: HashMap<String, String> = HashMap::new();
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
                let resolved_id: String = tx.query_row(
                    "SELECT id FROM wearables WHERE mac = ?1 AND address_type = ?2",
                    params![w.mac, w.address_type],
                    |r| r.get(0),
                )?;
                if resolved_id != w.id {
                    wearable_id_remap.insert(w.id.clone(), resolved_id);
                }
            }

            // Import Sessions
            for s in &bundle.sessions {
                let effective_wearable_id = wearable_id_remap
                    .get(&s.wearable_id)
                    .map(String::as_str)
                    .unwrap_or(s.wearable_id.as_str());
                let inserted = tx.execute(
                    "INSERT INTO sessions (
                         id, wearable_id, status, start_ts_ms, end_ts_ms, end_reason,
                         detection_sensitivity, had_rr, warmup_mu_hr, warmup_sigma_hr,
                         warmup_ln_rmssd, pre_candidate_mu_hr, pre_candidate_sigma_hr,
                         pre_candidate_ln_rmssd, onset_candidate_ts_ms, onset_confirmed_ts_ms,
                         sleeping_ts_ms, sleep_mu_hr, onset_hr_bpm, delta_hr_onset,
                         sleep_ln_rmssd, sleep_hr_var_ratio, typical_sqi, post_trigger_sleep_sec,
                         trigger_power_action, rapid_resume_within_5m, grace_cancel_count,
                         rapid_wake_fp_count, snooze_count, auto_anomalous, epochs_count, raw_samples_count
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32)
                     ON CONFLICT(id) DO NOTHING",
                    params![
                        s.id,
                        effective_wearable_id,
                        s.status,
                        s.start_ts_ms,
                        s.end_ts_ms,
                        s.end_reason,
                        s.detection_sensitivity,
                        if s.had_rr { 1 } else { 0 },
                        s.warmup_mu_hr,
                        s.warmup_sigma_hr,
                        s.warmup_ln_rmssd,
                        s.pre_candidate_mu_hr,
                        s.pre_candidate_sigma_hr,
                        s.pre_candidate_ln_rmssd,
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

            // Build UUID -> internal surrogate session_pk map for child table inserts.
            let mut session_pk_map: HashMap<String, i64> = HashMap::new();
            {
                let mut pk_stmt = tx.prepare("SELECT id, pk FROM sessions")?;
                let rows =
                    pk_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
                for pair in rows {
                    let (id, pk) = pair?;
                    session_pk_map.insert(id, pk);
                }
            }

            let mut touched_session_pks: BTreeSet<i64> = BTreeSet::new();
            for s in &bundle.sessions {
                if let Some(&pk) = session_pk_map.get(&s.id) {
                    touched_session_pks.insert(pk);
                }
            }

            // Import Epochs
            for ep in &bundle.session_epochs {
                let session_pk = session_pk_map.get(&ep.session_id).copied().ok_or_else(|| {
                    StorageError::Validation(format!(
                        "bundle session_epochs references unknown session '{}'",
                        ep.session_id
                    ))
                })?;
                touched_session_pks.insert(session_pk);
                let inserted = tx.execute(
                    "INSERT INTO session_epochs (
                         session_pk, seq, ts_ms, hr_bpm_60s, z_hr, hr_slope_4m,
                         r_var_120s, ln_rmssd_60s, delta_ln_rmssd_60s, rmssd_sdnn_120s,
                         hrv_valid, rr_artifact_frac, coverage, flatline_stale,
                         accumulators_frozen, baseline_mu_hr, baseline_sigma_hr,
                         baseline_frozen, p_active_wake, p_quiet_wake, p_sleep, state
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
                     ON CONFLICT(session_pk, seq) DO NOTHING",
                    params![
                        session_pk,
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

            // Import Raw Samples (grouped per session, deduplicated by seq, and packed into <=15-sample chunks)
            let mut raw_by_session: BTreeMap<&str, Vec<(u32, HrPacket)>> = BTreeMap::new();
            for raw in &bundle.raw_samples {
                let rr_ms: Vec<f64> = serde_json::from_str(&raw.rr_ms_json)?;
                let contact_status: ContactStatus = raw
                    .contact_status
                    .parse()
                    .unwrap_or(ContactStatus::Unsupported);
                raw_by_session
                    .entry(raw.session_id.as_str())
                    .or_default()
                    .push((
                        raw.seq,
                        HrPacket {
                            ts_ms: raw.ts_ms,
                            bpm: raw.bpm,
                            rr_ms,
                            contact_status,
                        },
                    ));
            }
            for (session_id, mut samples) in raw_by_session {
                let session_pk = session_pk_map.get(session_id).copied().ok_or_else(|| {
                    StorageError::Validation(format!(
                        "bundle raw_samples references unknown session '{session_id}'"
                    ))
                })?;
                touched_session_pks.insert(session_pk);
                samples.sort_by_key(|(seq, _)| *seq);
                samples.dedup_by_key(|(seq, _)| *seq);

                let existing_chunks: Vec<RawChunkSpan> = if mode == ImportMode::Merge {
                    let mut stmt = tx.prepare_cached(
                        "SELECT seq, sample_count FROM raw_sample_chunks WHERE session_pk = ?1 ORDER BY seq ASC",
                    )?;
                    stmt.query_map(params![session_pk], |r| {
                        Ok(RawChunkSpan {
                            seq: r.get(0)?,
                            sample_count: r.get(1)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                } else {
                    Vec::new()
                };

                for (chunk_seq, pkts) in partition_new_raw_chunks(samples, &existing_chunks) {
                    let pkt_refs: Vec<&HrPacket> = pkts.iter().collect();
                    let encoded = encode_raw_chunk_with_bounds(&pkt_refs)?;
                    let inserted = tx.execute(
                        "INSERT INTO raw_sample_chunks (
                             session_pk, seq, start_ts_ms, end_ts_ms, sample_count, payload_blob
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT(session_pk, seq) DO NOTHING",
                        params![
                            session_pk,
                            chunk_seq,
                            encoded.start_ts_ms,
                            encoded.end_ts_ms,
                            encoded.sample_count,
                            encoded.payload_blob,
                        ],
                    )?;
                    if inserted > 0 {
                        summary.raw_samples_imported += pkt_refs.len();
                    }
                }
            }

            if !touched_session_pks.is_empty() {
                let mut reconcile_stmt = tx.prepare_cached(
                    "UPDATE sessions
                     SET epochs_count = MAX(
                             epochs_count,
                             (SELECT COUNT(*) FROM session_epochs WHERE session_pk = ?1)
                         ),
                         raw_samples_count = MAX(
                             raw_samples_count,
                             COALESCE((SELECT SUM(sample_count) FROM raw_sample_chunks WHERE session_pk = ?1), 0)
                         )
                     WHERE pk = ?1",
                )?;
                for session_pk in touched_session_pks {
                    reconcile_stmt.execute(params![session_pk])?;
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

        if mode == ImportMode::Replace {
            self.reclaim_freelist_pages()?;
        }
        self.flush_wal()?;

        Ok((summary, state))
    }
}

#[derive(Debug, Clone, Copy)]
struct RawChunkSpan {
    seq: u32,
    sample_count: u32,
}

impl RawChunkSpan {
    fn contains(self, target_seq: u32) -> bool {
        target_seq >= self.seq && target_seq < self.seq.saturating_add(self.sample_count)
    }
}

/// Filters sorted, deduplicated `(seq, HrPacket)` samples against existing chunk spans and
/// partitions remaining contiguous runs into `<= RAW_CHUNK_MAX_SAMPLES` chunks.
fn partition_new_raw_chunks(
    samples: Vec<(u32, HrPacket)>,
    existing_chunks: &[RawChunkSpan],
) -> Vec<(u32, Vec<HrPacket>)> {
    let mut chunks = Vec::new();
    let mut cur_seq: Option<u32> = None;
    let mut prev_seq = 0_u32;
    let mut cur_pkts: Vec<HrPacket> = Vec::new();

    for (seq, pkt) in samples {
        if existing_chunks.iter().any(|span| span.contains(seq)) {
            continue;
        }

        // Split on seq gaps so every stored chunk covers a contiguous [seq, seq + sample_count) span.
        let has_seq_gap = cur_seq.is_some() && seq != prev_seq.saturating_add(1);
        if (cur_pkts.len() >= RAW_CHUNK_MAX_SAMPLES || has_seq_gap)
            && let Some(start_seq) = cur_seq.take()
        {
            chunks.push((start_seq, std::mem::take(&mut cur_pkts)));
        }

        if cur_seq.is_none() {
            cur_seq = Some(seq);
        }
        prev_seq = seq;
        cur_pkts.push(pkt);
    }

    if let Some(start_seq) = cur_seq {
        chunks.push((start_seq, cur_pkts));
    }
    chunks
}
