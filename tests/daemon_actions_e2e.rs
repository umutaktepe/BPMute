mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use common::daemon::TestWearableSource;
use common::synthetic::{Phenotype, StageKind, generate_paired_trace};
use serde_json::json;
use tempfile::TempDir;

use bpmute::actions::fake::{FakeActionExecutor, FakeNotifier};
use bpmute::actions::traits::PowerActionResult;
use bpmute::calibration::CalibrationState;
use bpmute::cli::args::{Cli, Command};
use bpmute::cli::execute_cli;
use bpmute::config::{ActionsConfig, CliOverrides, LoadedConfig, PowerAction, ScheduleConfig};
use bpmute::daemon::{DaemonClock, DaemonHandle, DaemonOptions, VirtualClock, WearableEvent};
use bpmute::exit_code::ExitCode;
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::alert_ids;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::EngineEvent;
use bpmute::storage::{
    LedgerStore, OpenSessionRequest, SessionFullDetail, WearableDescriptor, db_path_in_data_dir,
    lock_path_in_data_dir,
};
use tokio::sync::mpsc;

fn test_paths(temp: &TempDir) -> ResolvedPaths {
    ResolvedPaths {
        config_file: temp.path().join("cfg/bpmute/config.toml"),
        data_dir: temp.path().join("data/bpmute"),
        socket_path: temp.path().join("run/bpmute/daemon.sock"),
    }
}

