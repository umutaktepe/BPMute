use std::fs;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot, watch};

use bpmute::actions::fake::FakeActionExecutor;
use bpmute::actions::traits::{ActionExecutor, BoxFuture, PowerActionResult};
use bpmute::bluetooth::agent::AgentAuthRequest;
use bpmute::cli::args::Cli;
use bpmute::cli::execute_cli;
use bpmute::config::{ActionsConfig, CliOverrides, LoadedConfig, PowerAction, ScheduleConfig};
use bpmute::daemon::{
    DaemonCore, DaemonHandle, DaemonOptions, EnrollmentPairingHook, VirtualClock,
};
use bpmute::exit_code::ExitCode;
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::{
    AlertSeverity, DaemonAlert, DiscoveredWearable, EnrolledWearableStatus, HrSampleEvent,
    MonitoringState, PauseMode, PauseParams, alert_ids,
};
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{
    CalibrationMaturity, ContactStatus, DetectorState, Epoch, EpochFeatures, StreamStatus,
};
use bpmute::storage::EnrolledWearable;
use bpmute::tui::views::header::{GraceBannerLayout, tab_hit_ranges};
use bpmute::tui::{
    GraceState, ModalState, PairingPromptModalState, Tab, Theme, TuiApp, TuiCommand, TuiEvent,
    TuiIpcBridge, UnenrollButton, detect_systemd_service_installed,
};

