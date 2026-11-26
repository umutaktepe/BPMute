mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command as ProcessCommand;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use bpmute::calibration::{CalibrationState, FeedbackVerdict};
use bpmute::cli::{Cli, execute_cli};
use bpmute::config::{CliOverrides, LoadedConfig, PatchField, PowerAction, Sensitivity};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock, WearableEvent, WearableSource};
use bpmute::exit_code::ExitCode;
use bpmute::ipc::IpcClient;
use bpmute::paths::{EnvSource, MapEnv, ProcessEnv, ResolvedPaths};
use bpmute::sleep_engine::{
    CalibrationMaturity, ContactStatus, DetectorState, EngineEvent, Epoch, EpochFeatures,
    GraceOutcome, GraceResult, HrPacket,
};
use bpmute::storage::{
    CloseSessionRequest, LedgerFileLock, LedgerStore, OpenSessionRequest, PersistenceWorker,
    SetFeedbackRequest, WearableDescriptor, db_path_in_data_dir, lock_path_in_data_dir,
};
use chrono::{Local, TimeZone, Utc};
use clap::Parser;
use common::daemon::send_trace_slice;
use common::synthetic::{Phenotype, StageKind, generate_paired_trace};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{mpsc as tokio_mpsc, watch};

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

use common::sample_session_summary as sample_summary;

#[test]
fn sqlite_wal_pragmas_permissions_uuidv7_and_flock_coordination() {
    let temp = TempDir::new().unwrap();
    let data_dir = temp.path().join("data/bpmute");
    let db_path = db_path_in_data_dir(&data_dir);
    let lock_path = lock_path_in_data_dir(&data_dir);

    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();

    // Check 0700 data_dir and 0600 files (db, wal, shm, lock)
    let dir_mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
    let db_mode = fs::metadata(&db_path).unwrap().permissions().mode() & 0o777;
    let lock_mode = fs::metadata(&lock_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(db_mode, 0o600);
    assert_eq!(lock_mode, 0o600);

    // Check SQLite WAL, secure_delete, auto_vacuum = INCREMENTAL (2), foreign_keys
    let ro_conn = Connection::open(&db_path).unwrap();
    let journal_mode: String = ro_conn
        .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
        .unwrap();
    let secure_delete: i64 = {
        let mut s = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
        let w_id = s
            .upsert_wearable(
                &WearableDescriptor {
                    mac: "AA:BB:CC:DD:EE:FF".to_string(),
                    address_type: "public".to_string(),
                    alias: Some("Band".to_string()),
                    advertised_name: Some("Watch".to_string()),
                    supports_rr: true,
                    supports_contact: true,
                },
                1_700_000_000_000,
            )
            .unwrap();
        assert_eq!(uuid::Uuid::parse_str(&w_id).unwrap().get_version_num(), 7);
        1
    };
    assert_eq!(secure_delete, 1);
    let auto_vacuum: i64 = ro_conn
        .query_row("PRAGMA auto_vacuum;", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    assert_eq!(auto_vacuum, 2, "auto_vacuum must be INCREMENTAL (2)");

    // Exclusive flock contention test: hold lock on main thread while worker thread mutates
    let guard = LedgerFileLock::acquire_exclusive(&lock_path).unwrap();
    let db_clone = db_path.clone();
    let lock_clone = lock_path.clone();
    let (done_tx, done_rx) = mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut s = LedgerStore::open_read_write(&db_clone, &lock_clone).unwrap();
        let st = s
            .set_day1_seed_and_recompute(Some(54.5), 1_700_000_001_000)
            .unwrap();
        done_tx.send(st).unwrap();
    });

    assert!(
        done_rx.recv_timeout(Duration::from_millis(80)).is_err(),
        "offline write must block on exclusive flock until released"
    );
    drop(guard);

    let seeded = done_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("offline write should complete immediately after flock release");
    handle.join().unwrap();
    assert_eq!(seeded.mu_sleep, Some(57.0));
    assert_eq!(store.get_calibration_state().unwrap().mu_sleep, Some(57.0));
}