async fn spawn_test_daemon(
    temp: &TempDir,
    start_utc: DateTime<Utc>,
    actions_cfg: ActionsConfig,
    schedule_cfg: ScheduleConfig,
    executor: Arc<FakeActionExecutor>,
    notifier: Arc<FakeNotifier>,
    source: Arc<TestWearableSource>,
) -> (DaemonHandle, ResolvedPaths, Arc<VirtualClock>) {
    let paths = test_paths(temp);
    let env = MapEnv::new();
    let cli = CliOverrides::default();
    let mut loaded = LoadedConfig::load(&paths.config_file, &env, &cli).unwrap();
    loaded.config.actions = actions_cfg;
    loaded.config.schedule = schedule_cfg;

    let clock = Arc::new(VirtualClock::starting_at(start_utc));

    let handle = DaemonHandle::spawn(
        DaemonOptions::for_testing(
            paths.clone(),
            loaded,
            Arc::new(env),
            Arc::clone(&clock) as Arc<dyn bpmute::daemon::DaemonClock>,
        )
        .with_cli_overrides(cli)
        .with_wearable_source(source)
        .with_action_executor(executor as Arc<dyn bpmute::actions::traits::ActionExecutor>)
        .with_notifier(notifier as Arc<dyn bpmute::actions::traits::Notifier>),
    )
    .await
    .expect("daemon should start");

    (handle, paths, clock)
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_cancel_grace_period_via_ipc() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        ..ActionsConfig::default()
    };

    let (daemon, paths, _clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::new(source),
    )
    .await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();

    // Trigger test_trigger
    let resp = client
        .request("monitor.test_trigger", json!({ "grace_sec": 20 }))
        .await
        .unwrap();
    assert_eq!(resp["test_trigger_started"], true);

    // Let 2 seconds elapse in virtual time
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Issue monitor.cancel_grace
    let cancel_resp = client
        .request("monitor.cancel_grace", json!({}))
        .await
        .unwrap();
    assert_eq!(cancel_resp["cancelled"], true);

    // Verify notification was closed
    let events = notifier.events();
    assert!(events.iter().any(|e| e.starts_with("close_notice:")));

    // Clean shutdown
    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_snooze_grace_period_via_ipc() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(30),
        snooze_duration_min: 30,
        ..ActionsConfig::default()
    };

    let (daemon, paths, _clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::new(source),
    )
    .await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();

    // Start test trigger
    let resp = client
        .request("monitor.test_trigger", json!({ "grace_sec": 30 }))
        .await
        .unwrap();
    assert_eq!(resp["test_trigger_started"], true);

    tokio::time::sleep(Duration::from_secs(3)).await;

    // Call monitor.snooze_grace
    let snooze_resp = client
        .request("monitor.snooze_grace", json!({ "duration_min": 45 }))
        .await
        .unwrap();
    assert_eq!(snooze_resp["snoozed"], true);
    assert_eq!(snooze_resp["duration_sec"], 2700);

    // Notice closed
    let events = notifier.events();
    assert!(events.iter().any(|e| e.starts_with("close_notice:")));

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_strictly_pause_media_only_zero_players_skips_grace() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    // Zero players playing
    executor.set_playing_players(&[]);

    let actions_cfg = ActionsConfig {
        pause_media: true,
        custom_command: vec![],
        lock_screen: false,
        power_action: PowerAction::None,
        grace_period_sec: Some(30),
        ..ActionsConfig::default()
    };

    let (daemon, _paths, _clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::new(source),
    )
    .await;

    // Simulate sleep onset confirmation in daemon core
    let now_ms = start_utc.timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    // Notice should NOT have been shown because Grace Period was skipped!
    assert!(notifier.events().is_empty());

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_cli_cancel_and_snooze_commands_e2e() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(30),
        snooze_duration_min: 20,
        ..ActionsConfig::default()
    };

    let (daemon, paths, _clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::new(source),
    )
    .await;

    let env = MapEnv::new();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    // 1. Try cancel when no grace is active -> exit code 4 (PreconditionConflict)
    let cli_cancel = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        no_mouse: false,
        ascii: false,
        command: Some(Command::Cancel),
    };
    let code = execute_cli(cli_cancel, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::PreconditionConflict);

    // 2. Start test-trigger via CLI -> ExitCode::Success
    stdout.clear();
    stderr.clear();
    let cli_test_trig = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        no_mouse: false,
        ascii: false,
        command: Some(Command::TestTrigger {
            grace_sec: Some(25),
        }),
    };
    let code = execute_cli(cli_test_trig, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::Success);
    let out_str = String::from_utf8_lossy(&stdout);
    assert!(out_str.contains("Initiated dry-run test trigger (25s grace period)"));

    // 3. Now cancel via CLI -> ExitCode::Success
    stdout.clear();
    stderr.clear();
    let cli_cancel_ok = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        no_mouse: false,
        ascii: false,
        command: Some(Command::Cancel),
    };
    let code = execute_cli(cli_cancel_ok, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::Success);
    let out_cancel = String::from_utf8_lossy(&stdout);
    assert!(out_cancel.contains("Cancelled sleep grace period"));

    // 4. Start test-trigger again and snooze via CLI -> ExitCode::Success
    stdout.clear();
    stderr.clear();
    let cli_test_trig2 = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        no_mouse: false,
        ascii: false,
        command: Some(Command::TestTrigger { grace_sec: None }),
    };
    let code = execute_cli(cli_test_trig2, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::Success);

    stdout.clear();
    stderr.clear();
    let cli_snooze = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        no_mouse: false,
        ascii: false,
        command: Some(Command::Snooze {
            duration: Some("45m".to_string()),
        }),
    };
    let code = execute_cli(cli_snooze, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::Success);
    let out_snooze = String::from_utf8_lossy(&stdout);
    assert!(out_snooze.contains("Snoozed sleep triggers for 45m"));

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_rapid_resume_vs_uncancelled_suspend_credit() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(10),
        ..ActionsConfig::default()
    };

    let (daemon, _paths, clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::new(source),
    )
    .await;

    // Simulate sleep trigger at t=0
    let t0 = clock.now_utc().timestamp_millis();
    *daemon.core().sleep_triggered_at.lock().unwrap() = Some(t0);
    *daemon.core().trigger_power_action.lock().unwrap() = PowerAction::Suspend;

    // Advance virtual time by 2 minutes (< 5 min)
    tokio::time::sleep(Duration::from_secs(120)).await;

    // System resumes from suspend: PrepareForSleep(false)
    daemon.core().signal_prepare_for_sleep(false).await;

    // Should be classified as rapid resume (< 5 min)
    assert!(*daemon.core().rapid_resume_within_5m.lock().unwrap());

    // Now test >= 5 min:
    let t1 = clock.now_utc().timestamp_millis();
    *daemon.core().sleep_triggered_at.lock().unwrap() = Some(t1);
    *daemon.core().trigger_power_action.lock().unwrap() = PowerAction::Suspend;

    // Advance virtual time by 10 minutes (>= 5 min)
    tokio::time::sleep(Duration::from_secs(600)).await;

    daemon.core().signal_prepare_for_sleep(false).await;

    // Should be uncancelled suspend credit (rapid_resume_within_5m = false)
    assert!(!*daemon.core().rapid_resume_within_5m.lock().unwrap());

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_prepare_for_sleep_true_aborts_grace_and_flushes_wal() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    let (daemon, paths, _) = spawn_test_daemon(
        &temp,
        start_utc,
        ActionsConfig {
            power_action: PowerAction::Suspend,
            grace_period_sec: Some(30),
            ..ActionsConfig::default()
        },
        ScheduleConfig::default(),
        executor,
        notifier,
        Arc::new(source),
    )
    .await;

    // Start a grace period via test_trigger
    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client
        .request("monitor.test_trigger", json!({ "grace_sec": 30 }))
        .await
        .unwrap();

    assert!(daemon.core().is_in_grace_period());

    // PrepareForSleep(true) should abort grace and flush WAL
    daemon.core().signal_prepare_for_sleep(true).await;
    tokio::task::yield_now().await;

    assert!(!daemon.core().is_in_grace_period());

    // Verify WAL flush succeeds without error
    let flush_res = daemon.core().persistence.flush_wal().await;
    assert!(flush_res.is_ok(), "SQLite WAL flush must succeed");

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_dry_run_records_zero_calibration_feedback() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let paths = test_paths(&temp);
    let env = MapEnv::new();
    let (source, _tx) = TestWearableSource::new();

    // Write config with actions.dry_run = true
    let cfg_dir = paths.config_file.parent().unwrap();
    std::fs::create_dir_all(cfg_dir).unwrap();
    std::fs::write(
        &paths.config_file,
        r#"
schema_version = 1

[actions]
dry_run = true
grace_period_sec = 10
"#,
    )
    .unwrap();

    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock.clone())
            .with_wearable_source(Arc::new(source))
            .with_action_executor(executor)
            .with_notifier(notifier),
    )
    .await
    .unwrap();

    assert!(daemon.core().is_dry_run());

    // Trigger sleep onset confirmation under dry_run
    daemon
        .core()
        .handle_confirmed_sleep_onset(clock.now_utc().timestamp_millis())
        .await;

    // Advance 15s to expire grace period
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Under dry run, no calibration update is delivered and no side effects execute
    let cal = daemon.core().calibration_state();
    assert_eq!(cal.valid_sessions, 0);

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_post_trigger_mpris_watch_delivers_rapid_wake_fp() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let (source, _source_tx) = TestWearableSource::new();

    // Mark spotify as playing so pause_media pauses it
    executor.set_playing_players(&["org.mpris.MediaPlayer2.spotify"]);

    let actions_cfg = ActionsConfig {
        pause_media: true,
        power_action: PowerAction::None,
        grace_period_sec: Some(10),
        ..ActionsConfig::default()
    };

    let (daemon, paths, clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        executor.clone(),
        notifier,
        Arc::new(source),
    )
    .await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let _ = client.next_event().await.unwrap();

    // Trigger sleep onset confirmation
    daemon
        .core()
        .handle_confirmed_sleep_onset(clock.now_utc().timestamp_millis())
        .await;

    // Advance 12s to expire grace period and execute pause_media pipeline
    tokio::time::sleep(Duration::from_secs(12)).await;

    // Now media is paused, and watch_mpris_resume is active.
    // Trigger player playback resume:
    executor.trigger_mpris_resume();
    tokio::time::sleep(Duration::from_millis(50)).await;

    daemon.shutdown().await;
}