fn buffer_to_string(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn make_wearable(
    id: &str,
    mac: &str,
    alias: Option<&str>,
    status: StreamStatus,
    battery: Option<u8>,
    body_location: Option<&str>,
) -> EnrolledWearableStatus {
    EnrolledWearableStatus {
        wearable: EnrolledWearable {
            id: id.to_string(),
            mac: mac.to_string(),
            mac_masked: "..:22:33".to_string(),
            address_type: "public".to_string(),
            alias: alias.map(str::to_string),
            advertised_name: None,
            supports_rr: true,
            supports_contact: true,
            hr_var_floor: 0.25,
            typical_sqi: Some(0.95),
            enrolled_at_ms: 1_700_000_000_000,
            last_seen_at_ms: 1_700_000_000_000,
            active: true,
        },
        status,
        battery,
        body_location: body_location.map(str::to_string),
    }
}

fn make_test_paths(temp: &TempDir) -> (ResolvedPaths, MapEnv) {
    let env = MapEnv::new()
        .with(
            "XDG_CONFIG_HOME",
            temp.path().join("cfg").display().to_string(),
        )
        .with(
            "XDG_DATA_HOME",
            temp.path().join("data").display().to_string(),
        )
        .with(
            "XDG_RUNTIME_DIR",
            temp.path().join("run").display().to_string(),
        );
    let paths = ResolvedPaths {
        config_file: temp.path().join("cfg/bpmute/config.toml"),
        data_dir: temp.path().join("data/bpmute"),
        socket_path: temp.path().join("run/bpmute/daemon.sock"),
    };
    (paths, env)
}

async fn wait_for_app_state<F>(
    app: &mut TuiApp,
    event_rx: &mut mpsc::Receiver<TuiEvent>,
    mut pred: F,
) where
    F: FnMut(&TuiApp) -> bool,
{
    for _ in 0..256 {
        if pred(app) {
            return;
        }
        let ev = event_rx
            .recv()
            .await
            .expect("TUI IPC event channel closed before reaching expected state");
        let _ = app.handle_event(ev);
    }
    panic!("TUI app did not reach expected state within event budget");
}

#[tokio::test(start_paused = true)]
async fn test_cli_tui_flags_are_scoped_and_80x24_baseline_renders_cleanly() {
    // Bare `bpmute --no-mouse --ascii` parses cleanly with no subcommand
    let cli = Cli::try_parse_from(["bpmute", "--no-mouse", "--ascii"]).unwrap();
    assert!(cli.no_mouse);
    assert!(cli.ascii);
    assert!(cli.command.is_none());

    // Subcommand trailing TUI flags must be rejected by clap
    assert!(Cli::try_parse_from(["bpmute", "status", "--ascii"]).is_err());
    assert!(Cli::try_parse_from(["bpmute", "daemon", "--no-mouse"]).is_err());

    // Leading TUI flags paired with a subcommand must return UsageOrConfigError
    let temp = TempDir::new().unwrap();
    let (_, env) = make_test_paths(&temp);
    let bad_cli = Cli::try_parse_from(["bpmute", "--ascii", "status"]).unwrap();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let code = execute_cli(bad_cli, &env, &mut out_buf, &mut err_buf).await;
    assert_eq!(code, ExitCode::UsageOrConfigError);

    // 80x24 Dashboard baseline with active wearable, schedule, calibration maturity, and dry-run
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.monitoring_state = MonitoringState::Monitoring;
    app.calibration_maturity = CalibrationMaturity::Calibrated;
    app.armed = true;
    app.dry_run = true;
    app.latest_bpm = Some(64.0);
    app.latest_p_sleep = Some(0.24);
    app.daemon_status = Some(bpmute::ipc::protocol::DaemonStatus {
        v: 1,
        daemon_running: true,
        version: "0.1.0".into(),
        pid: 1,
        uptime_sec: 10,
        monitoring_state: MonitoringState::Monitoring,
        armed: true,
        paused: false,
        paused_until: None,
        pause_mode: None,
        dry_run: true,
        schedule: ScheduleConfig {
            active_hours_enabled: true,
            ..ScheduleConfig::default()
        },
        config_path: String::new(),
        config_loaded_from_file: false,
        socket_path: String::new(),
        log_biometrics: false,
        alerts: Vec::new(),
        idle_backend: Some("ext-idle-notify".into()),
        playing_players_count: Some(1),
    });
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(88),
        Some("Chest"),
    )];

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    assert!(out.contains("1:Dashboard"));
    assert!(out.contains("2:History"));
    assert!(out.contains("3:Wearables"));
    assert!(out.contains("4:Settings"));
    assert!(out.contains("64"));
    assert!(out.contains("Sleep State"));
    assert!(out.contains("Wearable: Polar H10 (88%)"));
    assert!(!out.contains("Device:"), "Must not use avoided term Device");
    assert!(out.contains("Streaming | Calibrated"));
    assert!(out.contains("Armed [DRY] | 22:00-08:00"));
    assert!(out.contains("[1-4] Tabs"));
    assert!(out.contains("[?] Help"));
    assert!(out.contains("[q] Quit"));

    // Navigation shortcuts: 1-4, Tab, BackTab, h/l, Left/Right, ?, Esc, q
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(app.active_tab, Tab::History);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Monitoring Sessions"));

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('3'))));
    assert_eq!(app.active_tab, Tab::Wearables);
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.active_tab, Tab::Settings);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("System Settings & Storage"));

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    assert_eq!(app.active_tab, Tab::Dashboard);
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert_eq!(app.active_tab, Tab::History);
    app.handle_event(TuiEvent::Key(key(KeyCode::BackTab)));
    assert_eq!(app.active_tab, Tab::Dashboard);
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.active_tab, Tab::History);
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('h'))));
    assert_eq!(app.active_tab, Tab::Dashboard);
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert_eq!(app.active_tab, Tab::History);
    app.handle_event(TuiEvent::Key(key(KeyCode::Left)));
    assert_eq!(app.active_tab, Tab::Dashboard);

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.modal, ModalState::Help);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(
        buffer_to_string(terminal.backend().buffer()).contains("BPMute Keyboard & Mouse Shortcuts")
    );
    app.handle_event(TuiEvent::Key(key(KeyCode::Esc)));
    assert_eq!(app.modal, ModalState::None);

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('q'))));
    assert!(app.should_quit);
}

#[test]
fn test_mouse_tab_clicks_grace_banner_buttons_and_no_mouse() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.last_viewport_width = 80;
    app.grace_state = Some(GraceState {
        session_id: "s-1".into(),
        remaining_sec: 45,
        total_sec: 60,
        planned_actions: vec!["Pause Media".into(), "Suspend".into()],
        dry_run: false,
    });

    // Click each tab using exact rendered hit ranges
    for (expected_tab, start_col, _) in tab_hit_ranges() {
        app.handle_event(TuiEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: start_col + 2,
            row: 0,
            modifiers: KeyModifiers::empty(),
        }));
        assert_eq!(app.active_tab, expected_tab);
    }

    // Click [c] Cancel and [s] Snooze on the Grace banner (row 1) at 80 cols
    let layout = GraceBannerLayout::compute(app.grace_state.as_ref().unwrap(), 80);
    let cancel_cmd = app.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: layout.cancel_cols.0 + 1,
        row: 1,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(cancel_cmd, Some(TuiCommand::CancelGrace));

    let snooze_cmd = app.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: layout.snooze_cols.0 + 1,
        row: 1,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(
        snooze_cmd,
        Some(TuiCommand::SnoozeGrace(None)),
        "Clicking [s] Snooze on the 80-col banner must dispatch SnoozeGrace"
    );

    // --no-mouse suppresses all mouse events
    let mut app_no_mouse = TuiApp::new(true, false, false);
    app_no_mouse.daemon_connected = true;
    app_no_mouse.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: tab_hit_ranges()[2].1 + 2,
        row: 0,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(app_no_mouse.active_tab, Tab::Dashboard);
}

