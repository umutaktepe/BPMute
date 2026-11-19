use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use tokio::sync::mpsc;

use bpmute::actions::fake::FakeNotifier;
use bpmute::actions::grace::{GraceControlCmd, GraceCoordinator, GraceRunOutcome, GraceRunParams};
use bpmute::config::{ActionsConfig, PowerAction};
use bpmute::daemon::{DaemonClock, VirtualClock};
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