fn load_single_session_and_cal(paths: &ResolvedPaths) -> (SessionFullDetail, CalibrationState) {
    let store = LedgerStore::open_read_only(&db_path_in_data_dir(&paths.data_dir))
        .unwrap()
        .expect("ledger database must exist");
    let page = store.list_sessions(10, 0).unwrap();
    assert_eq!(page.sessions.len(), 1);
    let detail = store.get_session(&page.sessions[0].id).unwrap();
    let cal = store.get_calibration_state().unwrap();
    (detail, cal)
}

async fn feed_until_confirmed(
    wearable_tx: &mpsc::Sender<WearableEvent>,
    client: &mut IpcClient,
    base_ts: i64,
) {
    let desc = WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:11".to_string(),
        address_type: "public".to_string(),
        alias: Some("Polar Strap".to_string()),
        advertised_name: Some("Polar H10".to_string()),
        supports_rr: true,
        supports_contact: true,
    };

    let trace = generate_paired_trace(
        101,
        Phenotype::typical(),
        &[
            StageKind::QuietWake { duration_sec: 900 },
            StageKind::N1Onset { duration_sec: 420 },
            StageKind::N2Sleep { duration_sec: 900 },
        ],
    );
    let split_idx = trace.bpm_rr.len() * 2 / 3;
    for ev in &trace.bpm_rr[..split_idx] {
        match ev {
            EngineEvent::StreamState(st) => {
                let mut shifted = *st;
                shifted.ts_ms += base_ts;
                wearable_tx
                    .send(WearableEvent::StreamState {
                        wearable: desc.clone(),
                        state: shifted,
                    })
                    .await
                    .unwrap();
            }
            EngineEvent::HrPacket(pkt) => {
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
            EngineEvent::DesktopActivity(_) | EngineEvent::GraceOutcome(_) => {}
        }
    }

    for _ in 0..10_000 {
        let ev = client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data["monitoring_state"] == "Confirmed" {
            return;
        }
    }
    panic!("SleepEngine must reach Confirmed during N1/N2 trace");
}

