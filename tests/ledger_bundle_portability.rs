mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use bpmute::config::{PatchField, PowerAction, Sensitivity};
use bpmute::exit_code::ExitCode;
use bpmute::sleep_engine::{ContactStatus, DetectorState, HrPacket, SessionEndReason};
use bpmute::storage::{
    CloseSessionRequest, ExportBundleOptions, ImportMode, LedgerStore, OpenSessionRequest,
    PersistenceWorker, SetFeedbackRequest, WearableDescriptor, backups_dir_in_data_dir,
};
use rusqlite::Connection;
use serde_json::{Value, json};

use common::sample_session_summary as sample_summary;
use common::storage::{
    TempStoreEnv, run_inproc_cli, sample_epoch, seed_wearable_and_closed_session,
};

#[tokio::test]
async fn test_json_ledger_bundle_export_import_merge_and_replace_rollback() {
    let store1_env = TempStoreEnv::new();

    {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&store1_env.db_path, &store1_env.lock_path, 5_000_000)
                .unwrap();
        let desc = WearableDescriptor {
            mac: "44:55:66:77:88:99".to_string(),
            address_type: "public".to_string(),
            alias: Some("BundleWatch".to_string()),
            advertised_name: Some("BundleWatch".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        let w_id = handle.upsert_wearable(desc, 5_000_000).await.unwrap();

        let sid = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: 5_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();
        handle
            .append_epoch_and_raw(
                sid.clone(),
                Some((
                    0,
                    sample_epoch(5_015_000, 59.0, 0.9, DetectorState::Sleeping),
                )),
                vec![(
                    0,
                    HrPacket {
                        ts_ms: 5_001_000,
                        bpm: 59.0,
                        rr_ms: vec![1010.0],
                        contact_status: ContactStatus::Contact,
                    },
                )],
            )
            .await
            .unwrap();
        handle
            .close_session_and_recompute(
                CloseSessionRequest {
                    session_id: sid,
                    summary: sample_summary(5_000_000, 5_030_000, 57.0),
                    trigger_power_action: PowerAction::Suspend,
                    rapid_resume_within_5m: false,
                },
                5_030_000,
            )
            .await
            .unwrap();

        drop(handle);
        worker.shutdown_and_join().unwrap();
    }

    // 1) Export Ledger Bundle
    let bundle_path = store1_env.temp.path().join("export.json");
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "export",
            &bundle_path.to_string_lossy(),
            "--include-epochs",
            "--include-wearables",
            "--include-raw",
        ],
        &store1_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Exported ledger bundle"));

    // Check bundle 0600 permissions
    let bundle_perms = fs::metadata(&bundle_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(bundle_perms, 0o600);

    // Validate bundle JSON content
    let bundle_content = fs::read_to_string(&bundle_path).unwrap();
    let bundle_json: Value = serde_json::from_str(&bundle_content).unwrap();
    assert_eq!(bundle_json["schema_version"], 2);
    assert_eq!(bundle_json["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(bundle_json["wearables"].as_array().unwrap().len(), 1);
    assert_eq!(bundle_json["session_epochs"].as_array().unwrap().len(), 1);
    assert_eq!(bundle_json["raw_samples"].as_array().unwrap().len(), 1);
    assert!(bundle_json["session_epochs"][0].get("id").is_none());
    assert!(bundle_json["raw_samples"][0].get("id").is_none());
    assert!(bundle_json["session_epochs"][0]["session_id"].is_string());
    assert!(bundle_json["raw_samples"][0]["session_id"].is_string());

    // 2) Set up a second environment for import
    let store2_env = TempStoreEnv::new();
    {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&store2_env.db_path, &store2_env.lock_path, 1).unwrap();
        drop(handle);
        worker.shutdown_and_join().unwrap();
    }

    // Import with --mode merge
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &bundle_path.to_string_lossy(),
            "--mode",
            "merge",
            "--yes",
        ],
        &store2_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Imported bundle"));

    let store2 = LedgerStore::open_read_only(&store2_env.db_path)
        .unwrap()
        .unwrap();
    let stats2 = store2.get_storage_stats().unwrap();
    assert_eq!(stats2.sessions_count, 1);
    assert_eq!(stats2.wearables_count, 1);
    assert_eq!(stats2.session_epochs_count, 1);
    assert_eq!(stats2.raw_samples_count, 1);

    // Idempotent re-import: re-importing the same bundle adds 0 sessions
    drop(store2);
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &bundle_path.to_string_lossy(),
            "--mode",
            "merge",
            "--yes",
        ],
        &store2_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Imported bundle"));
    let store2_re = LedgerStore::open_read_only(&store2_env.db_path)
        .unwrap()
        .unwrap();
    assert_eq!(store2_re.get_storage_stats().unwrap().sessions_count, 1);
    drop(store2_re);

    // 3) Import with --mode replace: verifies automatic 0600 rollback backup in backups/
    let backups_dir = backups_dir_in_data_dir(&store2_env.paths.data_dir);
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &bundle_path.to_string_lossy(),
            "--mode",
            "replace",
            "--yes",
        ],
        &store2_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Rollback backup saved to"));

    let backups = fs::read_dir(&backups_dir).unwrap().collect::<Vec<_>>();
    assert!(
        !backups.is_empty(),
        "rollback backup must be created before replace"
    );
    let bkp_entry = backups[0].as_ref().unwrap();
    let bkp_mode = bkp_entry.metadata().unwrap().permissions().mode() & 0o777;
    assert_eq!(bkp_mode, 0o600);
}