#[test]
fn test_responsive_resizing_limits_and_secondary_card_collapse() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        None,
    )];

    // Below 60x18: Resize notice
    let mut terminal_small = Terminal::new(TestBackend::new(55, 15)).unwrap();
    terminal_small
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_small = buffer_to_string(terminal_small.backend().buffer());
    assert!(out_small.contains("Terminal too small"));
    assert!(out_small.contains("55×15"));
    assert!(out_small.contains("60×18"));

    // Below 80 cols (e.g. 70x20): Secondary "Wearable & System" card collapses cleanly
    let mut terminal_compact = Terminal::new(TestBackend::new(70, 20)).unwrap();
    terminal_compact
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_compact = buffer_to_string(terminal_compact.backend().buffer());
    assert!(out_compact.contains("Live Heart Rate"));
    assert!(out_compact.contains("Sleep State"));
    assert!(!out_compact.contains("Wearable & System"));
}

#[test]
fn test_no_color_and_ascii_modes_with_live_sparkline() {
    // 1. Theme::from_env reads NO_COLOR from injected EnvSource
    let env_no_color = MapEnv::new().with("NO_COLOR", "1");
    let theme = Theme::from_env(&env_no_color, true);
    assert!(theme.no_color);
    assert!(theme.ascii);

    // 2. Test --ascii mode WITH an enrolled wearable + live HR + baselines so Sparkline renders!
    let mut app_ascii = TuiApp::new(false, true, true);
    app_ascii.daemon_connected = true;
    app_ascii.latest_bpm = Some(62.0);
    app_ascii.latest_p_sleep = Some(0.88);
    app_ascii.sparkline_data.quiet_wake_baseline = Some(72.0);
    app_ascii.sparkline_data.sleeping_hr = Some(58.0);
    app_ascii.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        Some("Chest"),
    )];
    for bpm in [74.0, 72.0, 69.0, 66.0, 63.0, 60.0, 58.0] {
        app_ascii.sparkline_data.push_sample(HrSampleEvent {
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "w-1".into(),
            session_id: None,
            ts_ms: 1_700_000_000_000,
        });
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app_ascii.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    assert!(out.contains("* HR (live)"));
    assert!(out.contains("- QW (72)"));
    assert!(out.contains("- Sleep (58)"));
    assert!(
        out.is_ascii(),
        "--ascii mode output must be strictly 7-bit ASCII across the entire screen, got:\n{out}"
    );

    // Verify NO_COLOR keeps every cell at Color::Reset across Dashboard + Grace Banner + Offline
    let buf = terminal.backend().buffer();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            assert_eq!(cell.fg, Color::Reset);
            assert_eq!(cell.bg, Color::Reset);
        }
    }
}