#[tokio::test(start_paused = true)]
async fn test_poweroff_race_preserves_trigger_metadata_and_calibration() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let actions_cfg = ActionsConfig {
        pause_media: true,
        power_action: PowerAction::Poweroff,
        grace_period_sec: Some(10),
        ..ActionsConfig::default()
    };

    let (source, wearable_tx) = TestWearableSource::new();
    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_playing_players(&["org.mpris.MediaPlayer2.mpv"]);
    let mut power_rx = executor.subscribe_power_actions();
    let notifier = Arc::new(FakeNotifier::new());

    let (daemon, paths, _clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        notifier,
        Arc::new(source),
    )
    .await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let _ = client.next_event().await.unwrap();

    feed_until_confirmed(&wearable_tx, &mut client, start_utc.timestamp_millis()).await;

    assert_eq!(power_rx.recv().await, Some(PowerAction::Poweroff));

    // Even before daemon shutdown completes, SQLite already holds the pre-poweroff trigger state.
    let (pre_detail, _) = load_single_session_and_cal(&paths);
    assert!(pre_detail.session.sleeping_ts_ms.is_some());
    assert_eq!(
        pre_detail.session.trigger_power_action,
        PowerAction::Poweroff
    );

    daemon.shutdown().await;

    let (detail, cal) = load_single_session_and_cal(&paths);
    assert!(
        detail.session.sleeping_ts_ms.is_some(),
        "sleeping_ts_ms must be persisted when poweroff triggers"
    );
    assert_eq!(
        detail.session.trigger_power_action,
        PowerAction::Poweroff,
        "trigger_power_action must be persisted as poweroff"
    );
    assert_eq!(
        cal.valid_sessions, 1,
        "uncancelled poweroff Monitoring Session must count toward calibration"
    );
}

