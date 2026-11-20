use std::sync::Arc;

use serde_json::{Value, json};

use crate::calibration::CalibrationState;
use crate::config::ConfigPatch;
use crate::ipc::protocol::{
    AlertDismissParams, CalibrationSetBoundaryParams, DataBackupSqliteParams,
    DataExportBundleParams, DataExportBundleResponse, DataHardWipeParams, DataImportBundleParams,
    DataImportBundleResponse, DataPurgeRawResponse, EnrollAuthResponseParams, EnrollSelectParams,
    EnrollSelectResponse, EnrollStartParams, EnrollStartResponse, ErrorPayload, PROTOCOL_VERSION,
    PauseParams, SessionDeleteParams, SessionDeleteResponse, SessionExportTraceParams,
    SessionExportTraceResponse, SessionGetParams, SessionListParams, SetDryRunParams,
    SetFeedbackParams, SetSeedParams, SnoozeGraceParams, SnoozeGraceResponse, TestTriggerParams,
    TestTriggerResponse, WearableConnectParams, WearableConnectResponse, WearableListResponse,
    WearableMutationResponse, WearableRenameParams, WearableUnenrollParams, error_codes,
};
use crate::storage::{ImportMode, SetFeedbackRequest, WearableDescriptor};

use super::enrollment::try_system_conn;
use super::{DaemonCore, EnrollmentLock, ipc_err, serialize_json, storage_to_ipc_err};