#[test]
fn test_hotkey_suspension_in_text_inputs() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;

    // 1. Rename modal
    app.modal = ModalState::RenameModal {
        wearable_id: "w-01".into(),
        current_alias: "Old".into(),
        input: "My ".into(),
    };
    assert!(app.is_input_focused());

    for c in ['1', '2', 'q', '?', 'h', 'l', 'd', 'p'] {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.active_tab, Tab::Wearables);
    assert!(!app.should_quit);
    assert!(!app.is_diagnostics_open);

    app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::WearableRename {
            id: "w-01".into(),
            alias: Some("My 12q?hld".into()),
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 2. Agent1 PinCode modal
    app.modal = ModalState::EnrollmentModal {
        scanning_remaining_ms: 20_000,
        selected_discovered_idx: 0,
        alias_input: String::new(),
        alias_focused: false,
        enrolling: true,
        error_message: None,
        pairing_prompt: Some(PairingPromptModalState::PinCode {
            wearable: "Polar H10".into(),
            input: String::new(),
        }),
    };
    assert!(app.is_input_focused());
    for c in ['0', '4', '1', 'q'] {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    assert!(!app.should_quit);
    assert_eq!(app.active_tab, Tab::Wearables);
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    if let Some(TuiCommand::EnrollAuthReply(params)) = cmd {
        assert_eq!(params.confirmed, Some(true));
        assert_eq!(params.pincode.as_deref(), Some("041q"));
    } else {
        panic!("Expected EnrollAuthReply");
    }
    // EnrollmentModal remains open (enrolling: true) until OpCompleted arrives!
    assert!(matches!(
        app.modal,
        ModalState::EnrollmentModal {
            enrolling: true,
            pairing_prompt: None,
            ..
        }
    ));
}

#[test]
fn test_dashboard_diagnostics_units_and_pause_popover() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.idle_backend = Some("mutter-idle-monitor".into());
    app.playing_players_count = Some(2);
    app.latest_epoch = Some(Epoch {
        session_index: 1,
        ts_ms: 1_700_000_000_000,
        features: EpochFeatures {
            hr_bpm_60s: 58.0,
            z_hr: -1.4,
            hr_slope_4m: -0.45,
            r_var_120s: 0.38,
            ln_rmssd_60s: Some(3.21),
            delta_ln_rmssd_60s: Some(-0.2),
            rmssd_sdnn_120s: Some(0.78),
        },
        hrv_valid: true,
        rr_artifact_frac: 0.04,
        coverage: 1.0,
        flatline_stale: false,
        accumulators_frozen: false,
        baseline_mu_hr: 66.0,
        baseline_sigma_hr: 3.0,
        baseline_frozen: true,
        p_active_wake: 0.05,
        p_quiet_wake: 0.15,
        p_sleep: 0.80,
        state: DetectorState::Candidate,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // Toggling diagnostics when connected issues RefreshStatus so player count updates
    let diag_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('d'))));
    assert_eq!(diag_cmd, Some(TuiCommand::RefreshStatus));
    assert!(app.is_diagnostics_open);

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Engine Diagnostics [d]"));
    assert!(
        out.contains("Slope:-0.45 bpm/m"),
        "hr_slope_4m must NOT be multiplied by 60"
    );
    assert!(out.contains("Var:0.38"));
    assert!(!out.contains("bpm²"), "R_var is dimensionless ratio");
    assert!(out.contains("ln(RMSSD):3.21"));
    assert!(out.contains("RMSSD/SDNN:0.78"));
    assert!(out.contains("SQI:0.96"));
    assert!(out.contains("mutter-idle-monitor"));
    assert!(out.contains("Playing MPRIS2 Players: 2"));

    // Pause popover
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('p'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::Pause(PauseParams {
            mode: Some(PauseMode::Duration),
            duration_sec: Some(3600),
        }))
    );
}

#[test]
fn test_alert_strip_canonical_id_truncation_and_non_dismissible_guard() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;

    // Canonical persistent shift ID with realistic long message from daemon
    app.alerts.push(DaemonAlert {
        id: alert_ids::PERSISTENT_SHIFT.into(),
        kind: alert_ids::PERSISTENT_SHIFT.into(),
        severity: AlertSeverity::Warn,
        message: "Persistent downward baseline shift detected across 5 consecutive sessions; accept or dismiss via calibration controls".into(),
        dismissible: true,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("[a] Accept"));
    assert!(out.contains("[i] Ignore"));
    assert!(out.contains("[x] Dismiss"));

    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('a')))),
        Some(TuiCommand::AcceptShift)
    );
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('i')))),
        Some(TuiCommand::DismissShift)
    );
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('x')))),
        Some(TuiCommand::DismissAlert {
            id: alert_ids::PERSISTENT_SHIFT.into(),
        })
    );
    assert!(app.alerts.is_empty());

    // Non-dismissible alert must NOT be removed by pressing 'x'
    app.alerts.push(DaemonAlert {
        id: alert_ids::DEGRADED_IDLE.into(),
        kind: alert_ids::DEGRADED_IDLE.into(),
        severity: AlertSeverity::Warn,
        message: "No desktop idle backend available.".into(),
        dismissible: false,
    });
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('x')))),
        None
    );
    assert_eq!(app.alerts.len(), 1);
}