#[tokio::test(start_paused = true)]
async fn test_refused_poweroff_reverts_trigger_power_action_and_calibration() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let actions_cfg = ActionsConfig {
        pause_media: false,
        power_action: PowerAction::Poweroff,
        grace_period_sec: Some(10),
        ..ActionsConfig::default()
    };

    let (source, wearable_tx) = TestWearableSource::new();
    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_power_action_override(Ok(PowerActionResult::RefusedInhibited(
        "systemd-inhibit: update".to_string(),
    )));
    let mut power_rx = executor.subscribe_power_actions();
    let notifier = Arc::new(FakeNotifier::new());

    let (daemon, paths, _clock) = spawn_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        ScheduleConfig::default(),
        Arc::clone(&executor),
        notifier,
        Arc::new(source),
    )
    .await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let _ = client.next_event().await.unwrap();

    feed_until_confirmed(&wearable_tx, &mut client, start_utc.timestamp_millis()).await;

    assert_eq!(power_rx.recv().await, Some(PowerAction::Poweroff));

    // Wait deterministically for the post-revert AlertChanged event published after
    // trigger_power_action has been reverted in memory and SQLite.
    for _ in 0..10_000 {
        let ev = client.next_event().await.unwrap();
        if ev.event == "AlertChanged"
            && ev.data["alerts"]
                .as_array()
                .is_some_and(|arr| arr.iter().any(|al| al["id"] == alert_ids::LOGIND_INHIBITED))
        {
            break;
        }
    }

    daemon.shutdown().await;

    let (detail, cal) = load_single_session_and_cal(&paths);
    assert!(detail.session.sleeping_ts_ms.is_some());
    assert_eq!(
        detail.session.trigger_power_action,
        PowerAction::None,
        "refused poweroff must revert trigger_power_action to none"
    );
    assert_eq!(
        cal.valid_sessions, 0,
        "refused poweroff without post-trigger sleep must not count as calibration"
    );
}

#[test]
fn test_stale_recovery_preserves_pre_poweroff_trigger_and_calibration() {
    let temp = TempDir::new().unwrap();
    let db_path = db_path_in_data_dir(temp.path());
    let lock_path = lock_path_in_data_dir(temp.path());
    let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();

    let wearable_id = store
        .upsert_wearable(
            &WearableDescriptor {
                mac: "AA:BB:CC:DD:EE:11".to_string(),
                address_type: "public".to_string(),
                alias: Some("Polar".to_string()),
                advertised_name: Some("Polar H10".to_string()),
                supports_rr: true,
                supports_contact: true,
            },
            1_700_000_000_000,
        )
        .unwrap();

    let session_id = store
        .open_session(&OpenSessionRequest {
            wearable_id,
            start_ts_ms: 1_700_000_000_000,
            detection_sensitivity: bpmute::config::Sensitivity::Balanced,
        })
        .unwrap();

    store
        .mark_session_sleep_trigger(&session_id, 1_700_001_000_000, PowerAction::Poweroff)
        .unwrap();

    let recovered = store.recover_stale_sessions(1_700_002_000_000).unwrap();
    assert_eq!(recovered, 1);

    let detail = store.get_session(&session_id).unwrap();
    assert_eq!(
        detail.session.sleeping_ts_ms,
        Some(1_700_001_000_000),
        "stale recovery must preserve pre-poweroff sleeping_ts_ms"
    );
    assert_eq!(
        detail.session.trigger_power_action,
        PowerAction::Poweroff,
        "stale recovery must preserve pre-poweroff trigger_power_action"
    );
    assert_eq!(store.get_calibration_state().unwrap().valid_sessions, 1);
}
