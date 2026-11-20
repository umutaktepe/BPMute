use std::fs;
use std::os::unix::fs::PermissionsExt;

use bpmute::cli::{Cli, execute_cli};
use bpmute::config::{
    PowerAction, RawSamplesRetention, RetentionConfig, Sensitivity, SessionEpochsRetention,
};
use bpmute::exit_code::ExitCode;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{
    CalibrationMaturity, ContactStatus, DetectorState, Epoch, EpochFeatures, HrPacket,
    SessionEndReason, SessionSummary,
};
use bpmute::storage::{
    CloseSessionRequest, LedgerStore, OpenSessionRequest, PersistenceWorker, WearableDescriptor,
    backups_dir_in_data_dir, db_path_in_data_dir, lock_path_in_data_dir,
};
use clap::Parser;
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn test_paths_and_env(temp: &TempDir) -> (ResolvedPaths, MapEnv) {
    let env = MapEnv::new()
        .with(
            "HOME",
            temp.path().join("home").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_CONFIG_HOME",
            temp.path().join("cfg").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_DATA_HOME",
            temp.path().join("data").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_RUNTIME_DIR",
            temp.path().join("run").to_string_lossy().into_owned(),
        );
    let paths = ResolvedPaths::resolve(None, None, None, &env).unwrap();
    (paths, env)
}