#[test]
fn test_global_grace_period_banner_across_tabs() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.grace_state = Some(GraceState {
        session_id: "sess-test".into(),
        remaining_sec: 23,
        total_sec: 30,
        planned_actions: vec!["Pause Media".into(), "Lock Screen".into(), "Suspend".into()],
        dry_run: false,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    for tab in [Tab::Dashboard, Tab::History, Tab::Wearables, Tab::Settings] {
        app.active_tab = tab;
        terminal
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let out = buffer_to_string(terminal.backend().buffer());
        assert!(out.contains("GRACE: 23s"));
        assert!(out.contains("[c] Cancel"));
        assert!(out.contains("[s] Snooze"));
    }

    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('s')))),
        Some(TuiCommand::SnoozeGrace(None))
    );
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('c')))),
        Some(TuiCommand::CancelGrace)
    );
}

#[test]
fn test_wearables_tab_states_countdown_and_offline_gating() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    let mut w1 = make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Chest Strap"),
        StreamStatus::SignalLost,
        Some(85),
        Some("Chest"),
    );
    w1.wearable.advertised_name = Some("Polar H10".to_string());
    let mut w2 = make_wearable(
        "w-2",
        "AA:BB:CC:44:55:66",
        Some("Wrist Band"),
        StreamStatus::PassiveStandby,
        None,
        Some("Wrist"),
    );
    w2.wearable.advertised_name = Some("GT 5".to_string());
    w2.wearable.supports_rr = false;
    app.enrolled_wearables = vec![w1, w2];

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    assert!(out.contains("Signal Lost"));
    assert!(out.contains("Passive Standby"));
    assert!(out.contains("BPM+RR (Chest)"));
    assert!(out.contains("BPM-only (Wrist)"));

    // Unenroll modal with UnenrollButton::Confirm / Cancel
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('u'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert!(matches!(
        app.modal,
        ModalState::UnenrollModal {
            forget_bluez: true,
            delete_sessions: true,
            selected_button: UnenrollButton::Confirm,
            ..
        }
    ));
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::WearableUnenroll {
            id: "w-1".into(),
            forget_bluez: true,
            delete_sessions: true,
        })
    );

    // 30s scan countdown uses TUI_TICK_MS (250ms): 4 ticks -> 29s
    app.open_enrollment_modal();
    for _ in 0..4 {
        app.handle_event(TuiEvent::Tick);
    }
    if let ModalState::EnrollmentModal {
        scanning_remaining_ms,
        ..
    } = app.modal
    {
        assert_eq!(scanning_remaining_ms, 29_000);
    }
    app.modal = ModalState::None;

    // Offline disables ALL live controls across Dashboard and Wearables!
    app.daemon_connected = false;
    app.grace_state = None;
    for code in [
        KeyCode::Char('e'),
        KeyCode::Char('n'),
        KeyCode::Char('c'),
        KeyCode::Char('u'),
    ] {
        assert_eq!(
            app.handle_event(TuiEvent::Key(key(code))),
            None,
            "Wearables hotkey {:?} must be disabled offline",
            code
        );
    }
    app.active_tab = Tab::Dashboard;
    app.alerts.push(DaemonAlert {
        id: alert_ids::PERSISTENT_SHIFT.into(),
        kind: alert_ids::PERSISTENT_SHIFT.into(),
        severity: AlertSeverity::Warn,
        message: "Shift".into(),
        dismissible: true,
    });
    for code in [
        KeyCode::Char('p'),
        KeyCode::Char('a'),
        KeyCode::Char('i'),
        KeyCode::Char('x'),
    ] {
        assert_eq!(
            app.handle_event(TuiEvent::Key(key(code))),
            None,
            "Dashboard hotkey {:?} must be disabled offline",
            code
        );
    }
}