impl DaemonCore {
    /// Dispatches an IPC command over the Unix domain socket.
    pub async fn handle_op(
        self: &Arc<Self>,
        op: &str,
        params: Value,
        client_id: u64,
    ) -> Result<Value, ErrorPayload> {
        match op {
            "daemon.ping" => Ok(json!({ "pong": true, "v": PROTOCOL_VERSION })),
            "daemon.status" => {
                self.refresh_playing_players_count().await;
                serialize_json(self.status())
            }
            "monitor.pause" => {
                let parsed: PauseParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid monitor.pause params: {e}"),
                    )
                })?;
                serialize_json(self.pause(parsed)?)
            }
            "monitor.resume" => serialize_json(self.resume()),
            "monitor.set_dry_run" => {
                let parsed: SetDryRunParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid monitor.set_dry_run params: {e}"),
                    )
                })?;
                serialize_json(self.set_dry_run(parsed.dry_run))
            }
            "monitor.cancel_grace" => {
                self.cancel_grace().await?;
                Ok(json!({ "cancelled": true }))
            }
            "monitor.snooze_grace" => {
                let parsed: SnoozeGraceParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid monitor.snooze_grace params: {e}"),
                    )
                })?;
                let dur_sec = parsed
                    .duration_sec
                    .or_else(|| parsed.duration_min.map(|m| m.saturating_mul(60)));
                let dur = self.snooze_grace(dur_sec).await?;
                serialize_json(SnoozeGraceResponse {
                    snoozed: true,
                    duration_sec: dur,
                })
            }
            "monitor.test_trigger" => {
                let parsed: TestTriggerParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid monitor.test_trigger params: {e}"),
                    )
                })?;
                let grace_sec = parsed.grace_sec.or(parsed.grace_period_sec);
                let eff_sec = self.start_test_trigger(grace_sec).await?;
                serialize_json(TestTriggerResponse {
                    test_trigger_started: true,
                    grace_period_sec: eff_sec,
                })
            }
            "config.get" => {
                let guard = self.inner.lock().expect("daemon state mutex poisoned");
                serialize_json(self.build_config_snapshot_locked(&guard))
            }
            "config.update" => {
                let patch: ConfigPatch = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_CONFIG,
                        format!("invalid config patch: {e}"),
                    )
                })?;
                serialize_json(self.update_config(&patch)?)
            }
            "config.reload" => serialize_json(self.reload_from_disk()?),
            "session.list" => {
                let parsed: SessionListParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.list params: {e}"),
                    )
                })?;
                let limit = parsed.limit.unwrap_or(20);
                let offset = parsed.offset.unwrap_or(0);
                let page = self
                    .persistence
                    .list_sessions(limit, offset)
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(page)
            }
            "session.get" => {
                let parsed: SessionGetParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.get params: {e}"),
                    )
                })?;
                let detail = self
                    .persistence
                    .get_session(parsed.id)
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(detail)
            }
            "session.delete" => {
                let parsed: SessionDeleteParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.delete params: {e}"),
                    )
                })?;
                {
                    let guard = self.inner.lock().expect("daemon state mutex poisoned");
                    if guard.active_session_id.as_deref() == Some(&parsed.id)
                        || guard
                            .active_session_id
                            .as_ref()
                            .is_some_and(|aid| aid.starts_with(&parsed.id))
                    {
                        return Err(ipc_err(
                            error_codes::PRECONDITION_FAILED,
                            "cannot delete an active session",
                        ));
                    }
                }
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (deleted_id, cal_state) = self
                    .persistence
                    .delete_session_and_recompute(parsed.id, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state);
                serialize_json(SessionDeleteResponse {
                    deleted_session_id: deleted_id.clone(),
                    deleted_id,
                })
            }
            "session.export_trace" => {
                let parsed: SessionExportTraceParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid session.export_trace params: {e}"),
                        )
                    })?;
                let dest_path = std::path::PathBuf::from(&parsed.path);
                let format = parsed.format.unwrap_or(crate::replay::TraceFormat::Csv);
                let count = self
                    .persistence
                    .export_trace(parsed.id, dest_path, format, !parsed.no_anonymize)
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(SessionExportTraceResponse {
                    path: parsed.path,
                    events_count: count,
                })
            }
            "session.set_feedback" => {
                let parsed: SetFeedbackParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid session.set_feedback params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let req = SetFeedbackRequest {
                    session_id_or_prefix: parsed.session_id,
                    verdict: parsed.verdict,
                    onset_hhmm: parsed.onset_hhmm,
                    avg_sleep_hr: parsed.avg_sleep_hr,
                    clear_all: parsed.clear,
                };
                let (detail, cal_state) = self
                    .persistence
                    .set_feedback_and_recompute(req, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state);
                serialize_json(detail)
            }
            "calibration.status" => serialize_json(self.calibration_state()),
            "calibration.set_seed" => {
                let parsed: SetSeedParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid calibration.set_seed params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let cal_state = self
                    .persistence
                    .set_day1_seed_and_recompute(parsed.sleep_hr, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                serialize_json(cal_state)
            }
            "calibration.accept_shift" => {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let accepted = self
                    .persistence
                    .accept_shift_and_recompute(now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(accepted.calibration.clone());
                serialize_json(accepted)
            }
            "calibration.dismiss_shift" => {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let cal_state = self
                    .persistence
                    .dismiss_shift_and_recompute(now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                serialize_json(cal_state)
            }
            "calibration.set_boundary" => {
                let parsed: CalibrationSetBoundaryParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid calibration.set_boundary params: {e}"),
                        )
                    })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (boundary, cal_state) = self
                    .persistence
                    .set_calibration_boundary_and_recompute(
                        parsed.effective_ts_ms,
                        parsed.seed_sleep_hr,
                        parsed.reset_floors,
                        now_ms,
                    )
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                Ok(json!({ "boundary": boundary, "calibration": cal_state }))
            }
            "calibration.undo_boundary" => {
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (boundary, cal_state) = self
                    .persistence
                    .undo_calibration_boundary_and_recompute(now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                Ok(json!({ "boundary": boundary, "calibration": cal_state }))
            }
            "calibration.list_boundaries" => {
                let boundaries = self
                    .persistence
                    .list_calibration_boundaries()
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(boundaries)
            }
            "data.stats" => {
                let stats = self
                    .persistence
                    .get_storage_stats()
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(stats)
            }
            "data.purge_raw" => {
                let reclaimed = self
                    .persistence
                    .purge_raw_samples()
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(DataPurgeRawResponse {
                    reclaimed_rows: reclaimed,
                })
            }
            "data.hard_wipe" => {
                let parsed: DataHardWipeParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid data.hard_wipe params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.active_session_id = None;
                    guard.detector_state = crate::sleep_engine::DetectorState::Idle;
                    guard.hr_ring_buffer.clear();
                    guard.calibration_state = CalibrationState::default();
                }
                self.persistence
                    .hard_wipe(parsed.all, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                if parsed.all && self.paths.config_file.try_exists().unwrap_or(false) {
                    let _ = std::fs::remove_file(&self.paths.config_file);
                }
                if parsed.all {
                    self.wearable_source.notify_allowlist_changed();
                    self.publish_wearables_updated().await;
                }
                self.apply_calibration_update(CalibrationState::default());
                Ok(json!({ "wiped": true }))
            }
            "data.backup_sqlite" => {
                let parsed: DataBackupSqliteParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid data.backup_sqlite params: {e}"),
                        )
                    })?;
                let path = std::path::PathBuf::from(parsed.path);
                self.persistence
                    .backup_sqlite(path.clone())
                    .await
                    .map_err(storage_to_ipc_err)?;
                Ok(json!({ "path": path.to_string_lossy() }))
            }
            "data.export_bundle" => {
                let parsed: DataExportBundleParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid data.export_bundle params: {e}"),
                        )
                    })?;
                let options = parsed.options();
                let path = std::path::PathBuf::from(&parsed.path);
                let now_ms = self.clock.now_utc().timestamp_millis();
                self.persistence
                    .export_bundle(path, options, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(DataExportBundleResponse { path: parsed.path })
            }
            "data.import_bundle" => {
                let parsed: DataImportBundleParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid data.import_bundle params: {e}"),
                        )
                    })?;
                let path = std::path::PathBuf::from(parsed.path);
                let mode = parsed.mode.unwrap_or(ImportMode::Merge);
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (summary, cal_state) = self
                    .persistence
                    .import_bundle(path, mode, parsed.force, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.apply_calibration_update(cal_state.clone());
                self.wearable_source.notify_allowlist_changed();
                self.publish_wearables_updated().await;
                serialize_json(DataImportBundleResponse {
                    summary,
                    calibration: cal_state,
                })
            }
            "wearable.list" => {
                let statuses = self
                    .build_wearable_statuses()
                    .await
                    .map_err(storage_to_ipc_err)?;
                serialize_json(WearableListResponse {
                    wearables: statuses,
                })
            }
            "wearable.rename" => {
                let parsed: WearableRenameParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid wearable.rename params: {e}"),
                    )
                })?;
                let updated = self
                    .persistence
                    .rename_wearable(parsed.id, parsed.alias)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.wearable_source.notify_allowlist_changed();
                self.publish_wearables_updated().await;
                serialize_json(WearableMutationResponse { wearable: updated })
            }
            "wearable.connect" => {
                let parsed: WearableConnectParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.connect params: {e}"),
                        )
                    })?;
                let list = self
                    .persistence
                    .list_wearables()
                    .await
                    .map_err(storage_to_ipc_err)?;
                let w = list
                    .into_iter()
                    .find(|w| w.id == parsed.id || w.id.starts_with(&parsed.id))
                    .ok_or_else(|| {
                        ipc_err(
                            error_codes::NOT_FOUND,
                            format!("wearable '{}' not found", parsed.id),
                        )
                    })?;
                if let Some(conn) = try_system_conn().await {
                    let _ = crate::bluetooth::operations::connect_device(&conn, &w.mac).await;
                }
                serialize_json(WearableConnectResponse {
                    ok: true,
                    message: format!("connection requested for {}", w.id),
                })
            }
            "wearable.unenroll" => {
                let parsed: WearableUnenrollParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.unenroll params: {e}"),
                        )
                    })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let (unenrolled, cal_state) = self
                    .persistence
                    .unenroll_wearable(parsed.id, parsed.delete_sessions, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                {
                    let norm = unenrolled.mac.to_ascii_uppercase();
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.wearable_statuses.remove(&norm);
                    guard.wearable_batteries.remove(&norm);
                    guard.wearable_body_locations.remove(&norm);
                }
                if parsed.forget_bluez
                    && let Some(conn) = try_system_conn().await
                {
                    let _ = crate::bluetooth::operations::remove_device(
                        &conn,
                        "/org/bluez/hci0",
                        &unenrolled.mac,
                    )
                    .await;
                }
                if let Some(cal_state) = cal_state {
                    self.apply_calibration_update(cal_state);
                }
                self.wearable_source.notify_allowlist_changed();
                self.publish_wearables_updated().await;
                serialize_json(WearableMutationResponse {
                    wearable: unenrolled,
                })
            }
            "wearable.enroll_start" => {
                let parsed: EnrollStartParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid wearable.enroll_start params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let timeout_sec = parsed.timeout_sec.unwrap_or(30).clamp(5, 120);
                let timeout_ms = (timeout_sec as i64) * 1000;

                if let Some(conn) = try_system_conn().await {
                    let _ =
                        crate::bluetooth::operations::start_le_discovery(&conn, "/org/bluez/hci0")
                            .await;
                    if let Ok(discovered) =
                        crate::bluetooth::operations::list_heart_rate_devices(&conn).await
                    {
                        let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                        guard.discovered_wearables = discovered;
                    }
                }

                let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                if let Some(ref lock) = guard.enrollment {
                    let expired = now_ms >= lock.started_at_ms + lock.timeout_ms;
                    if !expired && lock.client_id != client_id {
                        return Err(ipc_err(
                            error_codes::ENROLLMENT_IN_PROGRESS,
                            "another enrollment session is currently in progress",
                        ));
                    }
                }
                guard.enrollment = Some(EnrollmentLock {
                    client_id,
                    started_at_ms: now_ms,
                    timeout_ms,
                });
                let devices = guard.discovered_wearables.clone();
                serialize_json(EnrollStartResponse {
                    devices: devices.clone(),
                    wearables: devices,
                })
            }
            "wearable.enroll_select" => {
                let parsed: EnrollSelectParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid wearable.enroll_select params: {e}"),
                    )
                })?;
                let now_ms = self.clock.now_utc().timestamp_millis();
                let norm_mac = parsed.address.trim().to_ascii_uppercase();
                let (discovered, is_rpa, address_type, advertised_name) = {
                    let guard = self.inner.lock().expect("daemon state mutex poisoned");
                    match guard.enrollment {
                        Some(ref lock) if lock.client_id == client_id => {}
                        Some(_) => {
                            return Err(ipc_err(
                                error_codes::PRECONDITION_FAILED,
                                "enrollment session is owned by another client",
                            ));
                        }
                        None => {
                            return Err(ipc_err(
                                error_codes::PRECONDITION_FAILED,
                                "no active enrollment session",
                            ));
                        }
                    }
                    let discovered = guard
                        .discovered_wearables
                        .iter()
                        .find(|d| d.address.eq_ignore_ascii_case(&norm_mac))
                        .cloned();

                    let address_type = discovered
                        .as_ref()
                        .map(|d| d.address_type.clone())
                        .unwrap_or_else(|| {
                            if crate::bluetooth::dbus::is_resolvable_private_address(
                                &norm_mac, "random",
                            ) {
                                "random".to_string()
                            } else {
                                "public".to_string()
                            }
                        });
                    let is_rpa = crate::bluetooth::dbus::is_resolvable_private_address(
                        &norm_mac,
                        &address_type,
                    );
                    let advertised_name = discovered.as_ref().and_then(|d| d.name.clone());
                    (discovered, is_rpa, address_type, advertised_name)
                };

                let pair_result = Arc::clone(&self.enrollment_driver)
                    .bond_and_pair(self, &norm_mac, is_rpa)
                    .await;

                {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    if matches!(guard.enrollment, Some(ref lock) if lock.client_id == client_id) {
                        guard.enrollment = None;
                    }
                    guard.active_auth_reply = None;
                }

                if let Err(err) = pair_result {
                    return Err(ipc_err(error_codes::BACKEND_REFUSED, err));
                }

                let mut warnings = Vec::new();
                let mut hints = Vec::new();
                if is_rpa {
                    warnings.push("Wearable uses a rotating Resolvable Private Address (RPA). Bonding ensures auto-reconnection persists.".to_string());
                }
                if let Some(ref d) = discovered
                    && crate::bluetooth::dbus::is_dual_mode_bredr_candidate(&d.uuids)
                {
                    hints.push("Dual-mode BR/EDR wearable detected; classic profiles (A2DP/HFP) may share this adapter.".to_string());
                }

                let desc = WearableDescriptor {
                    mac: norm_mac,
                    address_type,
                    alias: parsed.alias,
                    advertised_name,
                    supports_rr: false,
                    supports_contact: true,
                };
                let w_id = self
                    .persistence
                    .upsert_wearable(desc, now_ms)
                    .await
                    .map_err(storage_to_ipc_err)?;
                self.wearable_source.notify_allowlist_changed();
                self.publish_wearables_updated().await;
                let list = self
                    .persistence
                    .list_wearables()
                    .await
                    .map_err(storage_to_ipc_err)?;
                let enrolled = list.into_iter().find(|w| w.id == w_id).ok_or_else(|| {
                    ipc_err(
                        error_codes::INTERNAL_ERROR,
                        "failed to load newly enrolled wearable",
                    )
                })?;

                serialize_json(EnrollSelectResponse {
                    wearable: enrolled,
                    warnings,
                    hints,
                })
            }
            "alert.dismiss" => {
                let parsed: AlertDismissParams = serde_json::from_value(params).map_err(|e| {
                    ipc_err(
                        error_codes::INVALID_PARAMS,
                        format!("invalid alert.dismiss params: {e}"),
                    )
                })?;
                let (changed, alerts) = {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    let before = guard.alerts.len();
                    guard.alerts.retain(|a| a.id != parsed.id || !a.dismissible);
                    (guard.alerts.len() != before, guard.alerts.clone())
                };
                if changed {
                    self.broker
                        .publish_control("AlertChanged", json!({ "alerts": alerts }));
                }
                Ok(json!({ "dismissed": changed }))
            }
            "wearable.enroll_cancel" => {
                if let Some(conn) = try_system_conn().await {
                    let _ =
                        crate::bluetooth::operations::stop_le_discovery(&conn, "/org/bluez/hci0")
                            .await;
                }
                let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                guard.enrollment = None;
                guard.discovered_wearables.clear();
                guard.active_auth_reply = None;
                Ok(json!({ "ok": true }))
            }
            "wearable.enroll_auth_response" => {
                let parsed: EnrollAuthResponseParams =
                    serde_json::from_value(params).map_err(|e| {
                        ipc_err(
                            error_codes::INVALID_PARAMS,
                            format!("invalid wearable.enroll_auth_response params: {e}"),
                        )
                    })?;
                let reply_tx = {
                    let mut guard = self.inner.lock().expect("daemon state mutex poisoned");
                    guard.active_auth_reply.take()
                };
                if let Some(tx) = reply_tx {
                    let _ = tx.send(parsed);
                }
                Ok(json!({ "ok": true }))
            }
            _ => Err(ipc_err(
                error_codes::UNKNOWN_OP,
                format!("unknown IPC operation '{op}'"),
            )),
        }
    }
}