#[tokio::test]
async fn test_manual_close_session_end_reason_persistence_and_bundle_round_trip() {
    assert_eq!(SessionEndReason::ManualClose.as_str(), "manual_close");
    assert_eq!(SessionEndReason::ManualClose.to_string(), "manual_close");
    assert_eq!(
        "manual_close".parse::<SessionEndReason>().unwrap(),
        SessionEndReason::ManualClose
    );
    assert_eq!(
        serde_json::to_string(&SessionEndReason::ManualClose).unwrap(),
        "\"manual_close\""
    );
    assert_eq!(
        serde_json::from_str::<SessionEndReason>("\"manual_close\"").unwrap(),
        SessionEndReason::ManualClose
    );

    let store1_env = TempStoreEnv::new();
    let sid = {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&store1_env.db_path, &store1_env.lock_path, 1_000_000)
                .unwrap();
        let wid = handle
            .upsert_wearable(
                WearableDescriptor {
                    mac: "11:22:33:44:55:66".to_string(),
                    address_type: "public".to_string(),
                    alias: Some("ManualCloseWatch".to_string()),
                    advertised_name: Some("ManualCloseWatch".to_string()),
                    supports_rr: true,
                    supports_contact: true,
                },
                1_000_000,
            )
            .await
            .unwrap();

        let sid = handle
            .open_session(OpenSessionRequest {
                wearable_id: wid,
                start_ts_ms: 1_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();

        let mut summary = sample_summary(1_000_000, 1_060_000, 58.0);
        summary.end_reason = SessionEndReason::ManualClose;

        handle
            .close_session_and_recompute(
                CloseSessionRequest {
                    session_id: sid.clone(),
                    summary,
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                1_060_000,
            )
            .await
            .unwrap();

        drop(handle);
        worker.shutdown_async().await;
        sid
    };

    let store1 = LedgerStore::open_read_only(&store1_env.db_path)
        .unwrap()
        .unwrap();
    let detail1 = store1.get_session(&sid).unwrap();
    assert_eq!(detail1.session.end_reason, SessionEndReason::ManualClose);
    assert_eq!(detail1.session.end_ts_ms, 1_060_000);

    let list1 = store1.list_sessions(10, 0).unwrap();
    assert_eq!(list1.sessions.len(), 1);
    assert_eq!(
        list1.sessions[0].end_reason,
        Some(SessionEndReason::ManualClose)
    );

    let bundle_path = store1_env.temp.path().join("manual_close_bundle.json");
    store1
        .export_bundle(
            &bundle_path,
            ExportBundleOptions {
                include_epochs: true,
                include_wearables: true,
                include_raw: false,
            },
            1_100_000,
        )
        .unwrap();

    let bundle_content = fs::read_to_string(&bundle_path).unwrap();
    let bundle_json: Value = serde_json::from_str(&bundle_content).unwrap();
    assert_eq!(
        bundle_json["sessions"][0]["end_reason"].as_str(),
        Some("manual_close")
    );

    let store2_env = TempStoreEnv::new();
    let mut store2 =
        LedgerStore::open_read_write(&store2_env.db_path, &store2_env.lock_path).unwrap();
    store2
        .import_bundle(&bundle_path, ImportMode::Replace, false, 1_200_000)
        .unwrap();

    let detail2 = store2.get_session(&sid).unwrap();
    assert_eq!(detail2.session.end_reason, SessionEndReason::ManualClose);
    assert_eq!(detail2.session.end_ts_ms, 1_060_000);
}

#[tokio::test]
async fn test_ledger_bundle_v2_round_trip_and_v1_bundle_restore() {
    // 1. Seed store1 with wearable, multi-chunk raw samples (20 packets), 2 epochs, feedback, and boundary.
    let store1_env = TempStoreEnv::new();

    let base_ts = 1_700_100_000_000_i64;
    let packets: Vec<(u32, HrPacket)> = (0..20_u32)
        .map(|i| {
            let contact = match i % 3 {
                0 => ContactStatus::Unsupported,
                1 => ContactStatus::NoContact,
                _ => ContactStatus::Contact,
            };
            let bpm = 60.0 + f64::from(i) * 0.25;
            let rr_ms = match i % 3 {
                0 => vec![],
                1 => vec![f64::from(980_u16 + i as u16) * 1000.0 / 1024.0],
                _ => vec![931.125, 940.5],
            };
            (
                i,
                HrPacket {
                    ts_ms: base_ts + i64::from(i) * 1000,
                    bpm,
                    rr_ms,
                    contact_status: contact,
                },
            )
        })
        .collect();

    let wearable_desc = WearableDescriptor {
        mac: "AA:22:33:44:55:66".to_string(),
        address_type: "public".to_string(),
        alias: Some("V2BundleWatch".to_string()),
        advertised_name: Some("V2BundleWatch".to_string()),
        supports_rr: true,
        supports_contact: true,
    };

    let (wid1, sid1, expected_cal) = {
        let mut store1 =
            LedgerStore::open_read_write(&store1_env.db_path, &store1_env.lock_path).unwrap();
        let wid = store1.upsert_wearable(&wearable_desc, base_ts).unwrap();
        let sid = store1
            .open_session(&OpenSessionRequest {
                wearable_id: wid.clone(),
                start_ts_ms: base_ts,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .unwrap();
        let ep0 = sample_epoch(base_ts + 15_000, 61.0, 0.3, DetectorState::Monitoring);
        let ep1 = sample_epoch(base_ts + 30_000, 58.0, 0.9, DetectorState::Sleeping);
        let batch0: Vec<(u32, &HrPacket)> = packets[..15].iter().map(|(s, p)| (*s, p)).collect();
        let batch1: Vec<(u32, &HrPacket)> = packets[15..].iter().map(|(s, p)| (*s, p)).collect();
        store1
            .append_epoch_and_raw(&sid, Some((0, &ep0)), &batch0)
            .unwrap();
        store1
            .append_epoch_and_raw(&sid, Some((1, &ep1)), &batch1)
            .unwrap();
        store1
            .close_session_and_recompute(
                &CloseSessionRequest {
                    session_id: sid.clone(),
                    summary: sample_summary(base_ts, base_ts + 30_000, 57.5),
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                base_ts + 30_000,
            )
            .unwrap();
        store1
            .set_feedback_and_recompute(
                &SetFeedbackRequest {
                    session_id_or_prefix: sid.clone(),
                    verdict: PatchField::Unchanged,
                    onset_hhmm: PatchField::Unchanged,
                    avg_sleep_hr: PatchField::Set(56.5),
                    clear_all: false,
                },
                base_ts + 31_000,
            )
            .unwrap();
        let (_, cal) = store1
            .set_calibration_boundary(None, Some(56.0), false, base_ts + 32_000)
            .unwrap();
        (wid, sid, cal)
    };

    // 2. CLI export -> verify v2 schema, omitted child ids, retained top-level UUIDs, 1 Hz unpacked raw packets.
    let v2_bundle_path = store1_env.temp.path().join("bundle_v2.json");
    let (code, stdout, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "export",
            &v2_bundle_path.to_string_lossy(),
            "--include-epochs",
            "--include-wearables",
            "--include-raw",
        ],
        &store1_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "export stderr: {stderr}");
    assert!(stdout.contains("Exported ledger bundle"));

    let v2_json: Value =
        serde_json::from_str(&fs::read_to_string(&v2_bundle_path).unwrap()).unwrap();
    assert_eq!(v2_json["schema_version"], 2);
    assert_eq!(v2_json["wearables"][0]["id"], wid1);
    assert_eq!(v2_json["sessions"][0]["id"], sid1);
    assert!(v2_json["session_feedback"][0]["id"].is_string());
    assert!(v2_json["calibration_boundaries"][0]["id"].is_string());

    let epochs_arr = v2_json["session_epochs"].as_array().unwrap();
    assert_eq!(epochs_arr.len(), 2);
    for ep in epochs_arr {
        assert!(ep.get("id").is_none(), "v2 epoch must omit child id");
        assert_eq!(ep["session_id"], sid1);
    }

    let raw_arr = v2_json["raw_samples"].as_array().unwrap();
    assert_eq!(raw_arr.len(), 20);
    for (idx, raw) in raw_arr.iter().enumerate() {
        assert!(raw.get("id").is_none(), "v2 raw sample must omit child id");
        assert_eq!(raw["session_id"], sid1);
        assert_eq!(raw["seq"], idx as u64);
        assert!(raw["bpm"].is_f64());
        assert!(raw["rr_ms_json"].is_string());
        assert!(raw["contact_status"].is_string());
    }

    // 3. CLI import v2 into store2 (merge) and verify lossless replay, exact counts, and calibration parity.
    let store2_env = TempStoreEnv::new();
    {
        let _init =
            LedgerStore::open_read_write(&store2_env.db_path, &store2_env.lock_path).unwrap();
    }

    let (code, _, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &v2_bundle_path.to_string_lossy(),
            "--mode",
            "merge",
            "--yes",
        ],
        &store2_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "import v2 stderr: {stderr}");

    let store2 = LedgerStore::open_read_only(&store2_env.db_path)
        .unwrap()
        .unwrap();
    let stats2 = store2.get_storage_stats().unwrap();
    assert_eq!(stats2.sessions_count, 1);
    assert_eq!(stats2.session_epochs_count, 2);
    assert_eq!(stats2.raw_samples_count, 20);
    assert_eq!(stats2.session_feedback_count, 1);
    assert_eq!(stats2.calibration_boundaries_count, 1);
    assert_eq!(store2.get_calibration_state().unwrap(), expected_cal);

    let detail2 = store2.get_session(&sid1).unwrap();
    assert_eq!(detail2.session.epochs_count, 2);
    assert_eq!(detail2.session.raw_samples_count, 20);
    assert!(detail2.has_raw_samples);

    let replay2 = store2.load_session_for_replay(&sid1).unwrap();
    assert_eq!(replay2.raw_samples.len(), 20);
    for ((_, orig), loaded) in packets.iter().zip(replay2.raw_samples.iter()) {
        assert_eq!(loaded.ts_ms, orig.ts_ms);
        assert_eq!(loaded.bpm.to_bits(), orig.bpm.to_bits());
        assert_eq!(loaded.contact_status, orig.contact_status);
        assert_eq!(loaded.rr_ms.len(), orig.rr_ms.len());
        for (a, b) in orig.rr_ms.iter().zip(loaded.rr_ms.iter()) {
            assert_eq!(b.to_bits(), a.to_bits());
        }
    }
    drop(store2);

    // 4. Construct legacy v1 Ledger Bundle (with child UUIDs + one duplicate raw seq) and reject invalid v1 / unsupported versions.
    let mut v1_json = v2_json.clone();
    v1_json["schema_version"] = json!(1);
    for (idx, ep) in v1_json["session_epochs"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        ep.as_object_mut().unwrap().insert(
            "id".to_string(),
            json!(format!("01950000-0000-7000-8000-00000001{:04x}", idx)),
        );
    }
    for (idx, raw) in v1_json["raw_samples"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .enumerate()
    {
        raw.as_object_mut().unwrap().insert(
            "id".to_string(),
            json!(format!("01950000-0000-7000-8000-00000002{:04x}", idx)),
        );
    }
    // Add duplicate (session_id, seq = 0) with a distinct legacy child UUID to verify (session_pk, seq) deduplication.
    let mut dup_raw = v1_json["raw_samples"][0].clone();
    dup_raw["id"] = json!("01950000-0000-7000-8000-000000029999");
    v1_json["raw_samples"].as_array_mut().unwrap().push(dup_raw);
    // Simulate stale session header counts to verify imported row reconciliation.
    v1_json["sessions"][0]["epochs_count"] = json!(0);
    v1_json["sessions"][0]["raw_samples_count"] = json!(0);

    let v1_bundle_path = store1_env.temp.path().join("bundle_v1.json");
    fs::write(
        &v1_bundle_path,
        serde_json::to_string_pretty(&v1_json).unwrap(),
    )
    .unwrap();

    let mut bad_v1 = v1_json.clone();
    bad_v1["raw_samples"][0]["id"] = json!("not-a-valid-uuid");
    let bad_v1_path = store1_env.temp.path().join("bad_v1.json");
    fs::write(&bad_v1_path, serde_json::to_string(&bad_v1).unwrap()).unwrap();

    let mut unsupported_ver = v2_json.clone();
    unsupported_ver["schema_version"] = json!(99);
    let unsupported_path = store1_env.temp.path().join("unsupported_v99.json");
    fs::write(
        &unsupported_path,
        serde_json::to_string(&unsupported_ver).unwrap(),
    )
    .unwrap();

    // 5. Restore legacy v1 Ledger Bundle into store3 via CLI `merge` and `LedgerStore::import_bundle` idempotency.
    let store3_env = TempStoreEnv::new();
    {
        let mut store3 =
            LedgerStore::open_read_write(&store3_env.db_path, &store3_env.lock_path).unwrap();
        assert!(
            store3
                .import_bundle(&bad_v1_path, ImportMode::Merge, false, base_ts + 40_000)
                .is_err(),
            "v1 Ledger Bundle with malformed child UUID must be rejected"
        );
        assert!(
            store3
                .import_bundle(
                    &unsupported_path,
                    ImportMode::Merge,
                    false,
                    base_ts + 40_000
                )
                .is_err(),
            "unsupported bundle schema_version must be rejected"
        );
    }

    let (code, _, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &v1_bundle_path.to_string_lossy(),
            "--mode",
            "merge",
            "--yes",
        ],
        &store3_env.env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "import v1 merge stderr: {stderr}");

    {
        let mut store3 =
            LedgerStore::open_read_write(&store3_env.db_path, &store3_env.lock_path).unwrap();
        let stats3 = store3.get_storage_stats().unwrap();
        assert_eq!(stats3.sessions_count, 1);
        assert_eq!(stats3.session_epochs_count, 2);
        assert_eq!(stats3.raw_samples_count, 20);
        assert_eq!(store3.get_calibration_state().unwrap(), expected_cal);

        let d3 = store3.get_session(&sid1).unwrap();
        assert_eq!(d3.session.epochs_count, 2);
        assert_eq!(d3.session.raw_samples_count, 20);

        let (re_summary, re_cal) = store3
            .import_bundle(&v1_bundle_path, ImportMode::Merge, false, base_ts + 50_000)
            .unwrap();
        assert_eq!(re_summary.sessions_imported, 0);
        assert_eq!(re_summary.epochs_imported, 0);
        assert_eq!(re_summary.raw_samples_imported, 0);
        assert_eq!(re_cal, expected_cal);
        assert_eq!(store3.get_storage_stats().unwrap().raw_samples_count, 20);
    }

    // 6. Partial-overlap Merge: DB has first 10 packets (0..10); importing v2 bundle (0..20) fills 10..20 without duplication or loss.
    let store4_env = TempStoreEnv::new();
    {
        let mut store4 =
            LedgerStore::open_read_write(&store4_env.db_path, &store4_env.lock_path).unwrap();
        store4.upsert_wearable(&wearable_desc, base_ts).unwrap();
        let mut partial_v2 = v2_json.clone();
        partial_v2["raw_samples"] =
            Value::Array(v2_json["raw_samples"].as_array().unwrap()[..10].to_vec());
        partial_v2["sessions"][0]["raw_samples_count"] = json!(10);
        let partial_path = store4_env.temp.path().join("partial_v2.json");
        fs::write(&partial_path, serde_json::to_string(&partial_v2).unwrap()).unwrap();

        let (sum_p, _) = store4
            .import_bundle(&partial_path, ImportMode::Merge, false, base_ts + 45_000)
            .unwrap();
        assert_eq!(sum_p.raw_samples_imported, 10);

        let (sum_full, _) = store4
            .import_bundle(&v2_bundle_path, ImportMode::Merge, false, base_ts + 46_000)
            .unwrap();
        assert_eq!(sum_full.raw_samples_imported, 10);
        assert_eq!(store4.get_storage_stats().unwrap().raw_samples_count, 20);
        assert_eq!(
            store4.get_session(&sid1).unwrap().session.raw_samples_count,
            20
        );
        assert_eq!(
            store4
                .load_session_for_replay(&sid1)
                .unwrap()
                .raw_samples
                .len(),
            20
        );
    }

    // 7. ImportMode::Replace via CLI reclaims SQLite pages from heavy prior DB and restores v1 Ledger Bundle cleanly.
    let store5_env = TempStoreEnv::new();
    let size_before_replace = {
        let mut store5 =
            LedgerStore::open_read_write(&store5_env.db_path, &store5_env.lock_path).unwrap();
        seed_wearable_and_closed_session(
            &mut store5,
            "BB:CC:DD:EE:FF:00",
            "HeavyWatch",
            base_ts - 1_000_000,
            64.0,
            3_000,
        );
        store5.get_storage_stats().unwrap().db_size_bytes
    };

    let (code, stdout, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &v1_bundle_path.to_string_lossy(),
            "--mode",
            "replace",
            "--yes",
        ],
        &store5_env.env,
    )
    .await;
    assert_eq!(
        code,
        ExitCode::Success,
        "import v1 replace stderr: {stderr}"
    );
    assert!(stdout.contains("Rollback backup saved to"));

    let store5_after = LedgerStore::open_read_only(&store5_env.db_path)
        .unwrap()
        .unwrap();
    let stats5_after = store5_after.get_storage_stats().unwrap();
    assert_eq!(stats5_after.wal_size_bytes, 0);
    assert_eq!(stats5_after.sessions_count, 1);
    assert_eq!(stats5_after.session_epochs_count, 2);
    assert_eq!(stats5_after.raw_samples_count, 20);
    assert!(
        stats5_after.db_size_bytes < size_before_replace,
        "ImportMode::Replace must reclaim heavy prior pages: before={size_before_replace}, after={}",
        stats5_after.db_size_bytes
    );
    let ro_conn = Connection::open(&store5_env.db_path).unwrap();
    let freelist_count: i64 = ro_conn
        .query_row("PRAGMA freelist_count;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(freelist_count, 0);
}
