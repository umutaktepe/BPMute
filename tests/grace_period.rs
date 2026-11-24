mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use tokio::sync::mpsc;

use bpmute::actions::fake::FakeNotifier;
use bpmute::actions::grace::{GraceControlCmd, GraceCoordinator, GraceRunOutcome, GraceRunParams};
use bpmute::actions::{GraceNoticeSpec, GracePhase};
use bpmute::config::{ActionsConfig, PowerAction};
use bpmute::daemon::{DaemonClock, VirtualClock};
use bpmute::idle::fake::FakeIdleProbe;
use bpmute::ipc::pubsub::EventBroker;
use bpmute::sleep_engine::{GraceOutcome, GraceResult};

#[tokio::test(start_paused = true)]
async fn test_grace_period_countdown_ticks_and_notification_cadence() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let broker = Arc::new(EventBroker::default());
    let (_sub_id, queue) = broker.subscribe();

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::new(FakeIdleProbe::default()),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

    let (_ctrl_tx, ctrl_rx) = mpsc::channel(8);

    let config = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(15), // 15 seconds
        ..ActionsConfig::default()
    };

    let params = GraceRunParams {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: false,
    };

    let outcome = coordinator.run(params, ctrl_rx).await;

    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Expired,
            ..
        })
    ));

    // Verify notifications cadence:
    // Started at 15s (id=1, rem=15/15)
    // Ticked to 10s (replaces_id=1, rem=10/15)
    // Then every 1s: 9, 8, 7, 6, 5, 4, 3, 2, 1
    // Closed on exit
    let events = notifier.events();
    assert!(
        events
            .iter()
            .any(|e| e.contains("grace_notice: id=1, rem=15/15, urg=Normal"))
    );
    assert!(
        events
            .iter()
            .any(|e| e.contains("grace_notice: id=1, rem=10/15, urg=Normal"))
    );
    assert!(
        events
            .iter()
            .any(|e| e.contains("grace_notice: id=1, rem=1/15, urg=Normal"))
    );
    assert!(events.iter().any(|e| e == "close_notice:1"));

    // Verify IPC GraceTick push events
    let mut tick_count = 0;
    while let Some(ev) = queue.pop_next() {
        if ev.event == "GraceTick" {
            tick_count += 1;
        }
    }
    assert_eq!(tick_count, 16);
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_cancel_from_notification() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let broker = Arc::new(EventBroker::default());

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::new(FakeIdleProbe::default()),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

    let (_ctrl_tx, ctrl_rx) = mpsc::channel(8);

    let config = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(60),
        ..ActionsConfig::default()
    };

    let params = GraceRunParams {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: false,
    };

    let n_clone = Arc::clone(&notifier);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        // User clicks "cancel" in notification
        n_clone.emit_action(1, "cancel");
    });

    let outcome = coordinator.run(params, ctrl_rx).await;

    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));

    // Notice closed on exit
    let events = notifier.events();
    assert!(events.iter().any(|e| e == "close_notice:1"));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_snooze_from_command() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let broker = Arc::new(EventBroker::default());

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::new(FakeIdleProbe::default()),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

    let (ctrl_tx, ctrl_rx) = mpsc::channel(8);

    let config = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(30),
        snooze_duration_min: 45,
        ..ActionsConfig::default()
    };

    let params = GraceRunParams {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: false,
    };

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _ = ctrl_tx
            .send(GraceControlCmd::Snooze { duration_sec: None })
            .await;
    });

    let outcome = coordinator.run(params, ctrl_rx).await;

    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Snoozed { duration_sec: 2700 },
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_swipe_dismiss_is_non_cancelling() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let broker = Arc::new(EventBroker::default());

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::new(FakeIdleProbe::default()),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

    let (_ctrl_tx, ctrl_rx) = mpsc::channel(8);

    let config = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(12),
        ..ActionsConfig::default()
    };

    let params = GraceRunParams {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: false,
    };

    let n_clone = Arc::clone(&notifier);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        // User swiped notification (reason = 2: dismissed by user)
        n_clone.emit_closed(1, 2);
    });

    let outcome = coordinator.run(params, ctrl_rx).await;

    // Countdown continued to expiration!
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Expired,
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_asymmetric_signal_lost() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let broker = Arc::new(EventBroker::default());

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::new(FakeIdleProbe::default()),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

    let (ctrl_tx, ctrl_rx) = mpsc::channel(8);

    let config = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(60),
        ..ActionsConfig::default()
    };

    let params = GraceRunParams {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: false,
    };

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let _ = ctrl_tx.send(GraceControlCmd::SignalLost).await;
    });

    let outcome = coordinator.run(params, ctrl_rx).await;

    assert!(matches!(outcome, GraceRunOutcome::AsymmetricSignalLost));

    // Notice closed and info notification shown
    let events = notifier.events();
    assert!(events.iter().any(|e| e == "close_notice:1"));
    assert!(events.iter().any(|e| e.contains("info_notice")));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_arousal_abort_no_penalty() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let broker = Arc::new(EventBroker::default());

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::new(FakeIdleProbe::default()),
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

    let (ctrl_tx, ctrl_rx) = mpsc::channel(8);

    let config = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(30),
        ..ActionsConfig::default()
    };

    let params = GraceRunParams {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: false,
    };

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        let _ = ctrl_tx.send(GraceControlCmd::ArousalAbort).await;
    });

    let outcome = coordinator.run(params, ctrl_rx).await;

    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::AbortedNoPenalty,
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_early_exit_emits_zero_tick_and_respects_user_dismiss() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::default());
    let broker = Arc::new(EventBroker::default());
    let (_sub_id, queue) = broker.subscribe();

    let coordinator = make_coordinator(&notifier, &idle_probe, &clock, &broker);

    let (_ctrl_tx, ctrl_rx) = mpsc::channel(8);
    let config = ActionsConfig {
        power_action: PowerAction::None,
        grace_period_sec: Some(25),
        ..ActionsConfig::default()
    };
    let params = GraceRunParams {
        session_id: "s-early".to_string(),
        wearable_id: "w1".to_string(),
        config,
        dry_run: true,
    };

    let n_clone = Arc::clone(&notifier);
    let i_clone = Arc::clone(&idle_probe);
    tokio::spawn(async move {
        // User dismisses notification at t=2s (reason=2)
        tokio::time::sleep(Duration::from_secs(2)).await;
        n_clone.emit_closed(1, 2);
        // Wait past t=5s cadence boundary, then trigger desktop activity at t=8s
        tokio::time::sleep(Duration::from_secs(6)).await;
        i_clone.trigger_activity();
    });

    let outcome = coordinator.run(params, ctrl_rx).await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));

    // Must NOT re-pop notification at t=5s after user dismissed it at t=2s
    let grace_notices: Vec<_> = notifier
        .events()
        .into_iter()
        .filter(|e| e.starts_with("grace_notice:"))
        .collect();
    assert_eq!(
        grace_notices.len(),
        1,
        "must not re-show grace notice after user dismiss; got: {grace_notices:?}"
    );

    // Must emit terminal GraceTick with remaining_sec = 0 on early exit
    let mut ticks = Vec::new();
    while let Some(ev) = queue.pop_next() {
        if ev.event == "GraceTick" {
            ticks.push(ev.data["remaining_sec"].as_u64().unwrap());
        }
    }
    assert_eq!(
        ticks.last().copied(),
        Some(0),
        "early exit must emit terminal GraceTick(0); ticks={ticks:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_desktop_notifier_keeps_persistent_dbus_connection_and_resident_hints() {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use bpmute::actions::DesktopNotifier;
    use bpmute::actions::traits::{NotificationEvent, Notifier, Urgency};
    use tempfile::TempDir;
    use zbus::interface;
    use zbus::object_server::SignalContext;
    use zbus::zvariant::OwnedValue;

    #[derive(Clone, Default)]
    struct RecordedNotify {
        sender: String,
        replaces_id: u32,
        body: String,
        actions: Vec<String>,
        resident: Option<bool>,
        transient: Option<bool>,
        expire_timeout: i32,
    }

    #[derive(Default)]
    struct MockNotificationsState {
        next_id: u32,
        calls: Vec<RecordedNotify>,
        closed_ids: Vec<u32>,
    }

    struct MockNotifications {
        state: Arc<Mutex<MockNotificationsState>>,
    }

    #[interface(name = "org.freedesktop.Notifications")]
    impl MockNotifications {
        #[allow(clippy::too_many_arguments, reason = "D-Bus Notify signature")]
        async fn notify(
            &self,
            #[zbus(header)] hdr: zbus::message::Header<'_>,
            _app_name: String,
            replaces_id: u32,
            _app_icon: String,
            _summary: String,
            body: String,
            actions: Vec<String>,
            hints: HashMap<String, OwnedValue>,
            expire_timeout: i32,
        ) -> u32 {
            let mut guard = self.state.lock().unwrap();
            let id = if replaces_id != 0 {
                replaces_id
            } else {
                guard.next_id = guard.next_id.saturating_add(1);
                guard.next_id
            };
            let resident = hints.get("resident").and_then(|v| bool::try_from(v).ok());
            let transient = hints.get("transient").and_then(|v| bool::try_from(v).ok());
            guard.calls.push(RecordedNotify {
                sender: hdr.sender().map(|s| s.to_string()).unwrap_or_default(),
                replaces_id,
                body,
                actions,
                resident,
                transient,
                expire_timeout,
            });
            id
        }

        async fn close_notification(&self, id: u32) {
            self.state.lock().unwrap().closed_ids.push(id);
        }

        #[zbus(signal)]
        async fn action_invoked(
            ctx: &SignalContext<'_>,
            id: u32,
            action_key: &str,
        ) -> zbus::Result<()>;
    }

    let temp = TempDir::new().unwrap();
    let (_dbus_guard, bus_addr) = common::dbus::spawn_private_session_dbus(&temp);

    let state = Arc::new(Mutex::new(MockNotificationsState::default()));
    let server_conn = zbus::connection::Builder::address(bus_addr.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at(
            "/org/freedesktop/Notifications",
            MockNotifications {
                state: Arc::clone(&state),
            },
        )
        .unwrap()
        .build()
        .await
        .unwrap();

    let notifier = DesktopNotifier::with_bus_address(&bus_addr);
    let mut rx = notifier.subscribe_actions();

    let id1 = notifier
        .show_grace_notice(
            0,
            GraceNoticeSpec {
                remaining_sec: 90,
                total_sec: 90,
                phase: GracePhase::Countdown,
                urgency: Urgency::Normal,
                dry_run: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(id1, 1);

    let id2 = notifier
        .show_grace_notice(
            id1,
            GraceNoticeSpec {
                remaining_sec: 85,
                total_sec: 90,
                phase: GracePhase::Countdown,
                urgency: Urgency::Normal,
                dry_run: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(id2, 1);

    let id3 = notifier
        .show_grace_notice(
            id1,
            GraceNoticeSpec {
                remaining_sec: 10,
                total_sec: 90,
                phase: GracePhase::ActivityHold,
                urgency: Urgency::Normal,
                dry_run: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(id3, 1);

    // Emit ActionInvoked from the mock server and verify subscription receives it
    let iface_ref = server_conn
        .object_server()
        .interface::<_, MockNotifications>("/org/freedesktop/Notifications")
        .await
        .unwrap();
    MockNotifications::action_invoked(iface_ref.signal_context(), id1, "cancel")
        .await
        .unwrap();
    let ev = rx.recv().await.expect("must receive ActionInvoked");
    assert_eq!(
        ev,
        NotificationEvent::ActionInvoked {
            id: 1,
            action_key: "cancel".to_string(),
        }
    );

    notifier.close_notice(id1).await.unwrap();

    let guard = state.lock().unwrap();
    assert_eq!(guard.calls.len(), 3);
    assert!(!guard.calls[0].sender.is_empty());
    assert_eq!(
        guard.calls[0].sender, guard.calls[1].sender,
        "DesktopNotifier must reuse a persistent D-Bus sender connection"
    );
    assert_eq!(guard.calls[0].replaces_id, 0);
    assert_eq!(guard.calls[1].replaces_id, 1);
    assert_eq!(guard.calls[2].replaces_id, 1);
    assert_eq!(
        guard.calls[0].body,
        "Sleep action in 90s... Click Cancel if you're awake."
    );
    assert_eq!(
        guard.calls[2].body,
        "Activity detected — cancelling in 10s. Click Snooze to pause sleep actions."
    );
    assert_eq!(guard.calls[0].resident, Some(true));
    assert_eq!(guard.calls[0].transient, Some(false));
    assert_eq!(guard.calls[0].expire_timeout, 0);
    assert!(guard.calls[0].actions.iter().any(|a| a == "cancel"));
    assert!(guard.calls[0].actions.iter().any(|a| a == "snooze"));
    assert!(guard.calls[2].actions.iter().any(|a| a == "cancel"));
    assert!(guard.calls[2].actions.iter().any(|a| a == "snooze"));
    assert_eq!(guard.closed_ids, vec![1]);
}

async fn run_activity_hold_case<F, Fut>(
    session_id: &str,
    grace_period_sec: u32,
    snooze_duration_min: u32,
    driver: F,
) -> (GraceRunOutcome, Duration, Vec<String>, Vec<(u64, String)>)
where
    F: FnOnce(Arc<FakeNotifier>, Arc<FakeIdleProbe>, mpsc::Sender<GraceControlCmd>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let notifier = Arc::new(FakeNotifier::new());
    let idle_probe = Arc::new(FakeIdleProbe::default());
    let broker = Arc::new(EventBroker::default());
    let (_sub_id, queue) = broker.subscribe();
    let coordinator = make_coordinator(&notifier, &idle_probe, &clock, &broker);
    let (ctrl_tx, ctrl_rx) = mpsc::channel(8);
    let params = GraceRunParams {
        session_id: session_id.to_string(),
        wearable_id: "w1".to_string(),
        config: ActionsConfig {
            power_action: PowerAction::Suspend,
            grace_period_sec: Some(grace_period_sec),
            snooze_duration_min,
            ..ActionsConfig::default()
        },
        dry_run: false,
    };

    let start = tokio::time::Instant::now();
    let _keep_ctrl_tx = ctrl_tx.clone();
    tokio::spawn(driver(
        Arc::clone(&notifier),
        Arc::clone(&idle_probe),
        ctrl_tx,
    ));
    let outcome = coordinator.run(params, ctrl_rx).await;
    let elapsed = start.elapsed();

    let mut ticks = Vec::new();
    while let Some(ev) = queue.pop_next() {
        if ev.event == "GraceTick" {
            ticks.push((
                ev.data["remaining_sec"].as_u64().unwrap(),
                ev.data["phase"].as_str().unwrap().to_string(),
            ));
        }
    }
    (outcome, elapsed, notifier.events(), ticks)
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_activity_hold_transition_cadence_and_auto_cancel() {
    let (outcome, elapsed, events, ticks) =
        run_activity_hold_case("s-hold", 30, 30, |_, i, _| async move {
            // Trigger post-settle desktop activity at t=2s
            tokio::time::sleep(Duration::from_secs(2)).await;
            i.trigger_activity();
        })
        .await;

    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));
    assert_eq!(
        elapsed,
        Duration::from_secs(12),
        "2s countdown + 10s fixed Activity Hold must expire at t=12s"
    );

    // Verify notification cadence: initial Countdown (30/30), immediate ActivityHold (10/30),
    // then 9..=1 every 1s, then close_notice:1
    let hold_notices: Vec<_> = events
        .iter()
        .filter(|e| e.contains("phase=activity_hold"))
        .collect();
    assert_eq!(
        hold_notices.len(),
        10,
        "ActivityHold must emit 10 notices (10s down to 1s); got {hold_notices:?}"
    );
    assert!(hold_notices[0].contains("rem=10/30"));
    assert!(hold_notices[9].contains("rem=1/30"));
    assert!(events.iter().any(|e| e == "close_notice:1"));

    // Verify GraceTick sequence: (30..=28, countdown), (10..=0, activity_hold)
    let expected: Vec<(u64, String)> = [30, 29, 28]
        .into_iter()
        .map(|s| (s, "countdown".to_string()))
        .chain((0..=10).rev().map(|s| (s, "activity_hold".to_string())))
        .collect();
    assert_eq!(ticks, expected);
}

fn make_coordinator(
    notifier: &Arc<FakeNotifier>,
    idle_probe: &Arc<FakeIdleProbe>,
    clock: &Arc<VirtualClock>,
    broker: &Arc<EventBroker>,
) -> GraceCoordinator {
    GraceCoordinator::new(
        Arc::clone(notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(idle_probe) as Arc<dyn bpmute::idle::IdleProbe>,
        Arc::clone(clock) as Arc<dyn DaemonClock>,
        Arc::clone(broker),
    )
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_activity_hold_grants_full_10s_when_remaining_below_10_and_non_resetting()
{
    let (outcome, elapsed, _, _) = run_activity_hold_case("s-short", 5, 30, |_, i, _| async move {
        // Activity during 1s settle delay must be ignored
        tokio::time::sleep(Duration::from_millis(500)).await;
        i.trigger_activity();
        // Activity at t=3s (remaining_sec == 2 < 10) enters full 10s Activity Hold
        tokio::time::sleep(Duration::from_millis(2500)).await;
        i.trigger_activity();
        // Repeated input every 2s during Activity Hold must NOT reset or extend the 10s timer
        for _ in 0..4 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            i.trigger_activity();
        }
    })
    .await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));
    assert_eq!(elapsed, Duration::from_secs(13));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_activity_hold_caps_long_grace_at_10s() {
    let (outcome, elapsed, _, _) = run_activity_hold_case("s-long", 90, 30, |_, i, _| async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        i.trigger_activity();
    })
    .await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));
    assert_eq!(elapsed, Duration::from_secs(12));
}

#[tokio::test(start_paused = true)]
async fn test_grace_period_activity_hold_actions_commands_and_dismiss() {
    // 1. Notification "snooze" action during ActivityHold -> Snoozed immediately
    let (outcome, elapsed, _, _) =
        run_activity_hold_case("s-snooze-btn", 30, 15, |n, i, _| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            i.trigger_activity();
            tokio::time::sleep(Duration::from_secs(2)).await;
            n.emit_action(1, "snooze");
        })
        .await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Snoozed { duration_sec: 900 },
            ..
        })
    ));
    assert_eq!(elapsed, Duration::from_secs(4));

    // 2. Notification "default" and "cancel" actions during ActivityHold -> Cancelled immediately
    for action_key in ["default", "cancel"] {
        let key = action_key.to_string();
        let (outcome, elapsed, _, _) =
            run_activity_hold_case(&format!("s-{action_key}"), 30, 30, |n, i, _| async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                i.trigger_activity();
                tokio::time::sleep(Duration::from_secs(1)).await;
                n.emit_action(1, &key);
            })
            .await;
        assert!(matches!(
            outcome,
            GraceRunOutcome::Completed(GraceOutcome {
                outcome: GraceResult::Cancelled,
                ..
            })
        ));
        assert_eq!(elapsed, Duration::from_secs(3));
    }

    // 3. GraceControlCmd::Snooze & Cancel during ActivityHold
    let (outcome, elapsed, _, _) =
        run_activity_hold_case("s-cmd-snooze", 30, 30, |_, i, tx| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            i.trigger_activity();
            tokio::time::sleep(Duration::from_secs(3)).await;
            let _ = tx
                .send(GraceControlCmd::Snooze {
                    duration_sec: Some(600),
                })
                .await;
        })
        .await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Snoozed { duration_sec: 600 },
            ..
        })
    ));
    assert_eq!(elapsed, Duration::from_secs(5));

    let (outcome, elapsed, _, _) =
        run_activity_hold_case("s-cmd-cancel", 30, 30, |_, i, tx| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            i.trigger_activity();
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = tx.send(GraceControlCmd::Cancel).await;
        })
        .await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));
    assert_eq!(elapsed, Duration::from_secs(3));

    // 4. NotificationClosed(reason = 2) mid-second during ActivityHold does not extend or break 10s auto-cancel
    let (outcome, elapsed, events, _) =
        run_activity_hold_case("s-dismiss-hold", 30, 30, |n, i, _| async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            i.trigger_activity();
            tokio::time::sleep(Duration::from_millis(1500)).await;
            n.emit_closed(1, 2);
        })
        .await;
    assert!(matches!(
        outcome,
        GraceRunOutcome::Completed(GraceOutcome {
            outcome: GraceResult::Cancelled,
            ..
        })
    ));
    assert_eq!(
        elapsed,
        Duration::from_secs(12),
        "mid-second NotificationClosed(2) must not extend the 10s Activity Hold"
    );
    let hold_notices: Vec<_> = events
        .into_iter()
        .filter(|e| e.contains("phase=activity_hold"))
        .collect();
    // Shown at ActivityHold entry (10s) and t=3s (9s), then suppressed after t=3.5s dismiss
    assert_eq!(hold_notices.len(), 2, "got {hold_notices:?}");

    // 5. Preemption signals (ArousalAbort, PrepareForSleep, SignalLost) terminate ActivityHold immediately
    for (cmd, expect_signal_lost) in [
        (GraceControlCmd::ArousalAbort, false),
        (GraceControlCmd::PrepareForSleep, false),
        (GraceControlCmd::SignalLost, true),
    ] {
        let (outcome, elapsed, _, _) =
            run_activity_hold_case("s-preempt", 30, 30, move |_, i, tx| async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                i.trigger_activity();
                tokio::time::sleep(Duration::from_secs(2)).await;
                let _ = tx.send(cmd).await;
            })
            .await;
        assert_eq!(elapsed, Duration::from_secs(4));
        if expect_signal_lost {
            assert!(matches!(outcome, GraceRunOutcome::AsymmetricSignalLost));
        } else {
            assert!(matches!(
                outcome,
                GraceRunOutcome::Completed(GraceOutcome {
                    outcome: GraceResult::AbortedNoPenalty,
                    ..
                })
            ));
        }
    }
}