#[test]
fn test_zero_wearables_cta_and_systemd_unit_detection() {
    let temp = TempDir::new().unwrap();
    let cfg_dir = temp.path().join("cfg");
    let env = MapEnv::new().with("XDG_CONFIG_HOME", cfg_dir.display().to_string());
    assert!(!detect_systemd_service_installed(&env));

    let unit_path = cfg_dir.join("systemd/user/bpmute.service");
    fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
    fs::write(&unit_path, "[Unit]\nDescription=BPMute\n").unwrap();
    assert!(detect_systemd_service_installed(&env));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // When unit is NOT installed, hint shows without nonexistent `bpmute service install`
    let mut app_uninstalled = TuiApp::new(false, false, false);
    app_uninstalled.daemon_connected = true;
    app_uninstalled.systemd_service_installed = false;
    terminal
        .draw(|f| app_uninstalled.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("No BLE Heart Rate wearables enrolled yet."));
    assert!(out.contains("systemd --user unit not found"));
    assert!(!out.contains("bpmute service install"));

    // When unit IS installed, hint is hidden
    let mut app_installed = TuiApp::new(false, false, false);
    app_installed.daemon_connected = true;
    app_installed.systemd_service_installed = true;
    terminal
        .draw(|f| app_installed.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out2 = buffer_to_string(terminal.backend().buffer());
    assert!(!out2.contains("systemd --user unit not found"));
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_grace_cancel_snooze_calibration_mpris_and_inhibited_alert_e2e() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);

    let executor = Arc::new(FakeActionExecutor::new());
    executor.set_playing_players(&[
        "org.mpris.MediaPlayer2.mpv",
        "org.mpris.MediaPlayer2.spotify",
    ]);
    executor.set_power_action_override(Ok(PowerActionResult::RefusedInhibited(
        "systemd-inhibit: download".to_string(),
    )));

    let cli_overrides = CliOverrides::default();
    let mut loaded = LoadedConfig::load(&paths.config_file, &env, &cli_overrides).unwrap();
    loaded.config.actions = ActionsConfig {
        pause_media: true,
        lock_screen: true,
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(15),
        ..ActionsConfig::default()
    };

    let clock = Arc::new(VirtualClock::starting_at(Utc::now()));
    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_action_executor(Arc::clone(&executor) as Arc<dyn ActionExecutor>),
    )
    .await
    .unwrap();

    // Seed calibration so initial calibration.status fetch sets sleeping_hr
    let mut setup_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    setup_client
        .request("calibration.set_seed", json!({ "sleep_hr": 56.0 }))
        .await
        .unwrap();

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.daemon_connected && a.sparkline_data.sleeping_hr.is_some()
    })
    .await;
    assert_eq!(app.playing_players_count, Some(2));

    // 1. Test Snooze over real Unix socket
    setup_client
        .request("monitor.test_trigger", json!({ "grace_sec": 15 }))
        .await
        .unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_some()).await;
    assert_eq!(
        app.grace_state.as_ref().unwrap().planned_actions,
        vec![
            "Pause Media".to_string(),
            "Lock Screen".to_string(),
            "Suspend".to_string()
        ]
    );

    let snooze_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('s'))));
    assert_eq!(snooze_cmd, Some(TuiCommand::SnoozeGrace(None)));
    cmd_tx.send(snooze_cmd.unwrap()).await.unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_none()).await;

    // 2. Test Cancel over real Unix socket
    setup_client
        .request("monitor.test_trigger", json!({ "grace_sec": 15 }))
        .await
        .unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_some()).await;
    let cancel_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('c'))));
    assert_eq!(cancel_cmd, Some(TuiCommand::CancelGrace));
    cmd_tx.send(cancel_cmd.unwrap()).await.unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_none()).await;

    // 3. Test RefusedInhibited pipeline execution surfaces LOGIND_INHIBITED alert over IPC
    daemon
        .core()
        .handle_confirmed_sleep_onset(1_700_000_000_000)
        .await;
    tokio::time::advance(Duration::from_secs(18)).await;
    tokio::task::yield_now().await;

    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.alerts
            .iter()
            .any(|al| al.id == alert_ids::LOGIND_INHIBITED)
    })
    .await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon.shutdown().await;
}

struct TestAgent1PairingHook;