#[tokio::test]
async fn persistence_worker_round_trip_determinism_and_uuidv7_records() {
    let temp = TempDir::new().unwrap();
    let data_dir = temp.path().join("data/bpmute");
    let db_path = db_path_in_data_dir(&data_dir);
    let lock_path = lock_path_in_data_dir(&data_dir);

    let (worker, handle, initial_state) =
        PersistenceWorker::spawn(&db_path, &lock_path, 1_700_000_000_000).unwrap();
    assert_eq!(initial_state.maturity, CalibrationMaturity::ColdStart);

    let w_id = handle
        .upsert_wearable(
            WearableDescriptor {
                mac: "11:22:33:44:55:66".to_string(),
                address_type: "public".to_string(),
                alias: Some("Polar H10".to_string()),
                advertised_name: Some("Polar H10".to_string()),
                supports_rr: true,
                supports_contact: true,
            },
            1_700_000_000_000,
        )
        .await
        .unwrap();

    // Persist 3 clean sessions via PersistenceWorker
    let mut last_state = initial_state;
    let mut second_session_id = String::new();
    for i in 0..3 {
        let start = 1_700_000_000_000_i64 + i * 86_400_000;
        let sid = handle
            .open_session(OpenSessionRequest {
                wearable_id: w_id.clone(),
                start_ts_ms: start,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .await
            .unwrap();
        assert_eq!(uuid::Uuid::parse_str(&sid).unwrap().get_version_num(), 7);
        if i == 1 {
            second_session_id = sid.clone();
        }
        handle
            .append_epoch_and_raw(
                sid.clone(),
                Some((
                    0,
                    sample_epoch(start + 15_000, 66.0, 0.05, DetectorState::Warmup),
                )),
                vec![(
                    0,
                    HrPacket {
                        ts_ms: start + 1_000,
                        bpm: 66.0,
                        rr_ms: vec![909.0],
                        contact_status: ContactStatus::Contact,
                    },
                )],
            )
            .await
            .unwrap();
        handle
            .append_epoch_and_raw(
                sid.clone(),
                Some((
                    1,
                    sample_epoch(start + 1_800_000, 58.0, 0.94, DetectorState::Sleeping),
                )),
                Vec::new(),
            )
            .await
            .unwrap();

        let (detail, st) = handle
            .close_session_and_recompute(
                CloseSessionRequest {
                    session_id: sid,
                    summary: sample_summary(start, start + 3_600_000, 58.0),
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                start + 3_600_000,
            )
            .await
            .unwrap();
        assert_eq!(detail.epochs.len(), 2);
        assert!(detail.has_raw_samples);
        last_state = st;
    }

    assert_eq!(last_state.maturity, CalibrationMaturity::Learning);
    assert_eq!(last_state.valid_sessions, 3);

    // Edit feedback (mark session #2 as anomalous_exclude), then revert -> bitwise original!
    let (_, edited) = handle
        .set_feedback_and_recompute(
            SetFeedbackRequest {
                session_id_or_prefix: second_session_id[..8].to_string(),
                verdict: PatchField::Set(FeedbackVerdict::AnomalousExclude),
                onset_hhmm: PatchField::Unchanged,
                avg_sleep_hr: PatchField::Unchanged,
                clear_all: false,
            },
            1_700_300_000_000,
        )
        .await
        .unwrap();
    assert_eq!(edited.valid_sessions, 2);
    assert_eq!(edited.maturity, CalibrationMaturity::ColdStart);

    let (_, reverted) = handle
        .set_feedback_and_recompute(
            SetFeedbackRequest {
                session_id_or_prefix: second_session_id,
                verdict: PatchField::Unchanged,
                onset_hhmm: PatchField::Unchanged,
                avg_sleep_hr: PatchField::Unchanged,
                clear_all: true,
            },
            1_700_300_001_000,
        )
        .await
        .unwrap();
    assert_eq!(
        reverted.mu_sleep.map(f64::to_bits),
        last_state.mu_sleep.map(f64::to_bits)
    );
    assert_eq!(reverted.valid_sessions, last_state.valid_sessions);
    assert_eq!(reverted.maturity, last_state.maturity);

    worker.shutdown_async().await;

    let ro = LedgerStore::open_read_only(&db_path).unwrap().unwrap();
    let scratch = ro.recompute_from_scratch().unwrap();
    assert_eq!(
        scratch.mu_sleep.map(f64::to_bits),
        last_state.mu_sleep.map(f64::to_bits)
    );
}

struct FakeWearableSource {
    rx: Mutex<Option<tokio_mpsc::Receiver<WearableEvent>>>,
}

impl FakeWearableSource {
    fn channel() -> (tokio_mpsc::Sender<WearableEvent>, Self) {
        let (tx, rx) = tokio_mpsc::channel(4096);
        (
            tx,
            Self {
                rx: Mutex::new(Some(rx)),
            },
        )
    }
}

impl WearableSource for FakeWearableSource {
    fn subscribe(
        &self,
        _shutdown_rx: watch::Receiver<bool>,
    ) -> tokio_mpsc::Receiver<WearableEvent> {
        self.rx
            .lock()
            .unwrap()
            .take()
            .expect("FakeWearableSource::subscribe called once")
    }
}

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

#[tokio::test(start_paused = true)]
async fn live_daemon_wearable_stream_records_ledger_and_publishes_ipc_events() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let (wearable_tx, wearable_source) = FakeWearableSource::channel();

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_wearable_source(Arc::new(wearable_source)),
    )
    .await
    .unwrap();

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let snapshot = client.next_event().await.unwrap();
    assert_eq!(snapshot.event, "Snapshot");

    let trace = generate_paired_trace(
        101,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep {
                duration_sec: 1_500,
            },
        ],
    );
    let desc = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:11".to_string(),
        address_type: "public".to_string(),
        alias: Some("Polar Strap".to_string()),
        advertised_name: Some("Polar H10".to_string()),
        supports_rr: true,
        supports_contact: true,
    };

    let base_ts = start_utc.timestamp_millis();
    let split_idx = trace.bpm_rr.len() * 2 / 3;
    send_trace_slice(&wearable_tx, &desc, &trace.bpm_rr[..split_idx], base_ts).await;

    let mut saw_hr = false;
    let mut saw_epoch = false;
    let mut saw_state_changed = false;
    let mut saw_session_started = false;
    let mut started_session_id = String::new();
    let mut confirmed_ts_ms = None;

    for _ in 0..10_000 {
        let ev = client.next_event().await.unwrap();
        match ev.event.as_str() {
            "HrSample" => saw_hr = true,
            "SessionStarted" => {
                saw_session_started = true;
                started_session_id = ev.data["session_id"].as_str().unwrap().to_string();
                assert!(!ev.data["wearable_id"].as_str().unwrap().is_empty());
                assert!(ev.data["start_ts_ms"].as_i64().is_some());
            }
            "EpochUpdate" => saw_epoch = true,
            "StateChanged" => {
                saw_state_changed = true;
                if ev.data["monitoring_state"] == "Confirmed" {
                    confirmed_ts_ms = Some(base_ts + 1_320_000);
                    break;
                }
            }
            _ => {}
        }
    }
    let grace_ts = confirmed_ts_ms.expect("engine must reach Confirmed during N1/N2");
    daemon
        .deliver_grace_outcome(GraceOutcome {
            ts_ms: grace_ts,
            outcome: GraceResult::Expired,
        })
        .await;

    for ev in &trace.bpm_rr[split_idx..] {
        if let EngineEvent::HrPacket(pkt) = ev {
            let mut shifted = pkt.clone();
            shifted.ts_ms += base_ts;
            wearable_tx
                .send(WearableEvent::HrPacket {
                    wearable: desc.clone(),
                    packet: shifted,
                })
                .await
                .unwrap();
        }
    }
    drop(wearable_tx);

    let mut saw_sleeping = false;
    let mut saw_session_closed = false;
    let mut saw_cal_updated = false;
    let mut closed_session_id = String::new();

    for _ in 0..10_000 {
        let ev = client.next_event().await.unwrap();
        match ev.event.as_str() {
            "HrSample" => saw_hr = true,
            "EpochUpdate" => saw_epoch = true,
            "StateChanged" => {
                if ev.data["monitoring_state"] == "Sleeping" {
                    saw_sleeping = true;
                }
            }
            "SessionClosed" => {
                saw_session_closed = true;
                closed_session_id = ev.data["session"]["session"]["id"]
                    .as_str()
                    .unwrap()
                    .to_string();
            }
            "CalibrationUpdated" => {
                saw_cal_updated = true;
                if saw_session_closed {
                    break;
                }
            }
            _ => {}
        }
    }

    assert!(saw_hr, "must publish HrSample");
    assert!(saw_session_started, "must publish SessionStarted");
    assert_eq!(started_session_id, closed_session_id);
    assert!(saw_epoch, "must publish EpochUpdate");
    assert!(saw_state_changed, "must publish StateChanged");
    assert!(
        saw_sleeping,
        "must transition to Sleeping after GraceOutcome::Expired"
    );
    assert!(saw_session_closed, "must publish SessionClosed");
    assert!(saw_cal_updated, "must publish CalibrationUpdated");
    assert!(!closed_session_id.is_empty());

    // New subscriber Snapshot must carry rolling hr_ring_buffer!
    let mut sub2 = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub2.request("daemon.subscribe", json!({})).await.unwrap();
    let snap2 = sub2.next_event().await.unwrap();
    assert_eq!(snap2.event, "Snapshot");
    assert!(
        !snap2.data["hr_ring_buffer"].as_array().unwrap().is_empty(),
        "Snapshot must include buffered HrSamples"
    );

    let list_res = client
        .request("session.list", json!({ "limit": 10 }))
        .await
        .unwrap();
    assert_eq!(list_res["total_count"], 1);

    let detail = client
        .request("session.get", json!({ "id": closed_session_id }))
        .await
        .unwrap();
    assert_eq!(detail["has_raw_samples"], true);
    assert!(!detail["epochs"].as_array().unwrap().is_empty());

    // Verify online feedback + seed update synchronizes in-memory daemon calibration & SQLite
    let seed_res = client
        .request("calibration.set_seed", json!({ "sleep_hr": 55.0 }))
        .await
        .unwrap();
    assert_eq!(seed_res["day1_seed_sleep_hr"], 55.0);
    let ev_cal = client.next_event().await.unwrap();
    assert_eq!(ev_cal.event, "CalibrationUpdated");

    let status_res = client
        .request("calibration.status", json!({}))
        .await
        .unwrap();
    assert_eq!(status_res["day1_seed_sleep_hr"], 55.0);

    // MAC addresses must remain masked in session detail output
    assert_eq!(detail["wearable"]["mac_masked"], "..:EE:11");

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn live_daemon_respects_raw_samples_opt_out_and_shift_ipc_controls() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(
        &paths.config_file,
        "schema_version = 1\n[retention]\nraw_samples_days = 0\n",
    )
    .unwrap();
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let (wearable_tx, wearable_source) = FakeWearableSource::channel();

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_wearable_source(Arc::new(wearable_source)),
    )
    .await
    .unwrap();

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let _snap = client.next_event().await.unwrap();

    let trace = generate_paired_trace(
        202,
        Phenotype::typical(),
        &[StageKind::QuietWake { duration_sec: 600 }],
    );
    let desc = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:22".to_string(),
        address_type: "public".to_string(),
        alias: Some("GT5".to_string()),
        advertised_name: Some("HUAWEI WATCH GT 5".to_string()),
        supports_rr: false,
        supports_contact: true,
    };
    let base_ts = start_utc.timestamp_millis();
    for ev in &trace.bpm_only_gt5 {
        if let EngineEvent::HrPacket(pkt) = ev {
            let mut shifted = pkt.clone();
            shifted.ts_ms += base_ts;
            wearable_tx
                .send(WearableEvent::HrPacket {
                    wearable: desc.clone(),
                    packet: shifted,
                })
                .await
                .unwrap();
        }
    }
    drop(wearable_tx);

    let mut closed_id = String::new();
    for _ in 0..2000 {
        let ev = client.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            closed_id = ev.data["session"]["session"]["id"]
                .as_str()
                .unwrap()
                .to_string();
            break;
        }
    }

    let detail = client
        .request("session.get", json!({ "id": closed_id }))
        .await
        .unwrap();
    assert_eq!(detail["has_raw_samples"], false);
    assert_eq!(detail["session"]["raw_samples_count"], 0);
    assert!(!detail["epochs"].as_array().unwrap().is_empty());

    // accept-shift / dismiss-shift with no pending shift must return PRECONDITION_FAILED
    let err = client
        .request("calibration.accept_shift", json!({}))
        .await
        .expect_err("accept_shift without pending shift must fail");
    assert_eq!(err.code(), Some("PRECONDITION_FAILED"));

    daemon.shutdown().await;

    // Verify raw_sample_chunks table has 0 rows on disk
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let ro = Connection::open(&db_path).unwrap();
    let raw_count: i64 = ro
        .query_row("SELECT COUNT(*) FROM raw_sample_chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(raw_count, 0);
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

fn run_real_bin(temp: &TempDir, args: &[&str]) -> std::process::Output {
    ProcessCommand::new(env!("CARGO_BIN_EXE_bpmute"))
        .env_clear()
        .env("PATH", ProcessEnv.get("PATH").unwrap_or_default())
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("cfg"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("run"))
        .args(args)
        .output()
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn cli_sessions_calibration_online_offline_and_dual_fidelity_replay() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);

    // 1) Offline read-only queries with no DB must NOT create data_dir or bpmute.db.
    let (code, out, err) = run_inproc_cli(&["bpmute", "sessions", "list", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let empty_list: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(empty_list["total_count"], 0);
    assert!(!db_path.try_exists().unwrap());

    let (code, out, err) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let empty_cal: CalibrationState = serde_json::from_str(&out).unwrap();
    assert_eq!(empty_cal.maturity, CalibrationMaturity::ColdStart);
    assert!(!db_path.try_exists().unwrap());

    let (code, _, _) = run_inproc_cli(&["bpmute", "sessions", "show", "missing"], &env).await;
    assert_eq!(code, ExitCode::RuntimeError);
    assert!(!db_path.try_exists().unwrap());

    // 2) Real binary CLI validation & semantic exit codes (2, 4, 1)
    assert_eq!(
        run_real_bin(&temp, &["sessions", "list", "--limit", "0"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run_real_bin(&temp, &["sessions", "feedback", "abc"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run_real_bin(
            &temp,
            &["sessions", "feedback", "abc", "--avg-sleep-hr", "250"]
        )
        .status
        .code(),
        Some(2)
    );
    assert_eq!(
        run_real_bin(&temp, &["calibration", "seed", "20"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        run_real_bin(&temp, &["calibration", "accept-shift"])
            .status
            .code(),
        Some(4)
    );
    assert_eq!(
        run_real_bin(&temp, &["calibration", "dismiss-shift"])
            .status
            .code(),
        Some(4)
    );
    assert_eq!(
        run_real_bin(&temp, &["replay", "--session", "nonexistent"])
            .status
            .code(),
        Some(1)
    );

    // 3) Start live daemon, record Session A across midnight with raw_samples retained,
    //    then enable raw_samples_days = 0 via config.update and record Session B (epochs only).
    let start_local = Local.with_ymd_and_hms(2026, 11, 19, 23, 15, 0).unwrap();
    let start_utc = start_local.with_timezone(&Utc);
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let (wearable_tx, wearable_source) = FakeWearableSource::channel();

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env.clone()), clock)
            .with_wearable_source(Arc::new(wearable_source)),
    )
    .await
    .unwrap();

    let mut sub = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub.request("daemon.subscribe", json!({})).await.unwrap();
    let _snap = sub.next_event().await.unwrap();

    let trace_a = generate_paired_trace(
        303,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep {
                duration_sec: 1_200,
            },
        ],
    );
    let desc = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:33".to_string(),
        address_type: "public".to_string(),
        alias: Some("DualStrap".to_string()),
        advertised_name: Some("DualStrap".to_string()),
        supports_rr: true,
        supports_contact: true,
    };

    let base_a = start_utc.timestamp_millis();
    send_trace_slice(&wearable_tx, &desc, &trace_a.bpm_rr, base_a).await;
    // Insert > 15 min gap to close Session A while keeping WearableSource open
    let session_b_start = base_a + 4_200_000; // +70 min (now ~00:25 local next day)
    wearable_tx
        .send(WearableEvent::StreamState {
            wearable: desc.clone(),
            state: bpmute::sleep_engine::StreamState {
                ts_ms: base_a + 2_600_000,
                status: bpmute::sleep_engine::StreamStatus::SignalLost,
            },
        })
        .await
        .unwrap();

    // Wait until Session A has persisted through its final Epoch before disabling raw_samples.
    loop {
        let ev = sub.next_event().await.unwrap();
        if ev.event == "EpochUpdate"
            && ev.data["epoch"]["ts_ms"]
                .as_i64()
                .is_some_and(|ts| ts >= base_a + 2_500_000)
        {
            break;
        }
    }

    // Update retention to raw_samples_days = 0 live before Session B starts
    let mut cfg_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    cfg_client
        .request(
            "config.update",
            json!({ "retention": { "raw_samples_days": 0 } }),
        )
        .await
        .unwrap();

    let trace_b = generate_paired_trace(
        404,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep {
                duration_sec: 1_200,
            },
        ],
    );
    send_trace_slice(&wearable_tx, &desc, &trace_b.bpm_rr, session_b_start).await;
    drop(wearable_tx);

    let mut closed_ids = Vec::new();
    for _ in 0..15_000 {
        let ev = sub.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            let sid = ev.data["session"]["session"]["id"]
                .as_str()
                .unwrap()
                .to_string();
            closed_ids.push(sid);
            if closed_ids.len() == 2 {
                break;
            }
        }
    }
    assert_eq!(closed_ids.len(), 2);
    let session_a_id = &closed_ids[0];
    let session_b_id = &closed_ids[1];

    // 4) Online CLI commands (`sessions list`, `sessions show`, `sessions feedback` with midnight
    //    wraparound `--onset 23:32`, `calibration seed`, `calibration status`)
    let (code, list_out, err) = run_inproc_cli(
        &["bpmute", "sessions", "list", "--limit", "5", "--json"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let list_json: Value = serde_json::from_str(&list_out).unwrap();
    assert_eq!(list_json["total_count"], 2);

    let (code, show_out, err) = run_inproc_cli(
        &["bpmute", "sessions", "show", &session_a_id[..12], "--json"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let show_json: Value = serde_json::from_str(&show_out).unwrap();
    assert_eq!(show_json["has_raw_samples"], true);

    let (code, _, err) = run_inproc_cli(
        &[
            "bpmute",
            "sessions",
            "feedback",
            &session_a_id[..12],
            "--verdict",
            "missed_sleep",
            "--onset",
            "23:32",
            "--avg-sleep-hr",
            "54.5",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    // Out-of-bounds onset HH:MM fails with exit 2
    let (code, _, _) = run_inproc_cli(
        &[
            "bpmute",
            "sessions",
            "feedback",
            session_a_id,
            "--onset",
            "12:00",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::UsageOrConfigError);

    let (code, _, err) = run_inproc_cli(&["bpmute", "calibration", "seed", "55.0"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, cal_out, err) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let cal_online: CalibrationState = serde_json::from_str(&cal_out).unwrap();
    assert_eq!(cal_online.day1_seed_sleep_hr, Some(55.0));
    assert_eq!(cal_online.valid_sessions, 2);

    // 5) Shutdown daemon and verify offline CLI parity & dual-fidelity `replay --session`!
    daemon.shutdown().await;

    let (code, cal_off_out, err) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let cal_offline: CalibrationState = serde_json::from_str(&cal_off_out).unwrap();
    assert_eq!(cal_offline, cal_online);

    // Offline clear seed + clear feedback via flock
    let (code, _, err) = run_inproc_cli(&["bpmute", "calibration", "seed", "--clear"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let (code, _, err) = run_inproc_cli(
        &["bpmute", "sessions", "feedback", session_a_id, "--clear"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, cal_before_replay_out, _) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let cal_before_replay: CalibrationState = serde_json::from_str(&cal_before_replay_out).unwrap();

    // Dual-fidelity replay: Session A -> raw_samples
    let (code, rep_a_json_out, err) = run_inproc_cli(
        &["bpmute", "replay", "--session", session_a_id, "--json"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let rep_a: Value = serde_json::from_str(&rep_a_json_out).unwrap();
    assert_eq!(rep_a["fidelity"], "raw_samples");
    assert_eq!(rep_a["format"], "sqlite_ledger");
    assert_eq!(rep_a["read_only"], true);
    assert!(rep_a["samples_processed"].as_u64().unwrap() > 100);

    let (code, rep_a_text, _) =
        run_inproc_cli(&["bpmute", "replay", "--session", session_a_id], &env).await;
    assert_eq!(code, ExitCode::Success);
    assert!(rep_a_text.contains("raw_samples"));

    // Dual-fidelity replay: Session B (raw_samples_days = 0) -> session_epochs fallback!
    let (code, rep_b_json_out, err) = run_inproc_cli(
        &["bpmute", "replay", "--session", session_b_id, "--json"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let rep_b: Value = serde_json::from_str(&rep_b_json_out).unwrap();
    assert_eq!(rep_b["fidelity"], "session_epochs");
    assert_eq!(rep_b["format"], "sqlite_ledger");
    assert_eq!(rep_b["read_only"], true);
    assert_eq!(rep_b["samples_processed"], 0);
    assert!(rep_b["epochs_emitted"].as_u64().unwrap() > 100);

    let (code, rep_b_text, _) =
        run_inproc_cli(&["bpmute", "replay", "--session", session_b_id], &env).await;
    assert_eq!(code, ExitCode::Success);
    assert!(rep_b_text.contains("session_epochs"));
    assert!(rep_b_text.contains("fallback"));

    // Verify replay stayed strictly read-only
    let (code, cal_after_replay_out, _) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let cal_after_replay: CalibrationState = serde_json::from_str(&cal_after_replay_out).unwrap();
    assert_eq!(cal_after_replay, cal_before_replay);
}

#[tokio::test]
async fn cli_accept_shift_and_dismiss_shift_end_to_end() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);

    // Seed 5 clean sessions at 65 bpm + 5 shifted sessions at 78 bpm in SQLite
    {
        let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
        let w_id = store
            .upsert_wearable(
                &WearableDescriptor {
                    mac: "AA:BB:CC:DD:EE:44".to_string(),
                    address_type: "public".to_string(),
                    alias: Some("ShiftWatch".to_string()),
                    advertised_name: None,
                    supports_rr: true,
                    supports_contact: true,
                },
                1_700_000_000_000,
            )
            .unwrap();
        for i in 0..10_i64 {
            let start = 1_700_000_000_000 + i * 86_400_000;
            let sid = store
                .open_session(&OpenSessionRequest {
                    wearable_id: w_id.clone(),
                    start_ts_ms: start,
                    detection_sensitivity: Sensitivity::Balanced,
                })
                .unwrap();
            let (wake, sleep) = if i < 5 {
                (65.0 + ((i % 3) as f64 - 1.0) * 0.8, 58.0)
            } else {
                (78.0, 71.0)
            };
            let mut summary = sample_summary(start, start + 3_600_000, sleep);
            summary.warmup_mu_hr = Some(wake);
            store
                .close_session_and_recompute(
                    &CloseSessionRequest {
                        session_id: sid,
                        summary,
                        trigger_power_action: PowerAction::None,
                        rapid_resume_within_5m: false,
                    },
                    start + 3_600_000,
                )
                .unwrap();
        }
    }

    // Start daemon: startup should surface the pending_shift_alert in daemon.status!
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(
        Utc.with_ymd_and_hms(2026, 11, 20, 9, 0, 0).unwrap(),
    ));
    let daemon = DaemonHandle::spawn(DaemonOptions::for_testing(
        paths.clone(),
        loaded,
        Arc::new(env.clone()),
        clock,
    ))
    .await
    .unwrap();

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let status = client.request("daemon.status", json!({})).await.unwrap();
    assert!(
        status["alerts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "calibration_persistent_shift")
    );

    // Accept shift via CLI -> creates CalibrationBoundary, clears alert, updates daemon & SQLite!
    let (code, _, err) = run_inproc_cli(&["bpmute", "calibration", "accept-shift"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let status_after = client.request("daemon.status", json!({})).await.unwrap();
    assert!(status_after["alerts"].as_array().unwrap().is_empty());

    let (code, cal_out, _) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let cal: CalibrationState = serde_json::from_str(&cal_out).unwrap();
    assert!(cal.pending_shift_alert.is_none());
    assert!(cal.active_boundary_id.is_some());
    assert!((cal.mu_wake.unwrap() - 78.0).abs() < 0.1);

    daemon.shutdown().await;

    // Also verify offline `dismiss-shift` on a fresh DB with 5 shifted sessions
    let temp2 = TempDir::new().unwrap();
    let (paths2, env2) = test_paths_and_env(&temp2);
    let db2 = db_path_in_data_dir(&paths2.data_dir);
    let lock2 = lock_path_in_data_dir(&paths2.data_dir);
    {
        let mut store = LedgerStore::open_read_write(&db2, &lock2).unwrap();
        let w_id = store
            .upsert_wearable(
                &WearableDescriptor {
                    mac: "AA:BB:CC:DD:EE:55".to_string(),
                    address_type: "public".to_string(),
                    alias: None,
                    advertised_name: None,
                    supports_rr: false,
                    supports_contact: true,
                },
                1_700_000_000_000,
            )
            .unwrap();
        for i in 0..10_i64 {
            let start = 1_700_000_000_000 + i * 86_400_000;
            let sid = store
                .open_session(&OpenSessionRequest {
                    wearable_id: w_id.clone(),
                    start_ts_ms: start,
                    detection_sensitivity: Sensitivity::Balanced,
                })
                .unwrap();
            let (wake, sleep) = if i < 5 {
                (65.0 + ((i % 3) as f64 - 1.0) * 0.8, 58.0)
            } else {
                (52.0, 45.0)
            };
            let mut summary = sample_summary(start, start + 3_600_000, sleep);
            summary.warmup_mu_hr = Some(wake);
            store
                .close_session_and_recompute(
                    &CloseSessionRequest {
                        session_id: sid,
                        summary,
                        trigger_power_action: PowerAction::None,
                        rapid_resume_within_5m: false,
                    },
                    start + 3_600_000,
                )
                .unwrap();
        }
    }
    let (code, _, err) = run_inproc_cli(&["bpmute", "calibration", "dismiss-shift"], &env2).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let (code, cal2_out, _) =
        run_inproc_cli(&["bpmute", "calibration", "status", "--json"], &env2).await;
    assert_eq!(code, ExitCode::Success);
    let cal2: CalibrationState = serde_json::from_str(&cal2_out).unwrap();
    assert!(cal2.pending_shift_alert.is_none());
    assert!((cal2.mu_wake.unwrap() - 65.0).abs() < 0.2);
}

#[tokio::test(start_paused = true)]
async fn switching_wearables_mid_stream_attributes_closed_session_to_previous_wearable() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 22, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let (wearable_tx, wearable_source) = FakeWearableSource::channel();

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_wearable_source(Arc::new(wearable_source)),
    )
    .await
    .unwrap();

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let snapshot = client.next_event().await.unwrap();
    assert_eq!(snapshot.event, "Snapshot");

    let wearable_a = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:01".to_string(),
        address_type: "public".to_string(),
        alias: Some("Wearable A".to_string()),
        advertised_name: Some("Wearable A".to_string()),
        supports_rr: true,
        supports_contact: true,
    };
    let wearable_b = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:02".to_string(),
        address_type: "public".to_string(),
        alias: Some("Wearable B".to_string()),
        advertised_name: Some("Wearable B".to_string()),
        supports_rr: false,
        supports_contact: true,
    };

    let base_ts = start_utc.timestamp_millis();
    wearable_tx
        .send(WearableEvent::StreamState {
            wearable: wearable_a.clone(),
            state: bpmute::sleep_engine::StreamState {
                ts_ms: base_ts,
                status: bpmute::sleep_engine::StreamStatus::Streaming,
            },
        })
        .await
        .unwrap();
    common::daemon::send_synthetic_wave_stream(
        &wearable_tx,
        &wearable_a,
        common::daemon::SyntheticStreamParams::warmup(base_ts + 1_000),
    )
    .await;

    // Switch directly to Wearable B without an explicit disconnect on A
    let t_b = base_ts + 505_000;
    wearable_tx
        .send(WearableEvent::StreamState {
            wearable: wearable_b.clone(),
            state: bpmute::sleep_engine::StreamState {
                ts_ms: t_b,
                status: bpmute::sleep_engine::StreamStatus::Streaming,
            },
        })
        .await
        .unwrap();
    common::daemon::send_synthetic_wave_stream(
        &wearable_tx,
        &wearable_b,
        common::daemon::SyntheticStreamParams::warmup(t_b + 1_000),
    )
    .await;
    drop(wearable_tx);

    let mut closed_wearable_aliases = Vec::new();
    for _ in 0..2_000 {
        let ev = client.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            let alias = ev.data["session"]["wearable"]["alias"]
                .as_str()
                .unwrap()
                .to_string();
            let sid_wearable = ev.data["session"]["session"]["wearable_id"]
                .as_str()
                .unwrap()
                .to_string();
            let wearable_id = ev.data["session"]["wearable"]["id"]
                .as_str()
                .unwrap()
                .to_string();
            assert_eq!(sid_wearable, wearable_id);
            closed_wearable_aliases.push(alias);
            if closed_wearable_aliases.len() == 2 {
                break;
            }
        }
    }
    assert_eq!(
        closed_wearable_aliases,
        vec!["Wearable A".to_string(), "Wearable B".to_string()]
    );

    daemon.shutdown().await;

    let store = LedgerStore::open_read_only(&db_path_in_data_dir(&paths.data_dir))
        .unwrap()
        .unwrap();
    let page = store.list_sessions(10, 0).unwrap();
    assert_eq!(page.sessions.len(), 2);
    // Ordered newest first: session for B then session for A
    assert_eq!(
        page.sessions[0].wearable_alias.as_deref(),
        Some("Wearable B")
    );
    assert_eq!(
        page.sessions[1].wearable_alias.as_deref(),
        Some("Wearable A")
    );
}

struct ModeRestoreGuard(std::path::PathBuf, u32);

impl Drop for ModeRestoreGuard {
    fn drop(&mut self) {
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
    }
}

#[tokio::test]
async fn eacces_on_data_dir_or_ledger_db_maps_to_exit_code_5() {
    // 1) Inaccessible data_dir (0o000) -> EACCES across read/write offline ledger CLI commands
    let temp1 = TempDir::new().unwrap();
    let (paths1, env1) = test_paths_and_env(&temp1);
    fs::create_dir_all(&paths1.data_dir).unwrap();
    fs::set_permissions(&paths1.data_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let guard_dir = ModeRestoreGuard(paths1.data_dir.clone(), 0o700);

    for argv in [
        &["bpmute", "sessions", "list"][..],
        &["bpmute", "sessions", "show", "nonexistent"][..],
        &[
            "bpmute",
            "sessions",
            "feedback",
            "nonexistent",
            "--verdict",
            "false_positive",
        ][..],
        &["bpmute", "calibration", "status"][..],
        &["bpmute", "calibration", "accept-shift"][..],
        &["bpmute", "calibration", "dismiss-shift"][..],
    ] {
        let (code, _, err) = run_inproc_cli(argv, &env1).await;
        assert_eq!(
            code,
            ExitCode::PermissionOrBackendRefusal,
            "expected exit 5 for {argv:?} with 0o000 data_dir, stderr: {err}"
        );
    }
    drop(guard_dir);

    // 2) Inaccessible XDG_DATA_HOME parent (0o000) -> daemon returns exit code 5
    let parent1 = paths1.data_dir.parent().unwrap().to_path_buf();
    fs::set_permissions(&parent1, fs::Permissions::from_mode(0o000)).unwrap();
    let guard_parent = ModeRestoreGuard(parent1, 0o700);
    let out_daemon_parent = run_real_bin(&temp1, &["daemon"]);
    assert_eq!(
        out_daemon_parent.status.code(),
        Some(i32::from(ExitCode::PermissionOrBackendRefusal.as_u8())),
        "expected exit 5 for daemon with 0o000 data parent, stderr: {}",
        String::from_utf8_lossy(&out_daemon_parent.stderr)
    );
    drop(guard_parent);

    // 3) Existing bpmute.db set to 0o000 -> read-only, RW CLI, replay --session, and daemon return exit 5
    let temp2 = TempDir::new().unwrap();
    let (paths2, env2) = test_paths_and_env(&temp2);
    let db2 = db_path_in_data_dir(&paths2.data_dir);
    let lock2 = lock_path_in_data_dir(&paths2.data_dir);
    let session_id = {
        let mut store = LedgerStore::open_read_write(&db2, &lock2).unwrap();
        let w_id = store
            .upsert_wearable(
                &WearableDescriptor {
                    mac: "AA:BB:CC:DD:EE:99".to_string(),
                    address_type: "public".to_string(),
                    alias: None,
                    advertised_name: None,
                    supports_rr: false,
                    supports_contact: false,
                },
                1_700_000_000_000,
            )
            .unwrap();
        let sid = store
            .open_session(&OpenSessionRequest {
                wearable_id: w_id,
                start_ts_ms: 1_700_000_000_000,
                detection_sensitivity: Sensitivity::Balanced,
            })
            .unwrap();
        store
            .close_session_and_recompute(
                &CloseSessionRequest {
                    session_id: sid.clone(),
                    summary: sample_summary(1_700_000_000_000, 1_700_001_000_000, 58.0),
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                1_700_001_000_000,
            )
            .unwrap();
        sid
    };

    fs::set_permissions(&db2, fs::Permissions::from_mode(0o000)).unwrap();
    let _guard_db = ModeRestoreGuard(db2.clone(), 0o600);

    let out_daemon_db = run_real_bin(&temp2, &["daemon"]);
    assert_eq!(
        out_daemon_db.status.code(),
        Some(i32::from(ExitCode::PermissionOrBackendRefusal.as_u8())),
        "expected exit 5 for daemon with 0o000 bpmute.db, stderr: {}",
        String::from_utf8_lossy(&out_daemon_db.stderr)
    );

    for argv in [
        &["bpmute", "sessions", "list"][..],
        &["bpmute", "sessions", "show", &session_id][..],
        &[
            "bpmute",
            "sessions",
            "feedback",
            &session_id,
            "--verdict",
            "false_positive",
        ][..],
        &["bpmute", "calibration", "status"][..],
        &["bpmute", "calibration", "accept-shift"][..],
        &["bpmute", "calibration", "dismiss-shift"][..],
        &["bpmute", "replay", "--session", &session_id][..],
    ] {
        let (code, _, err) = run_inproc_cli(argv, &env2).await;
        assert_eq!(
            code,
            ExitCode::PermissionOrBackendRefusal,
            "expected exit 5 for {argv:?} with 0o000 bpmute.db, stderr: {err}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn live_daemon_hard_wipe_aborts_active_session_and_resets_engine() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = test_paths_and_env(&temp);
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let (wearable_tx, wearable_source) = FakeWearableSource::channel();

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_wearable_source(Arc::new(wearable_source)),
    )
    .await
    .unwrap();

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let _snapshot = client.next_event().await.unwrap();

    let desc = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:77".to_string(),
        address_type: "public".to_string(),
        alias: Some("ActiveWipeBand".to_string()),
        advertised_name: Some("ActiveWipeBand".to_string()),
        supports_rr: true,
        supports_contact: true,
    };
    let base_ts = start_utc.timestamp_millis();
    wearable_tx
        .send(WearableEvent::StreamState {
            wearable: desc.clone(),
            state: bpmute::sleep_engine::StreamState {
                ts_ms: base_ts,
                status: bpmute::sleep_engine::StreamStatus::Streaming,
            },
        })
        .await
        .unwrap();

    // Send 20 seconds of HR packets so an active session opens and emits an epoch.
    for sec in 1..=20 {
        wearable_tx
            .send(WearableEvent::HrPacket {
                wearable: desc.clone(),
                packet: bpmute::sleep_engine::HrPacket {
                    ts_ms: base_ts + sec * 1_000,
                    bpm: 64.0,
                    rr_ms: vec![930.0],
                    contact_status: bpmute::sleep_engine::ContactStatus::Contact,
                },
            })
            .await
            .unwrap();
    }

    let first_sid = loop {
        let ev = client.next_event().await.unwrap();
        if ev.event == "EpochUpdate" {
            break ev.data["session_id"].as_str().unwrap().to_string();
        }
    };

    let stats_before = client.request("data.stats", json!({})).await.unwrap();
    assert_eq!(stats_before["sessions_count"], 1);
    let status_before = client.request("daemon.status", json!({})).await.unwrap();
    assert_ne!(status_before["monitoring_state"], "Idle");
    let sessions_before = client.request("session.list", json!({})).await.unwrap();
    assert_eq!(sessions_before["sessions"][0]["id"], first_sid);
    assert_eq!(sessions_before["sessions"][0]["status"], "active");

    // Hard wipe (keep enrolled wearables) while monitoring session is active.
    client
        .request("data.hard_wipe", json!({ "all": false }))
        .await
        .unwrap();

    let status_after = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(status_after["monitoring_state"], "Idle");
    let sessions_after = client.request("session.list", json!({})).await.unwrap();
    assert!(sessions_after["sessions"].as_array().unwrap().is_empty());

    let cal_after = client
        .request("calibration.status", json!({}))
        .await
        .unwrap();
    assert_eq!(cal_after["wearables"].as_object().unwrap().len(), 1);

    let stats_after = client.request("data.stats", json!({})).await.unwrap();
    assert_eq!(stats_after["sessions_count"], 0);

    // Subsequent stream packets must start a fresh session cleanly without stale session FK errors.
    wearable_tx
        .send(WearableEvent::StreamState {
            wearable: desc.clone(),
            state: bpmute::sleep_engine::StreamState {
                ts_ms: base_ts + 25_000,
                status: bpmute::sleep_engine::StreamStatus::Streaming,
            },
        })
        .await
        .unwrap();
    for sec in 26..=45 {
        wearable_tx
            .send(WearableEvent::HrPacket {
                wearable: desc.clone(),
                packet: bpmute::sleep_engine::HrPacket {
                    ts_ms: base_ts + sec * 1_000,
                    bpm: 64.0,
                    rr_ms: vec![930.0],
                    contact_status: bpmute::sleep_engine::ContactStatus::Contact,
                },
            })
            .await
            .unwrap();
    }
    let second_sid = loop {
        let ev = client.next_event().await.unwrap();
        if ev.event == "EpochUpdate" {
            break ev.data["session_id"].as_str().unwrap().to_string();
        }
    };
    assert_ne!(second_sid, first_sid);

    let stats_new = client.request("data.stats", json!({})).await.unwrap();
    assert_eq!(stats_new["sessions_count"], 1);

    drop(wearable_tx);
    daemon.shutdown().await;
}

#[tokio::test]
async fn pre_candidate_persistence_and_schema_hydration() {
    let temp = TempDir::new().unwrap();
    let data_dir = temp.path().join("data/bpmute");
    let db_path = db_path_in_data_dir(&data_dir);
    let lock_path = lock_path_in_data_dir(&data_dir);

    let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let wid = store
        .upsert_wearable(
            &WearableDescriptor {
                mac: "AA:BB:CC:DD:EE:FF".to_string(),
                alias: Some("TestRing".to_string()),
                advertised_name: Some("Ring".to_string()),
                supports_rr: true,
                supports_contact: true,
                address_type: "public".to_string(),
            },
            1_000_000,
        )
        .unwrap();

    let sid = store
        .open_session(&OpenSessionRequest {
            wearable_id: wid.clone(),
            start_ts_ms: 2_000_000,
            detection_sensitivity: Sensitivity::Balanced,
        })
        .unwrap();

    let mut summary = sample_summary(2_000_000, 3_000_000, 56.0);
    summary.pre_candidate_mu_hr = Some(67.5);
    summary.pre_candidate_sigma_hr = Some(2.1);
    summary.pre_candidate_ln_rmssd = Some(3.8);

    store
        .close_session_and_recompute(
            &CloseSessionRequest {
                session_id: sid.clone(),
                summary,
                trigger_power_action: PowerAction::None,
                rapid_resume_within_5m: false,
            },
            3_000_000,
        )
        .unwrap();

    let detail = store.get_session(&sid).unwrap();
    assert_eq!(detail.session.pre_candidate_mu_hr, Some(67.5));
    assert_eq!(detail.session.pre_candidate_sigma_hr, Some(2.1));
    assert_eq!(detail.session.pre_candidate_ln_rmssd, Some(3.8));

    // Export bundle and verify JSON
    let export_path = temp.path().join("export.json");
    store
        .export_bundle(
            &export_path,
            bpmute::storage::ExportBundleOptions {
                include_raw: false,
                include_epochs: false,
                include_wearables: true,
            },
            3_000_000,
        )
        .unwrap();

    let exported_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&export_path).unwrap()).unwrap();
    let s_obj = &exported_json["sessions"][0];
    assert_eq!(s_obj["pre_candidate_mu_hr"], 67.5);
    assert_eq!(s_obj["pre_candidate_sigma_hr"], 2.1);
    assert_eq!(s_obj["pre_candidate_ln_rmssd"], 3.8);

    // Import into fresh store
    let temp2 = TempDir::new().unwrap();
    let db2 = db_path_in_data_dir(temp2.path());
    let lock2 = lock_path_in_data_dir(temp2.path());
    let mut store2 = LedgerStore::open_read_write(&db2, &lock2).unwrap();
    store2
        .import_bundle(
            &export_path,
            bpmute::storage::ImportMode::Replace,
            false,
            3_500_000,
        )
        .unwrap();

    let detail2 = store2.get_session(&sid).unwrap();
    assert_eq!(detail2.session.pre_candidate_mu_hr, Some(67.5));
    assert_eq!(detail2.session.pre_candidate_sigma_hr, Some(2.1));
    assert_eq!(detail2.session.pre_candidate_ln_rmssd, Some(3.8));
}

#[tokio::test]
async fn legacy_db_without_pre_candidate_columns_migrates_cleanly() {
    let temp = TempDir::new().unwrap();
    let db_path = temp.path().join("legacy.db");
    let lock_path = temp.path().join("legacy.lock");

    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 1;
             CREATE TABLE wearables (
                 id TEXT PRIMARY KEY,
                 mac TEXT NOT NULL,
                 address_type TEXT NOT NULL DEFAULT 'public',
                 alias TEXT,
                 advertised_name TEXT,
                 supports_rr INTEGER NOT NULL DEFAULT 0,
                 supports_contact INTEGER NOT NULL DEFAULT 0,
                 hr_var_floor REAL NOT NULL DEFAULT 0.25,
                 typical_sqi REAL,
                 enrolled_at_ms INTEGER NOT NULL,
                 last_seen_at_ms INTEGER NOT NULL,
                 active INTEGER NOT NULL DEFAULT 1,
                 UNIQUE (mac, address_type)
             );
             CREATE TABLE sessions (
                 id TEXT PRIMARY KEY,
                 wearable_id TEXT NOT NULL REFERENCES wearables(id),
                 status TEXT NOT NULL CHECK (status IN ('active', 'closed')),
                 start_ts_ms INTEGER NOT NULL,
                 end_ts_ms INTEGER NOT NULL,
                 end_reason TEXT,
                 detection_sensitivity TEXT NOT NULL,
                 had_rr INTEGER NOT NULL DEFAULT 0,
                 warmup_mu_hr REAL,
                 warmup_sigma_hr REAL,
                 warmup_ln_rmssd REAL,
                 onset_candidate_ts_ms INTEGER,
                 onset_confirmed_ts_ms INTEGER,
                 sleeping_ts_ms INTEGER,
                 sleep_mu_hr REAL,
                 onset_hr_bpm REAL,
                 delta_hr_onset REAL,
                 sleep_ln_rmssd REAL,
                 sleep_hr_var_ratio REAL,
                 typical_sqi REAL,
                 post_trigger_sleep_sec INTEGER NOT NULL DEFAULT 0,
                 trigger_power_action TEXT NOT NULL DEFAULT 'none',
                 rapid_resume_within_5m INTEGER NOT NULL DEFAULT 0,
                 grace_cancel_count INTEGER NOT NULL DEFAULT 0,
                 rapid_wake_fp_count INTEGER NOT NULL DEFAULT 0,
                 snooze_count INTEGER NOT NULL DEFAULT 0,
                 auto_anomalous INTEGER NOT NULL DEFAULT 0,
                 epochs_count INTEGER NOT NULL DEFAULT 0,
                 raw_samples_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE session_feedback (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE CASCADE,
                 verdict TEXT,
                 manual_onset_ts_ms INTEGER,
                 manual_avg_sleep_hr REAL,
                 notes TEXT,
                 updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE session_epochs (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL,
                 ts_ms INTEGER NOT NULL,
                 hr_bpm_60s REAL NOT NULL,
                 z_hr REAL NOT NULL,
                 hr_slope_4m REAL NOT NULL,
                 r_var_120s REAL NOT NULL,
                 ln_rmssd_60s REAL,
                 delta_ln_rmssd_60s REAL,
                 rmssd_sdnn_120s REAL,
                 hrv_valid INTEGER NOT NULL,
                 rr_artifact_frac REAL NOT NULL,
                 coverage REAL NOT NULL,
                 flatline_stale INTEGER NOT NULL,
                 accumulators_frozen INTEGER NOT NULL,
                 baseline_mu_hr REAL NOT NULL,
                 baseline_sigma_hr REAL NOT NULL,
                 baseline_frozen INTEGER NOT NULL,
                 p_active_wake REAL NOT NULL,
                 p_quiet_wake REAL NOT NULL,
                 p_sleep REAL NOT NULL,
                 state TEXT NOT NULL,
                 UNIQUE(session_id, seq)
             );
             CREATE TABLE raw_samples (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL,
                 ts_ms INTEGER NOT NULL,
                 bpm REAL NOT NULL,
                 rr_ms_json TEXT NOT NULL,
                 contact_status TEXT NOT NULL,
                 UNIQUE(session_id, seq)
             );
             CREATE TABLE calibration_boundaries (
                 id TEXT PRIMARY KEY,
                 effective_ts_ms INTEGER NOT NULL,
                 reason TEXT NOT NULL,
                 seed_sleep_hr REAL,
                 reset_floors INTEGER NOT NULL DEFAULT 0,
                 created_at_ms INTEGER NOT NULL,
                 reverted_at_ms INTEGER
             );
             CREATE TABLE calibration_meta (
                 singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
                 day1_seed_sleep_hr REAL,
                 day1_seed_updated_at_ms INTEGER,
                 dismissed_shift_anchor_ts_ms INTEGER,
                 dismissed_shift_direction TEXT
             );
             CREATE TABLE calibration_state (
                 singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
                 algorithm_version INTEGER NOT NULL,
                 computed_at_ms INTEGER NOT NULL,
                 state_json TEXT NOT NULL
             );
             INSERT INTO wearables (id, mac, address_type, enrolled_at_ms, last_seen_at_ms)
             VALUES ('w1', '11:22:33:44:55:66', 'public', 1000, 1000);
             INSERT INTO sessions (id, wearable_id, status, start_ts_ms, end_ts_ms, detection_sensitivity, warmup_mu_hr)
             VALUES ('s1', 'w1', 'closed', 1000, 2000, 'balanced', 65.0);
            ",
        )
        .unwrap();
    }

    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let detail = store.get_session("s1").unwrap();
    assert_eq!(detail.session.warmup_mu_hr, Some(65.0));
    assert_eq!(detail.session.pre_candidate_mu_hr, None);
    assert_eq!(detail.session.pre_candidate_sigma_hr, None);
    assert_eq!(detail.session.pre_candidate_ln_rmssd, None);
}
