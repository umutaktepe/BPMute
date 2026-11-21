mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use common::synthetic::{Phenotype, StageKind, generate_paired_trace};
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use bpmute::actions::fake::{FakeActionExecutor, FakeNotifier};
use bpmute::config::{ActionsConfig, CliOverrides, LoadedConfig, PowerAction, ScheduleConfig};
use bpmute::daemon::{DaemonClock, DaemonHandle, DaemonOptions, VirtualClock, WearableEvent};
use bpmute::idle::fake::FakeIdleProbe;
use bpmute::idle::traits::{IdleBackendKind, IdleProbe};
use bpmute::ipc::IpcClient;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::EngineEvent;
use bpmute::storage::WearableDescriptor;
use serde_json::json;

fn test_paths(temp: &TempDir) -> ResolvedPaths {
    ResolvedPaths {
        config_file: temp.path().join("cfg/bpmute/config.toml"),
        data_dir: temp.path().join("data/bpmute"),
        socket_path: temp.path().join("run/bpmute/daemon.sock"),
    }
}

struct TestWearableSource {
    rx: tokio::sync::Mutex<Option<mpsc::Receiver<WearableEvent>>>,
}

impl TestWearableSource {
    fn new() -> (Self, mpsc::Sender<WearableEvent>) {
        let (tx, rx) = mpsc::channel(4096);
        (
            Self {
                rx: tokio::sync::Mutex::new(Some(rx)),
            },
            tx,
        )
    }
}

impl bpmute::daemon::WearableSource for TestWearableSource {
    fn subscribe(&self, _shutdown_rx: watch::Receiver<bool>) -> mpsc::Receiver<WearableEvent> {
        self.rx
            .try_lock()
            .expect("single subscriber")
            .take()
            .expect("only subscribed once")
    }
}