impl EnrollmentPairingHook for TestAgent1PairingHook {
    fn bond_and_pair<'a>(
        &'a self,
        core: &'a Arc<DaemonCore>,
        _mac: &'a str,
        _pair: bool,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            core.bridge_agent_auth_request(AgentAuthRequest::Confirmation {
                device: "/org/bluez/hci0/dev_AA_BB_CC_11_22_33".to_string(),
                passkey: 482_910,
                reply: reply_tx,
            })
            .await;
            match reply_rx.await {
                Ok(true) => Ok(()),
                _ => Err("pairing rejected".to_string()),
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_agent1_pairing_prompt_during_enroll_select_e2e() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);

    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(Utc::now()));
    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_enrollment_driver(Arc::new(TestAgent1PairingHook)),
    )
    .await
    .unwrap();

    daemon
        .core()
        .seed_discovered_wearables(vec![DiscoveredWearable {
            address: "AA:BB:CC:11:22:33".into(),
            address_type: "public".into(),
            name: Some("Polar H10".into()),
            rssi: Some(-52),
            paired: false,
            already_enrolled: false,
            uuids: vec![],
        }]);

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    wait_for_app_state(&mut app, &mut event_rx, |a| a.daemon_connected).await;

    // Switch to Wearables tab, press 'e' to start scan
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('3'))));
    let start_cmd = app
        .handle_event(TuiEvent::Key(key(KeyCode::Char('e'))))
        .unwrap();
    cmd_tx.send(start_cmd).await.unwrap();

    wait_for_app_state(&mut app, &mut event_rx, |a| {
        !a.discovered_wearables.is_empty()
    })
    .await;

    // Press Enter -> sends EnrollSelect while keeping modal open in enrolling state
    let select_cmd = app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .unwrap();
    assert!(matches!(
        app.modal,
        ModalState::EnrollmentModal {
            enrolling: true,
            ..
        }
    ));
    cmd_tx.send(select_cmd).await.unwrap();

    // Daemon's in-flight EnrollSelect pushes Agent1 Confirmation prompt over IPC!
    wait_for_app_state(&mut app, &mut event_rx, |a| {
        matches!(
            a.modal,
            ModalState::EnrollmentModal {
                pairing_prompt: Some(PairingPromptModalState::Confirmation {
                    passkey: 482_910,
                    ..
                }),
                ..
            }
        )
    })
    .await;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Wearable: Polar H10"));
    assert!(out.contains("482910"));

    // Confirm with 'y' -> sends wearable.enroll_auth_response on the SAME socket without deadlock
    let reply_cmd = app
        .handle_event(TuiEvent::Key(key(KeyCode::Char('y'))))
        .unwrap();
    cmd_tx.send(reply_cmd).await.unwrap();

    // Expect EnrollSelect completion + refresh_wearables
    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.modal == ModalState::None && a.enrolled_wearables.len() == 1
    })
    .await;

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_daemon_offline_grace_reset_and_restart_reconnect_e2e() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (_cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    assert!(!app.daemon_connected);

    // 1. Spawn daemon #1 while TUI is already running offline
    let loaded1 = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let clock1 = Arc::new(VirtualClock::starting_at(Utc::now()));
    let daemon1 = DaemonHandle::spawn(DaemonOptions::for_testing(
        paths.clone(),
        loaded1,
        Arc::new(env.clone()),
        clock1,
    ))
    .await
    .unwrap();

    tokio::time::advance(Duration::from_millis(1100)).await;
    tokio::task::yield_now().await;
    wait_for_app_state(&mut app, &mut event_rx, |a| a.daemon_connected).await;

    // 2. Trigger Grace Period, then shut down daemon #1 mid-grace -> banner switches to Offline
    let mut client1 = IpcClient::connect(&paths.socket_path).await.unwrap();
    client1
        .request("monitor.test_trigger", json!({ "grace_sec": 20 }))
        .await
        .unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_some()).await;
    drop(client1);

    daemon1.shutdown().await;
    wait_for_app_state(&mut app, &mut event_rx, |a| !a.daemon_connected).await;
    assert!(
        app.grace_state.is_none(),
        "Disconnect must clear stale grace_state so offline banner is not masked"
    );

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Daemon offline — retrying…"));

    // 3. Restart daemon #2 on the SAME socket path -> bridge must auto-reconnect seamlessly!
    let loaded2 = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let clock2 = Arc::new(VirtualClock::starting_at(Utc::now()));
    let daemon2 = DaemonHandle::spawn(DaemonOptions::for_testing(
        paths.clone(),
        loaded2,
        Arc::new(env),
        clock2,
    ))
    .await
    .unwrap();

    tokio::time::advance(Duration::from_millis(1500)).await;
    tokio::task::yield_now().await;
    wait_for_app_state(&mut app, &mut event_rx, |a| a.daemon_connected).await;

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(!buffer_to_string(terminal.backend().buffer()).contains("Daemon offline"));

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon2.shutdown().await;
}
