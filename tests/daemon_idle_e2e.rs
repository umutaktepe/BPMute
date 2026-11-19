use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use bpmute::actions::fake::{FakeActionExecutor, FakeNotifier};
use bpmute::config::{ActionsConfig, CliOverrides, LoadedConfig, PowerAction, ScheduleConfig};
use bpmute::daemon::{DaemonClock, DaemonHandle, DaemonOptions, VirtualClock, WearableEvent};
use bpmute::idle::fake::FakeIdleProbe;
use bpmute::idle::traits::{IdleBackendKind, IdleProbe};
use bpmute::paths::{MapEnv, ResolvedPaths};

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
        let (tx, rx) = mpsc::channel(64);
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

    // Activity after settle delay cancels Grace Period immediately
    idle_probe.trigger_activity();
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(
        !daemon.core().is_in_grace_period(),
        "activity after 1s settle delay must cancel grace period"
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

    daemon.shutdown().await;
}