async fn spawn_idle_test_daemon(
    temp: &TempDir,
    start_utc: DateTime<Utc>,
    actions_cfg: ActionsConfig,
    executor: Arc<FakeActionExecutor>,
    notifier: Arc<FakeNotifier>,
    idle_probe: Arc<FakeIdleProbe>,
    source: Arc<TestWearableSource>,
) -> (DaemonHandle, ResolvedPaths, Arc<VirtualClock>) {
    let paths = test_paths(temp);
    let env = MapEnv::new();
    let cli = CliOverrides::default();
    let mut loaded = LoadedConfig::load(&paths.config_file, &env, &cli).unwrap();
    loaded.config.actions = actions_cfg;
    loaded.config.schedule = ScheduleConfig::default();

    let clock = Arc::new(VirtualClock::starting_at(start_utc));

    let opts = DaemonOptions::for_testing(
        paths.clone(),
        loaded,
        Arc::new(env),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
    )
    .with_wearable_source(source)
    .with_action_executor(executor as Arc<dyn bpmute::actions::traits::ActionExecutor>)
    .with_notifier(notifier as Arc<dyn bpmute::actions::traits::Notifier>)
    .with_idle_probe(Arc::clone(&idle_probe) as Arc<dyn IdleProbe>);

    let handle = DaemonHandle::spawn(opts)
        .await
        .expect("daemon should start");

    (handle, paths, clock)
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_pre_trigger_gate_suppresses_grace_period() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_playing_players(&["org.mpris.MediaPlayer2.vlc"]);
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::new(IdleBackendKind::WaylandExtIdleV2));
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        ..ActionsConfig::default()
    };

    // User was active within 2-minute trailing pre-gate window
    idle_probe.set_user_active_trailing(true);

    let (daemon, _paths, clock) = spawn_idle_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::clone(&idle_probe),
        Arc::new(source),
    )
    .await;

    // Simulate confirmed sleep onset
    let now_ms = clock.now_utc().timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pre-gate should have suppressed entering Grace Period!
    // Active grace period is false, and zero notifications shown.
    assert!(!daemon.core().is_in_grace_period());
    assert_eq!(notifier.active_notice_id(), None);

    // Now set user_active_trailing to false: pre-gate passes, and Grace Period is entered!
    idle_probe.set_user_active_trailing(false);
    let now_ms = clock.now_utc().timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(daemon.core().is_in_grace_period());
    assert!(notifier.active_notice_id().is_some());

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_settled_grace_cancellation_and_in_flight_race_suppression() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_playing_players(&["org.mpris.MediaPlayer2.vlc"]);
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::new(IdleBackendKind::WaylandExtIdleV2));
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        ..ActionsConfig::default()
    };

    let (daemon, _paths, clock) = spawn_idle_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::clone(&idle_probe),
        Arc::new(source),
    )
    .await;

    // Trigger confirmed sleep onset (pre-gate passes)
    idle_probe.set_user_active_trailing(false);
    let now_ms = clock.now_utc().timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    // Wait 200ms to enter Grace Period
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(daemon.core().is_in_grace_period());

    // 1. In-flight event at t0 + 400ms (within 1s settle delay):
    // Must be suppressed to avoid in-flight races!
    idle_probe.trigger_activity();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        daemon.core().is_in_grace_period(),
        "activity during 1s settle delay must not cancel grace"
    );

    // 2. Advance time past the 1s settle delay (e.g. at t0 + 1500ms)
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(daemon.core().is_in_grace_period());

    // Activity after settle delay enters fixed 10s Activity Hold (does NOT cancel immediately)
    idle_probe.trigger_activity();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        daemon.core().is_in_grace_period(),
        "activity after 1s settle delay must hold Grace Period open for 10s Activity Hold"
    );
    assert!(
        notifier.active_notice_id().is_some(),
        "grace notification must remain open during Activity Hold"
    );
    assert!(
        notifier
            .events()
            .iter()
            .any(|e| e.contains("rem=10/30") && e.contains("phase=activity_hold")),
        "Activity Hold must update notification immediately to 10s; got: {:?}",
        notifier.events()
    );

    // Still active at +9s into Activity Hold
    tokio::time::sleep(Duration::from_secs(9)).await;
    assert!(
        daemon.core().is_in_grace_period(),
        "Activity Hold must remain open for the full 10s"
    );

    // Expires at +10s with GraceResult::Cancelled
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !daemon.core().is_in_grace_period(),
        "Activity Hold must auto-cancel after 10s"
    );
    assert_eq!(notifier.active_notice_id(), None);

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_post_trigger_desktop_activity_watch_fp() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_playing_players(&["org.mpris.MediaPlayer2.vlc"]);
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::new(IdleBackendKind::WaylandExtIdleV2));
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(10),
        ..ActionsConfig::default()
    };

    let (daemon, _paths, clock) = spawn_idle_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::clone(&idle_probe),
        Arc::new(source),
    )
    .await;

    // Trigger confirmed sleep onset
    idle_probe.set_user_active_trailing(false);
    let now_ms = clock.now_utc().timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    // Advance 11s: Grace Period expires, actions execute, transitions to Sleeping
    tokio::time::sleep(Duration::from_secs(11)).await;
    assert_eq!(
        daemon.core().status().monitoring_state.to_string(),
        "Sleeping"
    );

    // 1 minute into the 5-minute post-trigger watch, user moves mouse or types
    tokio::time::sleep(Duration::from_secs(60)).await;
    idle_probe.trigger_activity();

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Post-trigger watch must have caught desktop activity, aborted Sleeping, and returned to Monitoring!
    assert_eq!(
        daemon.core().status().monitoring_state.to_string(),
        "Monitoring",
        "desktop activity during 5-min post-trigger watch must abort Sleeping and return to Monitoring"
    );

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_degraded_fallback_grace_floor_alert_and_strict_refusal() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();

    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::new(IdleBackendKind::Degraded));
    let (source, _source_tx) = TestWearableSource::new();

    let actions_cfg = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30), // Configured for 30s, but degraded floor is 90s!
        strict_idle_backend: false,
        ..ActionsConfig::default()
    };

    let (daemon, _paths, clock) = spawn_idle_test_daemon(
        &temp,
        start_utc,
        actions_cfg,
        Arc::clone(&executor),
        Arc::clone(&notifier),
        Arc::clone(&idle_probe),
        Arc::new(source),
    )
    .await;

    // 1. Status alerts must contain degraded idle backend warning for destructive power actions
    let status = daemon.core().status();
    assert_eq!(
        status.idle_backend.as_deref(),
        Some("Degraded (no idle backend available)")
    );
    assert!(
        status
            .alerts
            .iter()
            .any(|a| a.id == "idle_backend_degraded"),
        "status alerts must include idle_backend_degraded alert"
    );

    // 2. Trigger sleep onset confirmation:
    // Degraded floor enforces >= 90s grace period instead of configured 30s!
    let now_ms = clock.now_utc().timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(daemon.core().is_in_grace_period());

    // Advance 35s: configured 30s would have expired, but 90s floor means grace is still active!
    tokio::time::sleep(Duration::from_secs(35)).await;
    assert!(
        daemon.core().is_in_grace_period(),
        "grace period must still be active after 35s due to >= 90s degraded safety floor"
    );

    // Advance remaining 56s (total > 90s) -> grace expires and power action executes
    tokio::time::sleep(Duration::from_secs(56)).await;
    assert!(!daemon.core().is_in_grace_period());
    assert!(
        executor
            .events()
            .contains(&"power_action:suspend".to_string())
    );

    // 3. Test strict_idle_backend = true refusal
    executor.clear_events();
    daemon
        .core()
        .handle_op(
            "config.update",
            serde_json::json!({
                "actions": {
                    "strict_idle_backend": true
                }
            }),
            1,
        )
        .await
        .unwrap();

    let now_ms = clock.now_utc().timestamp_millis();
    daemon.core().handle_confirmed_sleep_onset(now_ms).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    tokio::time::sleep(Duration::from_secs(92)).await;

    // Destructive power action must be refused!
    assert!(
        !executor
            .events()
            .contains(&"power_action:suspend".to_string()),
        "destructive power action must be refused when strict_idle_backend is true on degraded backend"
    );

    // 4. Test that start_test_trigger honors explicit testing grace (e.g. 10s) without forcing 90s floor
    let test_grace = daemon.core().start_test_trigger(Some(10)).await.unwrap();
    assert_eq!(
        test_grace, 10,
        "start_test_trigger must honor explicit test grace"
    );
    daemon.core().cancel_grace().await.unwrap();

    // 5. Test inhibitor-sensitive alert when switching to WaylandExtIdleV1
    idle_probe.set_backend_kind(IdleBackendKind::WaylandExtIdleV1);
    daemon
        .core()
        .handle_op("config.reload", serde_json::Value::Null, 2)
        .await
        .unwrap();
    let status_v1 = daemon.core().status();
    assert_eq!(
        status_v1.idle_backend.as_deref(),
        Some("Wayland (ext-idle-notify-v1 v1, inhibitor-sensitive)")
    );
    assert!(
        status_v1
            .alerts
            .iter()
            .any(|a| a.id == "idle_backend_inhibitor_sensitive"),
        "status alerts must include idle_backend_inhibitor_sensitive alert for v1"
    );

    daemon.shutdown().await;
}