async fn run_inproc_cli(args: &[&str], env: &MapEnv) -> (ExitCode, String, String) {
    let cli = Cli::try_parse_from(args).expect("CLI args should parse");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = execute_cli(cli, env, &mut stdout, &mut stderr).await;
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn sample_epoch(ts_ms: i64, hr_bpm: f64, p_sleep: f64, state: DetectorState) -> Epoch {
    Epoch {
        session_index: 1,
        ts_ms,
        features: EpochFeatures {
            hr_bpm_60s: hr_bpm,
            z_hr: -1.5,
            hr_slope_4m: -0.35,
            r_var_120s: 0.45,
            ln_rmssd_60s: Some(3.6),
            delta_ln_rmssd_60s: Some(0.15),
            rmssd_sdnn_120s: Some(0.95),
        },
        hrv_valid: true,
        rr_artifact_frac: 0.01,
        coverage: 1.0,
        flatline_stale: false,
        accumulators_frozen: false,
        baseline_mu_hr: 66.0,
        baseline_sigma_hr: 2.3,
        baseline_frozen: p_sleep >= 0.30,
        p_active_wake: 0.05,
        p_quiet_wake: 1.0 - p_sleep - 0.05,
        p_sleep,
        state,
    }
}

fn sample_summary(start_ts_ms: i64, end_ts_ms: i64, sleep_mu_hr: f64) -> SessionSummary {
    SessionSummary {
        session_index: 1,
        start_ts_ms,
        end_ts_ms,
        end_reason: SessionEndReason::SustainedWakefulness,
        candidate_ts_ms: Some(start_ts_ms + 900_000),
        confirmed_ts_ms: Some(start_ts_ms + 1_080_000),
        sleeping_ts_ms: Some(start_ts_ms + 1_110_000),
        anomalous: false,
        fp_count: 0,
        warmup_mu_hr: Some(66.0),
        warmup_sigma_hr: Some(2.3),
        warmup_ln_rmssd: Some(3.45),
        onset_hr_bpm: Some(sleep_mu_hr + 0.5),
        sleep_mu_hr: Some(sleep_mu_hr),
        sleep_ln_rmssd: Some(3.7),
        observed_hr_var_ratio: Some(0.26),
        typical_sqi: Some(0.97),
        post_trigger_sleep_sec: 1_800,
        had_rr: true,
        grace_cancel_count: 0,
        rapid_wake_fp_count: 0,
        snooze_count: 0,
        epochs_count: 2,
    }
}

#[tokio::test]
async fn test_retention_pruning_rolling_ttl_and_active_session_immunity() {
    let temp = TempDir::new().unwrap();
    let data_dir = temp.path().join("data/bpmute");
    let db_path = db_path_in_data_dir(&data_dir);
    let lock_path = lock_path_in_data_dir(&data_dir);

    let (worker, handle, _) =
        PersistenceWorker::spawn(&db_path, &lock_path, 1_700_000_000_000).unwrap();

    let desc = WearableDescriptor {
        mac: "11:22:33:44:55:66".to_string(),
        address_type: "public".to_string(),
        alias: Some("Watch".to_string()),
        advertised_name: Some("Watch".to_string()),
        supports_rr: true,
        supports_contact: true,
    };
    let w_id = handle
        .upsert_wearable(desc, 1_700_000_000_000)
        .await
        .unwrap();

    let now_ms = 1_700_000_000_000i64;
    let day_ms = 86_400_000i64;

    // Session 1: closed 20 days ago (ts = now - 20 days)
    let s1_start = now_ms - 20 * day_ms;
    let s1_id = handle
        .open_session(OpenSessionRequest {
            wearable_id: w_id.clone(),
            start_ts_ms: s1_start,
            detection_sensitivity: Sensitivity::Balanced,
        })
        .await
        .unwrap();
    handle
        .append_epoch_and_raw(
            s1_id.clone(),
            Some((
                0,
                sample_epoch(s1_start + 15_000, 60.0, 0.9, DetectorState::Sleeping),
            )),
            vec![(
                0,
                HrPacket {
                    ts_ms: s1_start + 1_000,
                    bpm: 60.0,
                    rr_ms: vec![1000.0],
                    contact_status: ContactStatus::Contact,
                },
            )],
        )
        .await
        .unwrap();
    handle
        .close_session_and_recompute(
            CloseSessionRequest {
                session_id: s1_id.clone(),
                summary: sample_summary(s1_start, s1_start + 30_000, 58.0),
                trigger_power_action: PowerAction::Suspend,
                rapid_resume_within_5m: false,
            },
            s1_start + 30_000,
        )
        .await
        .unwrap();

    // Session 2: closed 5 days ago (ts = now - 5 days)
    let s2_start = now_ms - 5 * day_ms;
    let s2_id = handle
        .open_session(OpenSessionRequest {
            wearable_id: w_id.clone(),
            start_ts_ms: s2_start,
            detection_sensitivity: Sensitivity::Balanced,
        })
        .await
        .unwrap();
    handle
        .append_epoch_and_raw(
            s2_id.clone(),
            Some((
                0,
                sample_epoch(s2_start + 15_000, 62.0, 0.9, DetectorState::Sleeping),
            )),
            vec![(
                0,
                HrPacket {
                    ts_ms: s2_start + 1_000,
                    bpm: 62.0,
                    rr_ms: vec![980.0],
                    contact_status: ContactStatus::Contact,
                },
            )],
        )
        .await
        .unwrap();
    handle
        .close_session_and_recompute(
            CloseSessionRequest {
                session_id: s2_id.clone(),
                summary: sample_summary(s2_start, s2_start + 30_000, 60.0),
                trigger_power_action: PowerAction::Suspend,
                rapid_resume_within_5m: false,
            },
            s2_start + 30_000,
        )
        .await
        .unwrap();

    // Session 3: ACTIVE session opened 25 days ago (never closed)
    let s3_start = now_ms - 25 * day_ms;
    let s3_id = handle
        .open_session(OpenSessionRequest {
            wearable_id: w_id.clone(),
            start_ts_ms: s3_start,
            detection_sensitivity: Sensitivity::Balanced,
        })
        .await
        .unwrap();
    handle
        .append_epoch_and_raw(
            s3_id.clone(),
            Some((
                0,
                sample_epoch(s3_start + 15_000, 64.0, 0.9, DetectorState::Sleeping),
            )),
            vec![(
                0,
                HrPacket {
                    ts_ms: s3_start + 1_000,
                    bpm: 64.0,
                    rr_ms: vec![950.0],
                    contact_status: ContactStatus::Contact,
                },
            )],
        )
        .await
        .unwrap();

    // Set retention: raw_samples_days = 14, session_epochs_days = 30
    handle
        .set_retention_config(RetentionConfig {
            raw_samples_days: RawSamplesRetention::Days(14),
            session_epochs_days: SessionEpochsRetention::Days(30),
        })
        .await
        .unwrap();

    // Verify Session 1 has no raw samples (pruned automatically at Session 2 close), 1 epoch
    let s1_detail = handle.get_session(s1_id.clone()).await.unwrap();
    assert!(
        !s1_detail.has_raw_samples,
        "Session 1 raw sample (20 days old) was pruned at Session 2 close"
    );
    assert_eq!(s1_detail.epochs.len(), 1);

    // Verify Session 2 has raw samples, 1 epoch
    let s2_detail = handle.get_session(s2_id.clone()).await.unwrap();
    assert!(s2_detail.has_raw_samples);
    assert_eq!(s2_detail.epochs.len(), 1);

    // Verify Session 3 (ACTIVE) is completely immune!
    let s3_detail = handle.get_session(s3_id.clone()).await.unwrap();
    assert!(
        s3_detail.has_raw_samples,
        "Active session raw samples must never be pruned"
    );
    assert_eq!(
        s3_detail.epochs.len(),
        1,
        "Active session epochs must never be pruned"
    );

    // Now test session_epochs TTL: set session_epochs_days = 15
    handle
        .set_retention_config(RetentionConfig {
            raw_samples_days: RawSamplesRetention::Days(14),
            session_epochs_days: SessionEpochsRetention::Days(15),
        })
        .await
        .unwrap();
    let prune2 = handle.prune_retention(now_ms).await.unwrap();
    assert_eq!(
        prune2.session_epochs_pruned, 1,
        "Session 1 epoch (20 days old) pruned"
    );

    let s1_detail2 = handle.get_session(s1_id).await.unwrap();
    assert_eq!(s1_detail2.epochs.len(), 0);
    assert_eq!(s1_detail2.session.sleep_mu_hr, Some(58.0));

    // Session 3 still immune
    let s3_detail2 = handle.get_session(s3_id).await.unwrap();
    assert_eq!(s3_detail2.epochs.len(), 1, "Active session still untouched");

    drop(handle);
    worker.shutdown_and_join().unwrap();
}

#[tokio::test]
async fn test_sessions_delete_and_precondition_conflict_guardrail() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    let (s_closed_id, s_active_id) = {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&db_path, &lock_path, 1_000_000).unwrap();
        let desc = WearableDescriptor {
            mac: "22:33:44:55:66:77".to_string(),
            address_type: "public".to_string(),
            alias: Some("Band".to_string()),
            advertised_name: Some("Band".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        let w_id = handle.upsert_wearable(desc, 1_000_000).await.unwrap();

        let s1 = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: 1_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();
        handle
            .close_session_and_recompute(
                CloseSessionRequest {
                    session_id: s1.clone(),
                    summary: sample_summary(1_000_000, 1_030_000, 56.0),
                    trigger_power_action: PowerAction::Suspend,
                    rapid_resume_within_5m: false,
                },
                1_030_000,
            )
            .await
            .unwrap();

        let s2 = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: 2_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();

        drop(handle);
        worker.shutdown_and_join().unwrap();
        (s1, s2)
    };

    // 1) Offline CLI rejection of active session -> ExitCode::PreconditionConflict (4)
    let (code, _, err) = run_inproc_cli(
        &["bpmute", "sessions", "delete", &s_active_id, "--yes"],
        &env,
    )
    .await;
    assert_eq!(
        code,
        ExitCode::PreconditionConflict,
        "active session delete must exit with 4: {err}"
    );

    // 2) Offline CLI deletion of closed session -> ExitCode::Success (0)
    let (code, stdout, _) = run_inproc_cli(
        &["bpmute", "sessions", "delete", &s_closed_id, "--yes"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Deleted session"));

    // Verify session is gone from database
    let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
    assert!(store.get_session(&s_closed_id).is_err());
}

#[tokio::test]
async fn test_reversible_calibration_boundary_rebaseline_and_undo() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    // Record 3 sessions to build up calibration priors
    {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&db_path, &lock_path, 1_000_000).unwrap();
        let desc = WearableDescriptor {
            mac: "33:44:55:66:77:88".to_string(),
            address_type: "public".to_string(),
            alias: Some("Ring".to_string()),
            advertised_name: Some("Ring".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        let w_id = handle.upsert_wearable(desc, 1_000_000).await.unwrap();

        for i in 1..=3 {
            let start = 1_000_000i64 * i;
            let sid = handle
                .open_session(OpenSessionRequest {
                    wearable_id: w_id.clone(),
                    start_ts_ms: start,
                    detection_sensitivity: Sensitivity::Balanced,
                })
                .await
                .unwrap();
            handle
                .close_session_and_recompute(
                    CloseSessionRequest {
                        session_id: sid,
                        summary: sample_summary(start, start + 30_000, 55.0 + (i as f64)),
                        trigger_power_action: PowerAction::Suspend,
                        rapid_resume_within_5m: false,
                    },
                    start + 30_000,
                )
                .await
                .unwrap();
        }
        drop(handle);
        worker.shutdown_and_join().unwrap();
    }

    // Capture initial calibration state before boundary
    let initial_cal = {
        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        store.get_calibration_state().unwrap()
    };
    assert_ne!(initial_cal.maturity, CalibrationMaturity::ColdStart);

    // 1) Apply calibration boundary via CLI: rebaseline with Day-1 seed
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "calibration",
            "rebaseline",
            "--seed",
            "52.0",
            "--yes",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Rebaselined calibration"));

    let rebaselined_cal = {
        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        store.get_calibration_state().unwrap()
    };
    assert_eq!(rebaselined_cal.maturity, CalibrationMaturity::ColdStart);
    assert_eq!(rebaselined_cal.day1_seed_sleep_hr, Some(52.0));
    assert_eq!(rebaselined_cal.wearables.len(), 1);

    // 2) Undo calibration boundary via CLI
    let (code, stdout, _) =
        run_inproc_cli(&["bpmute", "calibration", "undo-boundary", "--yes"], &env).await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("restored continuous calibration bit-for-bit"));

    let restored_cal = {
        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        store.get_calibration_state().unwrap()
    };
    // Bit-for-bit restoration!
    assert_eq!(restored_cal, initial_cal);

    // 3) Attempt second undo: must fail with ExitCode::PreconditionConflict (4)
    let (code, _, _) =
        run_inproc_cli(&["bpmute", "calibration", "undo-boundary", "--yes"], &env).await;
    assert_eq!(code, ExitCode::PreconditionConflict);
}

#[tokio::test]
async fn test_data_purge_raw_and_hard_wipe_guardrails() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    // Populate store with wearable, sessions, raw samples, epochs
    {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&db_path, &lock_path, 1_000_000).unwrap();
        let desc = WearableDescriptor {
            mac: "AA:BB:CC:DD:EE:01".to_string(),
            address_type: "public".to_string(),
            alias: Some("PurgeTest".to_string()),
            advertised_name: Some("PurgeTest".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        let w_id = handle.upsert_wearable(desc, 1_000_000).await.unwrap();

        let s1 = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: 1_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();
        handle
            .append_epoch_and_raw(
                s1.clone(),
                Some((
                    0,
                    sample_epoch(1_015_000, 70.0, 0.9, DetectorState::Sleeping),
                )),
                vec![(
                    0,
                    HrPacket {
                        ts_ms: 1_001_000,
                        bpm: 70.0,
                        rr_ms: vec![850.0],
                        contact_status: ContactStatus::Contact,
                    },
                )],
            )
            .await
            .unwrap();
        handle
            .close_session_and_recompute(
                CloseSessionRequest {
                    session_id: s1,
                    summary: sample_summary(1_000_000, 1_030_000, 68.0),
                    trigger_power_action: PowerAction::Suspend,
                    rapid_resume_within_5m: false,
                },
                1_030_000,
            )
            .await
            .unwrap();

        // Active session with raw samples (must be immune from purge-raw)
        let s2 = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: 2_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();
        handle
            .append_epoch_and_raw(
                s2.clone(),
                None,
                vec![(
                    0,
                    HrPacket {
                        ts_ms: 2_001_000,
                        bpm: 72.0,
                        rr_ms: vec![830.0],
                        contact_status: ContactStatus::Contact,
                    },
                )],
            )
            .await
            .unwrap();

        drop(handle);
        worker.shutdown_and_join().unwrap();
    }

    // 1) Test data purge-raw (closed session purged, active session immune)
    let (code, stdout, _) = run_inproc_cli(&["bpmute", "data", "purge-raw", "--yes"], &env).await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Purged raw biometric samples"));

    let stats_after_purge = {
        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        store.get_storage_stats().unwrap()
    };
    assert_eq!(
        stats_after_purge.raw_samples_count, 1,
        "Active session raw sample must be immune from purge"
    );
    assert_eq!(stats_after_purge.sessions_count, 2, "Sessions must remain");
    assert_eq!(
        stats_after_purge.session_epochs_count, 1,
        "Epochs must remain"
    );

    // Write a config file to verify wipe --all config removal behavior
    if let Some(parent) = paths.config_file.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&paths.config_file, "detection_sensitivity = 'balanced'\n").unwrap();
    assert!(paths.config_file.try_exists().unwrap());

    // 2) Test wipe non-interactive confirmation failure: requires --confirm WIPE
    let non_interactive_env = env.clone().with("BPMUTE_NON_INTERACTIVE", "1");
    let (code, _, err) = run_inproc_cli(&["bpmute", "data", "wipe"], &non_interactive_env).await;
    assert_eq!(
        code,
        ExitCode::UsageOrConfigError,
        "missing --confirm WIPE: {err}"
    );

    let (code, _, err) =
        run_inproc_cli(&["bpmute", "data", "wipe", "--confirm", "NOPE"], &env).await;
    assert_eq!(
        code,
        ExitCode::UsageOrConfigError,
        "wrong confirmation keyword: {err}"
    );

    // 3) Test wipe without --all: clears biometric history but keeps wearables
    let (code, stdout, _) =
        run_inproc_cli(&["bpmute", "data", "wipe", "--confirm", "WIPE"], &env).await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Wiped biometric session history"));

    let stats_after_wipe = {
        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        store.get_storage_stats().unwrap()
    };
    assert_eq!(stats_after_wipe.sessions_count, 0);
    assert_eq!(stats_after_wipe.session_epochs_count, 0);
    assert_eq!(
        stats_after_wipe.wearables_count, 1,
        "Enrolled wearable must be preserved without --all"
    );
    assert!(
        paths.config_file.try_exists().unwrap(),
        "Config file must be preserved without --all"
    );

    // 4) Test wipe with --all: also clears enrolled wearables and config file
    let (code, stdout, _) = run_inproc_cli(
        &["bpmute", "data", "wipe", "--all", "--confirm", "WIPE"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Wiped all biometric session history"));
    assert!(
        !paths.config_file.try_exists().unwrap(),
        "Config file must be removed when --all is specified"
    );

    let stats_after_wipe_all = {
        let store = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
        store.get_storage_stats().unwrap()
    };
    assert_eq!(
        stats_after_wipe_all.wearables_count, 0,
        "Wearables wiped with --all"
    );
}

#[tokio::test]
async fn test_json_ledger_bundle_export_import_merge_and_replace_rollback() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&db_path, &lock_path, 5_000_000).unwrap();
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

    // 1) Export ledger bundle
    let bundle_path = temp.path().join("export.json");
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
        &env,
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
    assert_eq!(bundle_json["schema_version"], 1);
    assert_eq!(bundle_json["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(bundle_json["wearables"].as_array().unwrap().len(), 1);
    assert_eq!(bundle_json["session_epochs"].as_array().unwrap().len(), 1);
    assert_eq!(bundle_json["raw_samples"].as_array().unwrap().len(), 1);

    // 2) Set up a second environment for import
    let temp2 = TempDir::new().unwrap();
    let (paths2, env2) = test_paths_and_env(&temp2);
    let db2_path = db_path_in_data_dir(&paths2.data_dir);
    let lock2_path = lock_path_in_data_dir(&paths2.data_dir);

    // Initialize second db
    {
        let (worker, handle, _) = PersistenceWorker::spawn(&db2_path, &lock2_path, 1).unwrap();
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
        &env2,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Imported bundle"));

    let store2 = LedgerStore::open_read_only(&db2_path).unwrap().unwrap();
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
        &env2,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Imported bundle"));
    let store2_re = LedgerStore::open_read_only(&db2_path).unwrap().unwrap();
    assert_eq!(store2_re.get_storage_stats().unwrap().sessions_count, 1);
    drop(store2_re);

    // 3) Import with --mode replace: verifies automatic 0600 rollback backup in backups/
    let backups_dir = backups_dir_in_data_dir(&paths2.data_dir);
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
        &env2,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Rollback backup saved to"));

    // Verify rollback backup file exists and has 0600 permissions
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
async fn test_session_canonical_trace_export_and_replay_compatibility() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    let session_id = {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&db_path, &lock_path, 1_700_000_000_000).unwrap();
        let desc = WearableDescriptor {
            mac: "55:66:77:88:99:AA".to_string(),
            address_type: "public".to_string(),
            alias: Some("TraceWatch".to_string()),
            advertised_name: Some("TraceWatch".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        let w_id = handle
            .upsert_wearable(desc, 1_700_000_000_000)
            .await
            .unwrap();

        let sid = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: 1_700_000_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();

        let mut packets = Vec::new();
        for i in 0..60 {
            packets.push((
                i as u32,
                HrPacket {
                    ts_ms: 1_700_000_000_000 + (i as i64) * 1_000,
                    bpm: 65.0 - (i as f64 * 0.1),
                    rr_ms: vec![920.0],
                    contact_status: ContactStatus::Contact,
                },
            ));
        }

        handle
            .append_epoch_and_raw(sid.clone(), None, packets)
            .await
            .unwrap();

        handle
            .close_session_and_recompute(
                CloseSessionRequest {
                    session_id: sid.clone(),
                    summary: sample_summary(1_700_000_000_000, 1_700_000_060_000, 62.0),
                    trigger_power_action: PowerAction::Suspend,
                    rapid_resume_within_5m: false,
                },
                1_700_000_060_000,
            )
            .await
            .unwrap();

        drop(handle);
        worker.shutdown_and_join().unwrap();
        sid
    };

    // 1) Export canonical CSV trace (anonymized by default)
    let csv_path = temp.path().join("trace.csv");
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "sessions",
            "export",
            &session_id,
            &csv_path.to_string_lossy(),
            "--format",
            "csv",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Exported 60 samples"));

    // Check 0600 permissions
    let csv_mode = fs::metadata(&csv_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(csv_mode, 0o600);

    // Verify CSV header and timestamp shifting (t = 0)
    let csv_lines: Vec<String> = fs::read_to_string(&csv_path)
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(csv_lines[0], "ts_ms,bpm,rr_ms,contact_status,event");
    assert!(csv_lines[1].starts_with("0,65")); // shifted to t = 0

    // 2) Directly replay exported CSV trace through `bpmute replay`
    let (code, stdout, err) = run_inproc_cli(
        &["bpmute", "replay", &csv_path.to_string_lossy(), "--json"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "replay failed: {err}");
    let replay_json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(replay_json["fidelity"], "raw_samples");

    // 3) Export canonical JSONL trace
    let jsonl_path = temp.path().join("trace.jsonl");
    let (code, stdout, _) = run_inproc_cli(
        &[
            "bpmute",
            "sessions",
            "export",
            &session_id,
            &jsonl_path.to_string_lossy(),
            "--format",
            "jsonl",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Exported 60 samples"));

    // Directly replay exported JSONL trace through `bpmute replay`
    let (code, stdout, err) = run_inproc_cli(
        &["bpmute", "replay", &jsonl_path.to_string_lossy(), "--json"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "replay jsonl failed: {err}");
    let replay_jsonl: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(replay_jsonl["fidelity"], "raw_samples");
}

#[tokio::test]
async fn test_sqlite_online_backup_and_stats() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    {
        let (worker, handle, _) =
            PersistenceWorker::spawn(&db_path, &lock_path, 1_000_000).unwrap();
        let desc = WearableDescriptor {
            mac: "66:77:88:99:AA:BB".to_string(),
            address_type: "public".to_string(),
            alias: Some("BackupWatch".to_string()),
            advertised_name: Some("BackupWatch".to_string()),
            supports_rr: true,
            supports_contact: true,
        };
        handle.upsert_wearable(desc, 1_000_000).await.unwrap();
        drop(handle);
        worker.shutdown_and_join().unwrap();
    }

    // 1) Test data backup
    let bkp_path = temp.path().join("backup.db");
    let (code, stdout, _) = run_inproc_cli(
        &["bpmute", "data", "backup", &bkp_path.to_string_lossy()],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    assert!(stdout.contains("Backed up SQLite database"));

    // Check 0600 permissions
    let bkp_mode = fs::metadata(&bkp_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(bkp_mode, 0o600);

    // Open backup with rusqlite and verify integrity
    let conn = Connection::open(&bkp_path).unwrap();
    let check: String = conn
        .query_row("PRAGMA integrity_check;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(check, "ok");

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM wearables;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);

    // 2) Test data stats
    let (code, stdout, _) = run_inproc_cli(&["bpmute", "data", "stats", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let stats_json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(stats_json["wearables_count"], 1);
    assert!(stats_json["db_size_bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_online_cli_sessions_and_data_ipc_roundtrip() {
    use std::sync::Arc;

    use bpmute::config::{CliOverrides, LoadedConfig};
    use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
    use chrono::Utc;

    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let base_ts = Utc::now().timestamp_millis() - 2_500_000;

    let s1_id = {
        let db_path = db_path_in_data_dir(&paths.data_dir);
        let lock_path = lock_path_in_data_dir(&paths.data_dir);
        let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
        let wid = store
            .upsert_wearable(
                &WearableDescriptor {
                    mac: "77:88:99:AA:BB:CC".into(),
                    address_type: "public".into(),
                    alias: Some("OnlineBand".into()),
                    advertised_name: Some("OnlineBand".into()),
                    supports_rr: true,
                    supports_contact: true,
                },
                base_ts - 100_000,
            )
            .unwrap();
        let sid = store
            .open_session(&OpenSessionRequest {
                wearable_id: wid,
                start_ts_ms: base_ts,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .unwrap();
        store
            .append_epoch_and_raw(
                &sid,
                None,
                &[(
                    0,
                    &HrPacket {
                        ts_ms: base_ts + 1_000,
                        bpm: 64.0,
                        rr_ms: vec![920.0],
                        contact_status: ContactStatus::Contact,
                    },
                )],
            )
            .unwrap();
        store
            .close_session_and_recompute(
                &CloseSessionRequest {
                    session_id: sid.clone(),
                    summary: sample_summary(base_ts, base_ts + 2_000_000, 54.0),
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                base_ts + 2_000_000,
            )
            .unwrap();
        sid
    };

    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(Utc::now()));
    let daemon = DaemonHandle::spawn(DaemonOptions::for_testing(
        paths.clone(),
        loaded,
        Arc::new(env.clone()),
        clock,
    ))
    .await
    .unwrap();

    // 1. sessions export (online)
    let trace_path = temp.path().join("online-trace.csv");
    let (code, stdout, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "sessions",
            "export",
            &s1_id,
            &trace_path.to_string_lossy(),
            "--format",
            "csv",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "sessions export stderr: {stderr}");
    assert!(
        stdout.contains("Exported 1 samples"),
        "unexpected sessions export stdout: {stdout}"
    );
    assert!(trace_path.try_exists().unwrap());

    // 2. data export (online)
    let bundle_path = temp.path().join("online-bundle.json");
    let (code, stdout, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "export",
            &bundle_path.to_string_lossy(),
            "--include-raw",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "data export stderr: {stderr}");
    assert!(
        stdout.contains(&bundle_path.to_string_lossy().into_owned()),
        "unexpected data export stdout: {stdout}"
    );
    assert!(bundle_path.try_exists().unwrap());

    // 3. data backup (online)
    let backup_path = temp.path().join("online-backup.db");
    let (code, _, stderr) = run_inproc_cli(
        &["bpmute", "data", "backup", &backup_path.to_string_lossy()],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "data backup stderr: {stderr}");
    assert!(backup_path.try_exists().unwrap());

    // 4. calibration rebaseline (online)
    let (code, _, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "calibration",
            "rebaseline",
            "--seed",
            "52",
            "--yes",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "rebaseline stderr: {stderr}");

    // 5. sessions delete (online)
    let (code, _, stderr) =
        run_inproc_cli(&["bpmute", "sessions", "delete", &s1_id, "--yes"], &env).await;
    assert_eq!(code, ExitCode::Success, "sessions delete stderr: {stderr}");

    // 6. data import (online)
    let (code, stdout, stderr) = run_inproc_cli(
        &[
            "bpmute",
            "data",
            "import",
            &bundle_path.to_string_lossy(),
            "--mode",
            "merge",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "data import stderr: {stderr}");
    assert!(
        stdout.contains("1 sessions"),
        "unexpected data import stdout: {stdout}"
    );

    // 7. data purge-raw (online)
    let (code, stdout, stderr) =
        run_inproc_cli(&["bpmute", "data", "purge-raw", "--yes"], &env).await;
    assert_eq!(code, ExitCode::Success, "data purge-raw stderr: {stderr}");
    assert!(
        stdout.contains("1 rows deleted"),
        "unexpected purge-raw stdout: {stdout}"
    );

    // 8. data wipe (online)
    let (code, _, stderr) =
        run_inproc_cli(&["bpmute", "data", "wipe", "--confirm", "WIPE"], &env).await;
    assert_eq!(code, ExitCode::Success, "data wipe stderr: {stderr}");

    daemon.shutdown().await;
}

#[test]
fn test_purge_and_hard_wipe_reclaim_storage_and_truncate_wal() {
    let temp = TempDir::new().unwrap();
    let (paths, _env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let wal_path = std::path::PathBuf::from(format!("{}-wal", db_path.display()));
    let wal_len = || std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();

    // Populate wearable + 2 closed sessions with 200 raw samples each
    let wid = store
        .upsert_wearable(
            &WearableDescriptor {
                mac: "AA:BB:CC:DD:EE:01".to_string(),
                address_type: "public".to_string(),
                alias: Some("Band".to_string()),
                advertised_name: None,
                supports_rr: true,
                supports_contact: true,
            },
            1000,
        )
        .unwrap();
    for s_idx in 0..2 {
        let base = (s_idx + 1) * 1_000_000;
        let sid = store
            .open_session(&OpenSessionRequest {
                wearable_id: wid.clone(),
                start_ts_ms: base,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .unwrap();
        let raw: Vec<(u32, HrPacket)> = (0..200)
            .map(|i| {
                (
                    i,
                    HrPacket {
                        ts_ms: base + (i as i64) * 1000,
                        bpm: 65.0,
                        rr_ms: vec![920.0],
                        contact_status: ContactStatus::Contact,
                    },
                )
            })
            .collect();
        let raw_refs: Vec<(u32, &HrPacket)> = raw.iter().map(|(seq, p)| (*seq, p)).collect();
        store.append_epoch_and_raw(&sid, None, &raw_refs).unwrap();
        store
            .close_session_and_recompute(
                &CloseSessionRequest {
                    session_id: sid,
                    summary: sample_summary(base, base + 200_000, 56.0),
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                base + 200_000,
            )
            .unwrap();
    }

    let size_before_purge = store.get_storage_stats().unwrap().db_size_bytes;
    let purged = store.purge_raw_samples().unwrap();
    assert_eq!(purged, 400);
    assert_eq!(wal_len(), 0, "purge_raw_samples must truncate WAL");
    let after_purge_stats = store.get_storage_stats().unwrap();
    assert_eq!(after_purge_stats.wal_size_bytes, 0);
    let size_after_purge = after_purge_stats.db_size_bytes;
    assert!(
        size_after_purge < size_before_purge,
        "purge_raw_samples must shrink DB size: before={size_before_purge}, after={size_after_purge}"
    );

    // Repeating purge when 0 closed raw samples remain must be a no-op on DB size.
    let purged2 = store.purge_raw_samples().unwrap();
    assert_eq!(purged2, 0);
    assert_eq!(wal_len(), 0);
    let size_after_purge2 = store.get_storage_stats().unwrap().db_size_bytes;
    assert_eq!(size_after_purge2, size_after_purge);

    // Soft rebaseline preserves sessions while resetting maturity to ColdStart.
    let (_b1_rec, cal_b1) = store
        .set_calibration_boundary(None, Some(52.0), true, 3_000_000)
        .unwrap();
    assert_eq!(cal_b1.valid_sessions, 0);
    assert_eq!(cal_b1.maturity, CalibrationMaturity::ColdStart);

    // Repeated hard_wipe calls must reclaim space, truncate WAL, and stay constant in size.
    let size_before_wipe = store.get_storage_stats().unwrap().db_size_bytes;
    let cal_w1 = store.hard_wipe(false, 5_000_000).unwrap();
    assert_eq!(cal_w1.valid_sessions, 0);
    assert!(
        cal_w1.wearables.contains_key(&wid),
        "hard_wipe(false) must preserve enrolled wearable calibration metadata"
    );
    assert_eq!(wal_len(), 0, "hard_wipe must truncate WAL");
    let after_wipe1_stats = store.get_storage_stats().unwrap();
    assert_eq!(after_wipe1_stats.wal_size_bytes, 0);
    let size_after_wipe1 = after_wipe1_stats.db_size_bytes;
    assert!(
        size_after_wipe1 < size_before_wipe,
        "first hard_wipe must shrink DB: before={size_before_wipe}, after={size_after_wipe1}"
    );

    store.hard_wipe(false, 6_000_000).unwrap();
    assert_eq!(wal_len(), 0);
    let size_after_wipe2 = store.get_storage_stats().unwrap().db_size_bytes;
    assert_eq!(
        size_after_wipe2, size_after_wipe1,
        "second hard_wipe must not grow DB: w1={size_after_wipe1}, w2={size_after_wipe2}"
    );

    store.hard_wipe(false, 7_000_000).unwrap();
    assert_eq!(wal_len(), 0);
    let size_after_wipe3 = store.get_storage_stats().unwrap().db_size_bytes;
    assert_eq!(
        size_after_wipe3, size_after_wipe2,
        "third hard_wipe must not grow DB: w2={size_after_wipe2}, w3={size_after_wipe3}"
    );
}
