use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use tokio::sync::mpsc;

use bpmute::actions::fake::FakeNotifier;
use bpmute::actions::grace::{GraceControlCmd, GraceCoordinator, GraceRunOutcome, GraceRunParams};
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

    let coordinator = GraceCoordinator::new(
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(&idle_probe) as Arc<dyn bpmute::idle::IdleProbe>,
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
        Arc::clone(&broker),
    );

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
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::Mutex;

    use bpmute::actions::DesktopNotifier;
    use bpmute::actions::traits::{NotificationEvent, Notifier, Urgency};
    use tempfile::TempDir;
    use zbus::interface;
    use zbus::object_server::SignalContext;
    use zbus::zvariant::OwnedValue;

    struct ChildGuard(Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[derive(Clone, Default)]
    struct RecordedNotify {
        sender: String,
        replaces_id: u32,
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
            _body: String,
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
    let bus_sock = temp.path().join("session-bus.sock");
    let listen_addr = format!("unix:path={}", bus_sock.display());
    let mut child = Command::new("dbus-daemon")
        .args([
            "--session",
            "--nopidfile",
            "--nosyslog",
            "--nofork",
            "--print-address=1",
            "--address",
            &listen_addr,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("dbus-daemon must spawn");
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut bus_addr = String::new();
    reader.read_line(&mut bus_addr).unwrap();
    let bus_addr = bus_addr.trim().to_string();
    let _dbus_guard = ChildGuard(child);

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
        .show_grace_notice(0, 90, 90, Urgency::Normal, true)
        .await
        .unwrap();
    assert_eq!(id1, 1);

    let id2 = notifier
        .show_grace_notice(id1, 85, 90, Urgency::Normal, true)
        .await
        .unwrap();
    assert_eq!(id2, 1);

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
    assert_eq!(guard.calls.len(), 2);
    assert!(!guard.calls[0].sender.is_empty());
    assert_eq!(
        guard.calls[0].sender, guard.calls[1].sender,
        "DesktopNotifier must reuse a persistent D-Bus sender connection"
    );
    assert_eq!(guard.calls[0].replaces_id, 0);
    assert_eq!(guard.calls[1].replaces_id, 1);
    assert_eq!(guard.calls[0].resident, Some(true));
    assert_eq!(guard.calls[0].transient, Some(false));
    assert_eq!(guard.calls[0].expire_timeout, 0);
    assert!(guard.calls[0].actions.iter().any(|a| a == "cancel"));
    assert!(guard.calls[0].actions.iter().any(|a| a == "snooze"));
    assert_eq!(guard.closed_ids, vec![1]);
}