fn default_test_wearable() -> WearableDescriptor {
    WearableDescriptor {
        mac: "AA:BB:CC:DD:EE:11".to_string(),
        address_type: "public".to_string(),
        alias: Some("Polar Strap".to_string()),
        advertised_name: Some("Polar H10".to_string()),
        supports_rr: true,
        supports_contact: true,
    }
}

async fn send_trace_slice(
    wearable_tx: &mpsc::Sender<WearableEvent>,
    events: &[EngineEvent],
    base_ts: i64,
) {
    let desc = default_test_wearable();
    for ev in events {
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
}

async fn feed_trace_until_confirmed(
    wearable_tx: &mpsc::Sender<WearableEvent>,
    client: &mut IpcClient,
    base_ts: i64,
) -> (Vec<EngineEvent>, usize) {
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
    send_trace_slice(wearable_tx, &trace.bpm_rr[..split_idx], base_ts).await;

    for _ in 0..10_000 {
        let ev = client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data["monitoring_state"] == "Confirmed" {
            return (trace.bpm_rr, split_idx);
        }
    }
    panic!("SleepEngine must reach Confirmed during N1/N2 trace");
}

async fn await_session_closed_and_calibration(sub: &mut IpcClient) -> String {
    let mut closed_session_id = String::new();
    let mut saw_cal_updated = false;
    for _ in 0..10_000 {
        let ev = sub.next_event().await.unwrap();
        match ev.event.as_str() {
            "SessionClosed" => {
                closed_session_id = ev.data["session"]["session"]["id"]
                    .as_str()
                    .unwrap()
                    .to_string();
            }
            "CalibrationUpdated" => {
                saw_cal_updated = true;
                if !closed_session_id.is_empty() {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(!closed_session_id.is_empty());
    assert!(saw_cal_updated);
    closed_session_id
}

async fn enter_activity_hold_in_daemon(
    temp: &TempDir,
    start_utc: DateTime<Utc>,
    trace_base_ts: i64,
    actions_cfg: ActionsConfig,
) -> (
    DaemonHandle,
    ResolvedPaths,
    mpsc::Sender<WearableEvent>,
    IpcClient,
    Vec<EngineEvent>,
    usize,
) {
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::new(IdleBackendKind::WaylandExtIdleV2));
    let (source, wearable_tx) = TestWearableSource::new();

    let (daemon, paths, _clock) = spawn_idle_test_daemon(
        temp,
        start_utc,
        actions_cfg,
        executor,
        notifier,
        Arc::clone(&idle_probe),
        Arc::new(source),
    )
    .await;

    let mut sub = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub.request("daemon.subscribe", json!({})).await.unwrap();
    let _snap = sub.next_event().await.unwrap();

    let (trace_events, split_idx) =
        feed_trace_until_confirmed(&wearable_tx, &mut sub, trace_base_ts).await;

    // Wait past 1s settle delay, then trigger desktop activity -> enters 10s Activity Hold
    tokio::time::sleep(Duration::from_millis(1500)).await;
    idle_probe.trigger_activity();

    (daemon, paths, wearable_tx, sub, trace_events, split_idx)
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_activity_hold_allows_snooze_with_zero_fp_penalty() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let (daemon, paths, wearable_tx, mut sub, _, _) = enter_activity_hold_in_daemon(
        &temp,
        start_utc,
        start_utc.timestamp_millis(),
        ActionsConfig {
            power_action: PowerAction::Suspend,
            grace_period_sec: Some(300),
            snooze_duration_min: 20,
            ..ActionsConfig::default()
        },
    )
    .await;

    let mut saw_hold_tick = false;
    for _ in 0..10_000 {
        let ev = sub.next_event().await.unwrap();
        if ev.event == "GraceTick"
            && ev.data["phase"] == "activity_hold"
            && ev.data["remaining_sec"] == 10
        {
            saw_hold_tick = true;
            break;
        }
    }
    assert!(
        saw_hold_tick,
        "must publish GraceTick(10, phase=activity_hold)"
    );
    assert!(daemon.core().is_in_grace_period());

    // Advance 2s into Activity Hold and issue monitor.snooze_grace via IPC
    tokio::time::sleep(Duration::from_secs(2)).await;
    let mut rpc = IpcClient::connect(&paths.socket_path).await.unwrap();
    let snooze_resp = rpc
        .request("monitor.snooze_grace", json!({ "duration_min": 20 }))
        .await
        .unwrap();
    assert_eq!(snooze_resp["snoozed"], true);
    assert_eq!(snooze_resp["duration_sec"], 1200);

    // Wait for Monitoring state and close session
    for _ in 0..10_000 {
        let ev = sub.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data["monitoring_state"] == "Monitoring" {
            break;
        }
    }
    drop(wearable_tx);
    let closed_session_id = await_session_closed_and_calibration(&mut sub).await;

    let detail = rpc
        .request("session.get", json!({ "id": closed_session_id }))
        .await
        .unwrap();
    assert_eq!(detail["session"]["snooze_count"], 1);
    assert_eq!(detail["session"]["grace_cancel_count"], 0);
    assert_eq!(detail["session"]["rapid_wake_fp_count"], 0);

    let list = rpc
        .request("session.list", json!({ "limit": 10 }))
        .await
        .unwrap();
    assert_eq!(list["sessions"][0]["fp_count"], 0);

    let cal = rpc.request("calibration.status", json!({})).await.unwrap();
    assert_eq!(cal["tau_p_sleep_offset"], 0.0);
    assert_eq!(cal["dwell_adjust_sec"], 0);

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_virtual_time_activity_hold_expiry_applies_cancelled_fp_penalty_and_cooldown() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let (daemon, paths, wearable_tx, mut sub, trace_events, split_idx) =
        enter_activity_hold_in_daemon(
            &temp,
            start_utc,
            start_utc.timestamp_millis() - 1_500_000,
            ActionsConfig {
                power_action: PowerAction::Suspend,
                grace_period_sec: Some(300),
                ..ActionsConfig::default()
            },
        )
        .await;

    // Let Activity Hold expire (10s) without issuing Snooze or Cancel
    let mut saw_monitoring = false;
    for _ in 0..10_000 {
        let ev = sub.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data["monitoring_state"] == "Monitoring" {
            saw_monitoring = true;
            break;
        }
    }
    assert!(
        saw_monitoring,
        "expired Activity Hold must return engine to Monitoring"
    );
    assert!(!daemon.core().is_in_grace_period());

    // Continued N2 sleep packets during the 15-min cooldown must NOT re-trigger Grace Period
    let follow_end = (split_idx + 120).min(trace_events.len());
    send_trace_slice(
        &wearable_tx,
        &trace_events[split_idx..follow_end],
        start_utc.timestamp_millis() - 1_460_000,
    )
    .await;
    tokio::task::yield_now().await;
    assert!(!daemon.core().is_in_grace_period());
    assert_eq!(
        daemon.core().status().monitoring_state.to_string(),
        "Monitoring",
        "15-min false-positive cooldown must suppress re-entering Candidate/Confirmed"
    );

    drop(wearable_tx);
    let closed_session_id = await_session_closed_and_calibration(&mut sub).await;

    let mut rpc = IpcClient::connect(&paths.socket_path).await.unwrap();
    let detail = rpc
        .request("session.get", json!({ "id": closed_session_id }))
        .await
        .unwrap();
    assert_eq!(detail["session"]["grace_cancel_count"], 1);
    assert_eq!(detail["session"]["snooze_count"], 0);
    assert_eq!(detail["session"]["rapid_wake_fp_count"], 0);

    let list = rpc
        .request("session.list", json!({ "limit": 10 }))
        .await
        .unwrap();
    assert_eq!(list["sessions"][0]["fp_count"], 1);

    let cal = rpc.request("calibration.status", json!({})).await.unwrap();
    assert!(
        cal["tau_p_sleep_offset"].as_f64().unwrap() > 0.0,
        "expired Activity Hold cancellation must apply strong_fp calibration threshold penalty: {cal}"
    );
    assert!(
        cal["dwell_adjust_sec"].as_i64().unwrap() > 0,
        "expired Activity Hold cancellation must apply strong_fp dwell penalty: {cal}"
    );

    daemon.shutdown().await;
}
