use std::sync::Arc;

use chrono::DateTime;

use bpmute::actions::fake::{FakeActionExecutor, FakeNotifier};
use bpmute::actions::pipeline::{PipelineContext, PipelineResult, SleepActionPipeline};
use bpmute::actions::traits::{
    ActionExecutor, CommandOutput, PowerActionResult, is_player_ignored,
};
use bpmute::config::{ActionsConfig, PowerAction};
use bpmute::daemon::{DaemonClock, VirtualClock};

#[tokio::test(start_paused = true)]
async fn test_pipeline_executes_in_exact_order_with_settle_delay() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());

    executor.set_available_players(&[
        "org.mpris.MediaPlayer2.vlc",
        "org.mpris.MediaPlayer2.spotify",
    ]);
    executor.set_playing_players(&["org.mpris.MediaPlayer2.vlc"]);

    let config = ActionsConfig {
        pause_media: true,
        ignore_players: vec!["spotify".to_string()],
        custom_command: vec!["/bin/true".to_string()],
        custom_command_timeout_sec: 10,
        abort_on_hook_failure: true,
        lock_screen: true,
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        snooze_duration_min: 30,
        dry_run: false,
        strict_idle_backend: false,
    };

    let pipeline = SleepActionPipeline::new(
        Arc::clone(&executor) as Arc<dyn bpmute::actions::traits::ActionExecutor>,
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
    );

    let ctx = PipelineContext {
        session_id: "018e3a24-1111-7000-8000-000000000001".to_string(),
        wearable_id: "018e3a24-2222-7000-8000-000000000002".to_string(),
        event: "sleep_trigger".to_string(),
        dry_run: false,
    };

    let res = pipeline
        .execute(&ctx, &config)
        .await
        .expect("pipeline success");

    assert_eq!(res.paused_players, vec!["org.mpris.MediaPlayer2.vlc"]);
    assert!(res.command_executed);
    assert!(res.screen_locked);
    assert_eq!(res.power_action_result, Some(PowerActionResult::Executed));

    let events = executor.events();
    assert_eq!(
        events,
        vec![
            "pause_mpris:[org.mpris.MediaPlayer2.vlc]".to_string(),
            "custom_command:/bin/true".to_string(),
            "lock_screen".to_string(),
            "power_action:suspend".to_string(),
        ]
    );

    // Verify 1.5s settle delay occurred in virtual clock
    assert_eq!(clock.now_utc().timestamp_millis(), 1_700_000_001_500);
}

#[test]
fn test_ignore_players_matches_prefix_and_exact_case_insensitively() {
    let ignore_list = vec![
        "spotify".to_string(),
        "org.mpris.MediaPlayer2.chrome".to_string(),
    ];

    assert!(is_player_ignored(
        "org.mpris.MediaPlayer2.spotify",
        &ignore_list
    ));
    assert!(is_player_ignored(
        "org.mpris.MediaPlayer2.Spotify.instance123",
        &ignore_list
    ));
    assert!(is_player_ignored(
        "org.mpris.MediaPlayer2.chrome",
        &ignore_list
    ));
    assert!(is_player_ignored(
        "org.mpris.MediaPlayer2.chrome.instance42",
        &ignore_list
    ));
    assert!(!is_player_ignored(
        "org.mpris.MediaPlayer2.vlc",
        &ignore_list
    ));
    assert!(!is_player_ignored(
        "org.mpris.MediaPlayer2.firefox",
        &ignore_list
    ));
}

#[tokio::test(start_paused = true)]
async fn test_settle_delay_skipped_if_zero_players_paused_or_no_subsequent_steps() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());

    // 0 players playing
    executor.set_playing_players(&[]);

    let config = ActionsConfig {
        pause_media: true,
        ignore_players: vec![],
        custom_command: vec!["/bin/true".to_string()],
        custom_command_timeout_sec: 10,
        abort_on_hook_failure: false,
        lock_screen: false,
        power_action: PowerAction::None,
        grace_period_sec: Some(30),
        snooze_duration_min: 30,
        dry_run: false,
        strict_idle_backend: false,
    };

    let pipeline = SleepActionPipeline::new(
        Arc::clone(&executor) as Arc<dyn bpmute::actions::traits::ActionExecutor>,
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
    );

    let ctx = PipelineContext {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        event: "sleep_trigger".to_string(),
        dry_run: false,
    };

    let _ = pipeline.execute(&ctx, &config).await.unwrap();

    // No delay because 0 players were paused
    assert_eq!(clock.now_utc().timestamp_millis(), 1_700_000_000_000);
}

#[tokio::test(start_paused = true)]
async fn test_abort_on_hook_failure_vetoes_remaining_steps() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());

    executor.set_command_override(Ok(CommandOutput {
        exit_code: Some(1),
        success: false,
        stdout_truncated: String::new(),
        stderr_truncated: "command failed".to_string(),
    }));

    let config = ActionsConfig {
        pause_media: false,
        ignore_players: vec![],
        custom_command: vec!["/bin/false".to_string()],
        custom_command_timeout_sec: 10,
        abort_on_hook_failure: true,
        lock_screen: true,
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        snooze_duration_min: 30,
        dry_run: false,
        strict_idle_backend: false,
    };

    let pipeline = SleepActionPipeline::new(
        Arc::clone(&executor) as Arc<dyn bpmute::actions::traits::ActionExecutor>,
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
    );

    let ctx = PipelineContext {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        event: "sleep_trigger".to_string(),
        dry_run: false,
    };

    let res = pipeline.execute(&ctx, &config).await.unwrap();

    assert!(res.command_executed);
    // Vetoed: lock_screen and power_action should NOT have executed!
    assert!(!res.screen_locked);
    assert_eq!(res.power_action_result, None);

    let events = executor.events();
    assert_eq!(events, vec!["custom_command:/bin/false".to_string()]);
}

#[tokio::test(start_paused = true)]
async fn test_inhibitor_refusal_preserves_media_pause_and_emits_warning() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());

    executor.set_playing_players(&["org.mpris.MediaPlayer2.vlc"]);
    executor.set_power_action_override(Ok(PowerActionResult::RefusedInhibited(
        "system upgrade in progress".to_string(),
    )));

    let config = ActionsConfig {
        pause_media: true,
        ignore_players: vec![],
        custom_command: vec![],
        custom_command_timeout_sec: 10,
        abort_on_hook_failure: false,
        lock_screen: false,
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        snooze_duration_min: 30,
        dry_run: false,
        strict_idle_backend: false,
    };

    let pipeline = SleepActionPipeline::new(
        Arc::clone(&executor) as Arc<dyn bpmute::actions::traits::ActionExecutor>,
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
    );

    let ctx = PipelineContext {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        event: "sleep_trigger".to_string(),
        dry_run: false,
    };

    let res = pipeline.execute(&ctx, &config).await.unwrap();

    // Preserved paused media
    assert_eq!(res.paused_players, vec!["org.mpris.MediaPlayer2.vlc"]);
    assert_eq!(
        res.power_action_result,
        Some(PowerActionResult::RefusedInhibited(
            "system upgrade in progress".to_string()
        ))
    );

    // Warning notification emitted with transient = false
    let notifier_events = notifier.events();
    assert!(
        notifier_events
            .iter()
            .any(|e| e.contains("transient=false") && e.contains("Sleep action inhibited"))
    );
}

#[tokio::test(start_paused = true)]
async fn test_dry_run_skips_all_executor_actions() {
    let clock = Arc::new(VirtualClock::starting_at(
        DateTime::from_timestamp_millis(1_700_000_000_000).unwrap(),
    ));
    let executor = Arc::new(FakeActionExecutor::new());
    let notifier = Arc::new(FakeNotifier::new());

    executor.set_playing_players(&["org.mpris.MediaPlayer2.vlc"]);

    let config = ActionsConfig {
        pause_media: true,
        ignore_players: vec![],
        custom_command: vec!["/bin/true".to_string()],
        custom_command_timeout_sec: 10,
        abort_on_hook_failure: true,
        lock_screen: true,
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        snooze_duration_min: 30,
        dry_run: false,
        strict_idle_backend: false,
    };

    let pipeline = SleepActionPipeline::new(
        Arc::clone(&executor) as Arc<dyn bpmute::actions::traits::ActionExecutor>,
        Arc::clone(&notifier) as Arc<dyn bpmute::actions::traits::Notifier>,
        Arc::clone(&clock) as Arc<dyn DaemonClock>,
    );

    let ctx = PipelineContext {
        session_id: "s1".to_string(),
        wearable_id: "w1".to_string(),
        event: "dry_run".to_string(),
        dry_run: true,
    };

    let res = pipeline.execute(&ctx, &config).await.unwrap();

    assert_eq!(res, PipelineResult::default());
    assert!(executor.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn test_has_playing_players_without_side_effects() {
    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_playing_players(&[
        "org.mpris.MediaPlayer2.spotify",
        "org.mpris.MediaPlayer2.vlc",
    ]);

    // With ignore matching spotify
    let ignore = vec!["spotify".to_string()];
    let has_playing = executor.has_playing_players(&ignore).await.unwrap();
    assert!(has_playing, "vlc is still playing and not ignored");

    // Probing playing status must NOT generate pause_mpris events
    assert!(executor.events().is_empty());

    // When both are ignored
    let ignore_all = vec!["spotify".to_string(), "vlc".to_string()];
    let has_playing_ignored = executor.has_playing_players(&ignore_all).await.unwrap();
    assert!(!has_playing_ignored, "all playing players are ignored");
    assert!(executor.events().is_empty());
}

#[tokio::test(start_paused = true)]
async fn test_watch_mpris_resume_timeout_and_trigger() {
    let executor = Arc::new(FakeActionExecutor::new());

    // 1) Test 300s timeout completes with false
    let ex_clone = Arc::clone(&executor);
    let watch_handle = tokio::spawn(async move {
        ex_clone
            .watch_mpris_resume(&["org.mpris.MediaPlayer2.vlc".to_string()])
            .await
    });

    tokio::time::sleep(std::time::Duration::from_secs(301)).await;
    let resumed = watch_handle.await.unwrap();
    assert!(!resumed, "expected timeout to return false");

    // 2) Test trigger_mpris_resume causes watch to return true
    let executor2 = Arc::new(FakeActionExecutor::new());
    let ex_clone2 = Arc::clone(&executor2);
    let watch_handle2 = tokio::spawn(async move {
        ex_clone2
            .watch_mpris_resume(&["org.mpris.MediaPlayer2.vlc".to_string()])
            .await
    });

    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    executor2.trigger_mpris_resume();
    let resumed2 = watch_handle2.await.unwrap();
    assert!(resumed2, "expected triggered resume to return true");
}
