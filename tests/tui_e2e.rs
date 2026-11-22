mod common;
use common::tui::*;

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clap::Parser;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::Color;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use bpmute::actions::fake::FakeActionExecutor;
use bpmute::actions::traits::{ActionExecutor, GracePhase, PowerActionResult};
use bpmute::calibration::{BoundaryReason, CalibrationBoundaryRecord, FeedbackVerdict};
use bpmute::cli::args::Cli;
use bpmute::cli::execute_cli;
use bpmute::config::{
    ActionsConfig, CliOverrides, LoadedConfig, PowerAction, ScheduleConfig, Sensitivity,
};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
use bpmute::exit_code::ExitCode;
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::{
    AlertSeverity, DaemonAlert, DaemonStatus, DiscoveredWearable, HrSampleEvent, MonitoringState,
    PauseMode, PauseParams, alert_ids,
};
use bpmute::paths::MapEnv;
use bpmute::sleep_engine::{
    CalibrationMaturity, ContactStatus, DetectorState, Epoch, EpochFeatures, SessionEndReason,
    SessionSummary, StreamStatus,
};
use bpmute::storage::{SessionListEntry, SessionStatus};
use bpmute::tui::{
    GraceState, IpcEvent, ModalState, PairingPromptModalState, Tab, Theme, TuiApp, TuiCommand,
    TuiEvent, TuiIpcBridge, UnenrollButton,
};

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
    app.grace_state = Some(GraceState {
        session_id: "s-1".into(),
        remaining_sec: 45,
        total_sec: 60,
        planned_actions: vec!["Pause Media".into(), "Suspend".into()],
        dry_run: false,
        phase: GracePhase::Countdown,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // Click each tab using visually rendered coordinates from TestBackend's buffer
    for (expected_tab, label) in [
        (Tab::Dashboard, "[1:Dashboard]"),
        (Tab::History, "[2:History]"),
        (Tab::Wearables, "[3:Wearables]"),
        (Tab::Settings, "[4:Settings]"),
    ] {
        let (col, row) = find_text_coords(terminal.backend().buffer(), label)
            .unwrap_or_else(|| panic!("rendered buffer missing tab {label}"));
        app.handle_event(mouse_left_click(col + 1, row));
        assert_eq!(app.active_tab, expected_tab);
    }

    // Rendering below 60x18 clears the interactive HitMap so stale tab clicks cannot fire
    let (settings_col, settings_row) =
        find_text_coords(terminal.backend().buffer(), "[4:Settings]").unwrap();
    let (dash_col, dash_row) =
        find_text_coords(terminal.backend().buffer(), "[1:Dashboard]").unwrap();
    let mut terminal_small = Terminal::new(TestBackend::new(55, 15)).unwrap();
    terminal_small
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    app.handle_event(mouse_left_click(dash_col + 1, dash_row));
    assert_eq!(
        app.active_tab,
        Tab::Settings,
        "Clicks while below 60x18 resize notice must not hit stale background tabs"
    );

    // Resizing back to >= 60x18 and re-rendering restores accurate tab hit targets
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    app.handle_event(mouse_left_click(dash_col + 1, dash_row));
    assert_eq!(app.active_tab, Tab::Dashboard);
    app.handle_event(mouse_left_click(settings_col + 1, settings_row));
    assert_eq!(app.active_tab, Tab::Settings);

    // Click [c] Cancel and [s] Snooze on the Grace banner (row 1) at 80 cols (< 96)
    let buf_80 = terminal.backend().buffer();
    let (cancel_col, cancel_row) = find_text_coords(buf_80, "[c] Cancel").unwrap();
    let (snooze_col, snooze_row) = find_text_coords(buf_80, "[s] Snooze").unwrap();
    let cancel_cmd = app.handle_event(mouse_left_click(cancel_col + 1, cancel_row));
    assert_eq!(cancel_cmd, Some(TuiCommand::CancelGrace));

    let snooze_cmd = app.handle_event(mouse_left_click(snooze_col + 1, snooze_row));
    assert_eq!(
        snooze_cmd,
        Some(TuiCommand::SnoozeGrace(None)),
        "Clicking [s] Snooze on the 80-col banner must dispatch SnoozeGrace"
    );

    // Click [c] Cancel (I'm Awake) and [s] Snooze on the Grace banner at 104 cols (>= 96)
    let mut terminal_wide = Terminal::new(TestBackend::new(104, 24)).unwrap();
    terminal_wide
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf_wide = terminal_wide.backend().buffer();
    let (wide_cancel_col, wide_cancel_row) =
        find_text_coords(buf_wide, "[c] Cancel (I'm Awake)").unwrap();
    let (wide_snooze_col, wide_snooze_row) = find_text_coords(buf_wide, "[s] Snooze").unwrap();
    // Click near the end of "(I'm Awake)" to verify the full rendered label is hit-tested
    assert_eq!(
        app.handle_event(mouse_left_click(wide_cancel_col + 18, wide_cancel_row)),
        Some(TuiCommand::CancelGrace)
    );
    assert_eq!(
        app.handle_event(mouse_left_click(wide_snooze_col + 2, wide_snooze_row)),
        Some(TuiCommand::SnoozeGrace(None))
    );

    // Disconnected offline banner must not expose Grace Period controls
    app.daemon_connected = false;
    terminal_wide
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(wide_cancel_col + 1, wide_cancel_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(wide_snooze_col + 1, wide_snooze_row)),
        None
    );
    app.daemon_connected = true;

    let sample_wearables = vec![
        make_wearable(
            "w-1",
            "AA:BB:CC:11:22:01",
            Some("Strap 1"),
            StreamStatus::Streaming,
            Some(90),
            None,
        ),
        make_wearable(
            "w-2",
            "AA:BB:CC:11:22:02",
            Some("Strap 2"),
            StreamStatus::Disconnected,
            Some(70),
            None,
        ),
    ];

    // --no-mouse suppresses all mouse events (clicks and wheel)
    let mut app_no_mouse = TuiApp::new(true, false, false);
    app_no_mouse.daemon_connected = true;
    app_no_mouse.grace_state = app.grace_state.clone();
    app_no_mouse.enrolled_wearables = sample_wearables.clone();
    let mut terminal_no_mouse = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal_no_mouse
        .draw(|f| app_no_mouse.render(f.area(), f.buffer_mut()))
        .unwrap();
    let nm_buf = terminal_no_mouse.backend().buffer();
    let (wear_col, wear_row) = find_text_coords(nm_buf, "[3:Wearables]").unwrap();
    let (nm_cancel_col, nm_cancel_row) = find_text_coords(nm_buf, "[c] Cancel").unwrap();
    app_no_mouse.handle_event(mouse_left_click(wear_col + 1, wear_row));
    assert_eq!(app_no_mouse.active_tab, Tab::Dashboard);
    assert_eq!(
        app_no_mouse.handle_event(mouse_left_click(nm_cancel_col + 1, nm_cancel_row)),
        None
    );
    app_no_mouse.active_tab = Tab::Wearables;
    terminal_no_mouse
        .draw(|f| app_no_mouse.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (table_col, table_row) =
        find_text_coords(terminal_no_mouse.backend().buffer(), "Strap 1").unwrap();
    app_no_mouse.handle_event(mouse_scroll_down(table_col, table_row));
    assert_eq!(app_no_mouse.selected_wearable_idx, 0);

    // Wearables scroll containment + modal shadowing across clicks and wheel events
    app.enrolled_wearables = sample_wearables;
    let (wear_tab_col, wear_tab_row) =
        find_text_coords(terminal.backend().buffer(), "[3:Wearables]").unwrap();
    app.handle_event(mouse_left_click(wear_tab_col + 1, wear_tab_row));
    assert_eq!(app.active_tab, Tab::Wearables);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (w_table_col, w_table_row) =
        find_text_coords(terminal.backend().buffer(), "Strap 1").unwrap();
    // Scrolling outside the table (on the header row) must not scroll Wearables
    app.handle_event(mouse_scroll_down(wear_tab_col, wear_tab_row));
    assert_eq!(
        app.selected_wearable_idx, 0,
        "Wheel events outside the Wearables table must not move selection"
    );

    // Scrolling inside the rendered table moves selection
    app.handle_event(mouse_scroll_down(w_table_col, w_table_row));
    assert_eq!(app.selected_wearable_idx, 1);
    app.handle_event(mouse_scroll_up(w_table_col, w_table_row));
    assert_eq!(app.selected_wearable_idx, 0);

    // Active modal (Help) shadows all background header, banner, and scroll targets
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.modal, ModalState::Help);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(dash_col + 1, dash_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Wearables);
    assert_eq!(
        app.handle_event(mouse_left_click(cancel_col + 1, cancel_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(snooze_col + 1, snooze_row)),
        None
    );
    app.handle_event(mouse_scroll_down(w_table_col, w_table_row));
    assert_eq!(
        app.selected_wearable_idx, 0,
        "Active Help modal must shadow background Wearables wheel scroll"
    );

    // Close Help modal, redraw, and verify background targets recover
    app.handle_event(TuiEvent::Key(key(KeyCode::Esc)));
    assert_eq!(app.modal, ModalState::None);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // Active in-tab modal (EnrollmentModal) also shadows background tabs, banner, and scroll
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('e'))));
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(dash_col + 1, dash_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Wearables);
    assert_eq!(
        app.handle_event(mouse_left_click(cancel_col + 1, cancel_row)),
        None
    );
    app.handle_event(mouse_scroll_down(w_table_col, w_table_row));
    assert_eq!(
        app.selected_wearable_idx, 0,
        "Active EnrollmentModal must shadow background Wearables wheel scroll"
    );
    app.handle_event(TuiEvent::Key(key(KeyCode::Esc)));
    assert_eq!(app.modal, ModalState::None);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(cancel_col + 1, cancel_row)),
        Some(TuiCommand::CancelGrace)
    );

    // Resize below 60x18 clears Wearables scroll hit region as well
    terminal_small
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    app.handle_event(mouse_scroll_down(w_table_col, w_table_row));
    assert_eq!(
        app.selected_wearable_idx, 0,
        "Wheel events below 60x18 resize notice must not scroll stale Wearables table"
    );
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
        phase: GracePhase::Countdown,
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
        .request("monitor.test_trigger", json!({ "grace_sec": 300 }))
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
    while daemon.core().is_in_grace_period() {
        tokio::task::yield_now().await;
    }

    // 2. Test Cancel over real Unix socket
    setup_client
        .request("monitor.test_trigger", json!({ "grace_sec": 300 }))
        .await
        .unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_some()).await;
    let cancel_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('c'))));
    assert_eq!(cancel_cmd, Some(TuiCommand::CancelGrace));
    cmd_tx.send(cancel_cmd.unwrap()).await.unwrap();
    wait_for_app_state(&mut app, &mut event_rx, |a| a.grace_state.is_none()).await;
    while daemon.core().is_in_grace_period() {
        tokio::task::yield_now().await;
    }

    // 3. Test RefusedInhibited pipeline execution surfaces LOGIND_INHIBITED alert over IPC
    daemon
        .core()
        .handle_confirmed_sleep_onset(1_700_000_000_000)
        .await;
    for _ in 0..25 {
        if daemon
            .core()
            .status()
            .alerts
            .iter()
            .any(|al| al.id == alert_ids::LOGIND_INHIBITED)
        {
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }

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

#[test]
fn test_offline_read_only_browsing_and_reconnect() {
    let temp = tempfile::tempdir().unwrap();
    let runtime_dir = temp.path().join("run");
    let state_dir = temp.path().join("state");
    let config_dir = temp.path().join("config");
    let cache_dir = temp.path().join("cache");
    let data_dir = temp.path().join("data");

    std::fs::create_dir_all(&runtime_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let paths = bpmute::paths::ResolvedPaths {
        config_file: config_dir.join("config.toml"),
        data_dir: data_dir.clone(),
        socket_path: temp.path().join("run/bpmute.sock"),
    };

    // 1. Write config.toml with responsive sensitivity and custom command
    let toml_content = r#"
schema_version = 1

[detection]
sensitivity = "responsive"

[actions]
pause_media = false
lock_screen = true
power_action = "none"
grace_period_sec = 60
custom_command = ["notify-send", "sleepy"]
"#;
    std::fs::write(&paths.config_file, toml_content).unwrap();

    // 2. Populate SQLite ledger with 2 sessions, epochs, boundary, and storage stats
    let (s1_id, _s2_id) = {
        let db_file = bpmute::storage::db_path_in_data_dir(&paths.data_dir);
        let db_lock = bpmute::storage::lock_path_in_data_dir(&paths.data_dir);
        let mut store = bpmute::storage::LedgerStore::open_read_write(&db_file, &db_lock).unwrap();

        let desc = bpmute::storage::WearableDescriptor {
            mac: "AA:BB:CC:DD:EE:01".into(),
            address_type: "public".into(),
            alias: Some("OfflineBand".into()),
            advertised_name: Some("OfflineBand".into()),
            supports_rr: true,
            supports_contact: true,
        };
        let wid = store.upsert_wearable(&desc, 1_700_000_000_000).unwrap();

        // Insert a calibration boundary
        store
            .set_calibration_boundary(
                Some(1_700_000_000_000),
                Some(52.0),
                false,
                1_700_000_000_000,
            )
            .unwrap();

        // Insert session 1 (closed)
        let s1_id = store
            .open_session(&bpmute::storage::OpenSessionRequest {
                wearable_id: wid.clone(),
                start_ts_ms: 1_700_000_100_000,
                detection_sensitivity: bpmute::config::Sensitivity::Balanced,
            })
            .unwrap();
        store
            .close_session_and_recompute(
                &bpmute::storage::CloseSessionRequest {
                    session_id: s1_id.clone(),
                    summary: SessionSummary {
                        session_index: 1,
                        start_ts_ms: 1_700_000_100_000,
                        end_ts_ms: 1_700_000_500_000,
                        end_reason: SessionEndReason::StreamEnded,
                        candidate_ts_ms: None,
                        confirmed_ts_ms: None,
                        sleeping_ts_ms: None,
                        anomalous: false,
                        fp_count: 0,
                        warmup_mu_hr: Some(64.0),
                        warmup_sigma_hr: Some(2.5),
                        warmup_ln_rmssd: None,
                        onset_hr_bpm: Some(54.0),
                        sleep_mu_hr: Some(54.0),
                        sleep_ln_rmssd: None,
                        observed_hr_var_ratio: None,
                        typical_sqi: None,
                        post_trigger_sleep_sec: 0,
                        had_rr: false,
                        grace_cancel_count: 0,
                        rapid_wake_fp_count: 0,
                        snooze_count: 0,
                        epochs_count: 20,
                    },
                    trigger_power_action: bpmute::config::PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                1_700_000_500_000,
            )
            .unwrap();

        // Insert session 2 (closed)
        let s2_id = store
            .open_session(&bpmute::storage::OpenSessionRequest {
                wearable_id: wid,
                start_ts_ms: 1_700_010_100_000,
                detection_sensitivity: bpmute::config::Sensitivity::Balanced,
            })
            .unwrap();
        store
            .close_session_and_recompute(
                &bpmute::storage::CloseSessionRequest {
                    session_id: s2_id.clone(),
                    summary: SessionSummary {
                        session_index: 2,
                        start_ts_ms: 1_700_010_100_000,
                        end_ts_ms: 1_700_010_600_000,
                        end_reason: SessionEndReason::StreamEnded,
                        candidate_ts_ms: None,
                        confirmed_ts_ms: None,
                        sleeping_ts_ms: None,
                        anomalous: false,
                        fp_count: 0,
                        warmup_mu_hr: Some(65.0),
                        warmup_sigma_hr: Some(2.5),
                        warmup_ln_rmssd: None,
                        onset_hr_bpm: Some(55.0),
                        sleep_mu_hr: Some(55.0),
                        sleep_ln_rmssd: None,
                        observed_hr_var_ratio: None,
                        typical_sqi: None,
                        post_trigger_sleep_sec: 0,
                        had_rr: false,
                        grace_cancel_count: 0,
                        rapid_wake_fp_count: 0,
                        snooze_count: 0,
                        epochs_count: 25,
                    },
                    trigger_power_action: bpmute::config::PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                1_700_010_600_000,
            )
            .unwrap();
        (s1_id, s2_id)
    };

    // 3. Initialize TUI app offline
    let mut app = TuiApp::new(false, false, false);
    assert!(!app.daemon_connected);
    app.set_offline_paths(paths.clone());

    let env = bpmute::paths::ProcessEnv;
    app.load_offline_data(&env);

    // Verify offline loaded state
    assert_eq!(app.history_sessions.len(), 2);
    assert_eq!(app.history_boundaries.len(), 1);
    assert!(app.active_config.is_some());
    assert!(app.storage_stats.is_some());

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // 4. Test History tab offline browsing
    app.active_tab = Tab::History;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_hist = buffer_to_string(terminal.backend().buffer());
    assert!(out_hist.contains("OfflineBand") || out_hist.contains("session-off-2"));
    assert!(out_hist.contains("Boundary"));

    // Navigate to session 1
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    assert_eq!(app.selected_session_idx, 1);
    assert_eq!(
        app.selected_session_detail
            .as_ref()
            .map(|d| d.session.id.as_str()),
        Some(s1_id.as_str())
    );

    // Mutating keys on History tab are completely disabled while offline
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('f')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('x')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('D')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    // 5. Test Settings tab offline browsing
    app.active_tab = Tab::Settings;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_settings = buffer_to_string(terminal.backend().buffer());
    assert!(out_settings.contains("Responsive"));
    assert!(out_settings.contains("mutations disabled: daemon offline"));
    assert!(out_settings.contains("2 sessions"));

    // Mutating keys on Settings tab are completely disabled while offline
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(' ')))),
        None
    );
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('B')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('P')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('W')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('U')))),
        None
    );

    // 6. Test Live Reconnect
    app.handle_event(TuiEvent::Ipc(Box::new(IpcEvent::Connected(DaemonStatus {
        v: 1,
        daemon_running: true,
        version: "0.1.0".into(),
        pid: 1,
        uptime_sec: 10,
        monitoring_state: MonitoringState::Idle,
        armed: true,
        paused: false,
        paused_until: None,
        pause_mode: None,
        dry_run: false,
        schedule: ScheduleConfig::default(),
        config_path: String::new(),
        config_loaded_from_file: false,
        socket_path: String::new(),
        log_biometrics: false,
        alerts: Vec::new(),
        idle_backend: Some("test-idle".into()),
        playing_players_count: Some(0),
    }))));
    assert!(app.daemon_connected);

    // After reconnect, mutating keys work immediately
    let toggle_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char(' '))));
    assert!(matches!(toggle_cmd, Some(TuiCommand::ConfigUpdate(_))));

    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('B')))),
        None
    );
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));
}

#[test]
fn test_mouse_footer_tab_actions_and_navigation_hint_ignores() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_config = Some(bpmute::config::Config::default());
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:01",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        None,
    )];
    app.grace_state = Some(GraceState {
        session_id: "s-1".into(),
        remaining_sec: 40,
        total_sec: 60,
        planned_actions: vec!["Pause Media".into()],
        dry_run: false,
        phase: GracePhase::Countdown,
    });
    app.alerts = vec![DaemonAlert {
        id: alert_ids::PERSISTENT_SHIFT.into(),
        kind: "calibration".into(),
        severity: AlertSeverity::Info,
        message: "Persistent baseline shift detected".into(),
        dismissible: true,
    }];
    app.history_sessions = vec![SessionListEntry {
        id: "hist-1".into(),
        wearable_id: "w-1".into(),
        wearable_alias: Some("Polar H10".into()),
        wearable_mac_masked: "..:22:01".into(),
        status: SessionStatus::Closed,
        start_ts_ms: 1_700_000_000_000,
        end_ts_ms: 1_700_003_600_000,
        end_reason: Some(SessionEndReason::StreamEnded),
        detection_sensitivity: Sensitivity::Balanced,
        had_rr: true,
        warmup_mu_hr: Some(72.0),
        sleep_mu_hr: Some(58.0),
        onset_confirmed_ts_ms: Some(1_700_001_200_000),
        sleeping_ts_ms: None,
        auto_anomalous: false,
        fp_count: 0,
        epochs_count: 10,
        raw_samples_count: 100,
        feedback: None,
    }];
    app.storage_stats = Some(bpmute::storage::StorageStats {
        db_size_bytes: 1024,
        wal_size_bytes: 0,
        db_size_formatted: "1.0 KB".into(),
        sessions_count: 1,
        session_epochs_count: 10,
        raw_samples_count: 100,
        session_feedback_count: 0,
        calibration_boundaries_count: 1,
        wearables_count: 1,
        session_epochs_retention: "90d".into(),
        raw_samples_retention: "7d".into(),
        calibration_maturity: CalibrationMaturity::Calibrated,
        active_calibration_boundary: Some(CalibrationBoundaryRecord {
            id: "bnd-1".into(),
            effective_ts_ms: 1_700_000_000_000,
            created_at_ms: 1_700_000_000_000,
            reason: BoundaryReason::ManualRebaseline,
            seed_sleep_hr: Some(55.0),
            reset_floors: false,
            reverted_at_ms: None,
        }),
    });

    // Use 140x24 so full footer lines fit without right-edge clipping
    let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();

    // 1. Dashboard footer
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    // Pure navigation hint [1-4] Tabs must ignore clicks
    let (tabs_col, tabs_row) = find_footer_coords(buf, "[1-4] Tabs").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(tabs_col + 1, tabs_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Dashboard);
    assert_eq!(app.modal, ModalState::None);

    // [d] Diagnostics toggles diagnostics pane and requests refresh when opening
    let (diag_col, diag_row) = find_footer_coords(buf, "[d] Diagnostics").unwrap();
    assert!(!app.is_diagnostics_open);
    assert_eq!(
        app.handle_event(mouse_left_click(diag_col + 1, diag_row)),
        Some(TuiCommand::RefreshStatus)
    );
    assert!(app.is_diagnostics_open);
    assert_eq!(
        app.handle_event(mouse_left_click(diag_col + 1, diag_row)),
        None
    );
    assert!(!app.is_diagnostics_open);

    // [c] Cancel, [s] Snooze, [a] Accept Shift, [i] Ignore Shift
    let (cancel_col, cancel_row) = find_footer_coords(buf, "[c] Cancel").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(cancel_col + 1, cancel_row)),
        Some(TuiCommand::CancelGrace)
    );
    let (snooze_col, snooze_row) = find_footer_coords(buf, "[s] Snooze").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(snooze_col + 1, snooze_row)),
        Some(TuiCommand::SnoozeGrace(None))
    );
    let (accept_col, accept_row) = find_footer_coords(buf, "[a] Accept Shift").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(accept_col + 1, accept_row)),
        Some(TuiCommand::AcceptShift)
    );
    let (ignore_col, ignore_row) = find_footer_coords(buf, "[i] Ignore Shift").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(ignore_col + 1, ignore_row)),
        Some(TuiCommand::DismissShift)
    );

    // [p] Pause opens PausePopover; clicking gap between [d] Diagnostics and [p] Pause does nothing
    let (pause_col, pause_row) = find_footer_coords(buf, "[p] Pause").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(pause_col - 1, pause_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(mouse_left_click(pause_col + 1, pause_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::PausePopover { .. }));
    app.modal = ModalState::None;

    // 2. Wearables footer
    app.active_tab = Tab::Wearables;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    let (conn_col, conn_row) = find_footer_coords(buf, "[c] Reconnect").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(conn_col + 1, conn_row)),
        Some(TuiCommand::WearableConnect { id: "w-1".into() })
    );
    let (rename_col, rename_row) = find_footer_coords(buf, "[n] Rename").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(rename_col + 1, rename_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::RenameModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (unenroll_col, unenroll_row) =
        find_footer_coords(terminal.backend().buffer(), "[u] Unenroll").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(unenroll_col + 1, unenroll_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::UnenrollModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (enroll_col, enroll_row) =
        find_footer_coords(terminal.backend().buffer(), "[e] Enroll").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(enroll_col + 1, enroll_row)),
        Some(TuiCommand::EnrollStart(Some(30)))
    );
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));
    app.modal = ModalState::None;

    // 3. History footer
    app.active_tab = Tab::History;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    let (jk_col, jk_row) = find_footer_coords(buf, "[j/k] Navigate").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(jk_col + 1, jk_row)), None);
    assert_eq!(app.modal, ModalState::None);

    let (fb_col, fb_row) = find_footer_coords(buf, "[f] Feedback").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(fb_col + 1, fb_row)), None);
    assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (exp_col, exp_row) = find_footer_coords(terminal.backend().buffer(), "[x] Export").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(exp_col + 1, exp_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::ExportTraceModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (del_col, del_row) = find_footer_coords(terminal.backend().buffer(), "[D] Delete").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(del_col + 1, del_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::DeleteSessionModal { .. }));
    app.modal = ModalState::None;

    // 4. Settings footer
    app.active_tab = Tab::Settings;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    let (s_jk_col, s_jk_row) = find_footer_coords(buf, "[j/k] Navigate").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(s_jk_col + 1, s_jk_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    // [Space] Toggle cycles currently selected preference (index 0 = Sensitivity: Balanced -> Responsive)
    let (space_col, space_row) = find_footer_coords(buf, "[Space] Toggle").unwrap();
    let toggle_cmd = app.handle_event(mouse_left_click(space_col + 1, space_row));
    assert!(matches!(
        toggle_cmd,
        Some(TuiCommand::ConfigUpdate(bpmute::config::ConfigPatch {
            detection: Some(bpmute::config::DetectionPatch {
                sensitivity: Some(bpmute::config::Sensitivity::Responsive),
                ..
            }),
            ..
        }))
    ));

    let (ed_col, ed_row) = find_footer_coords(buf, "[E] Editor").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(ed_col + 1, ed_row)),
        Some(TuiCommand::EditorHandoff)
    );

    let (undo_col, undo_row) = find_footer_coords(buf, "[U] Undo Boundary").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(undo_col + 1, undo_row)),
        Some(TuiCommand::CalibrationUndoBoundary)
    );

    let (rebase_col, rebase_row) = find_footer_coords(buf, "[B] Rebase").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(rebase_col + 1, rebase_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (purge_col, purge_row) =
        find_footer_coords(terminal.backend().buffer(), "[P] Purge").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(purge_col + 1, purge_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::PurgeRawModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (wipe_col, wipe_row) = find_footer_coords(terminal.backend().buffer(), "[W] Wipe").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(wipe_col + 1, wipe_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::HardWipeModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 5. Global [?] Help and [q] Quit
    let (help_col, help_row) = find_footer_coords(terminal.backend().buffer(), "[?] Help").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(help_col + 1, help_row)),
        None
    );
    assert_eq!(app.modal, ModalState::Help);
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (quit_col, quit_row) = find_footer_coords(terminal.backend().buffer(), "[q] Quit").unwrap();
    assert!(!app.should_quit);
    assert_eq!(
        app.handle_event(mouse_left_click(quit_col + 1, quit_row)),
        None
    );
    assert!(app.should_quit);
}

#[test]
fn test_mouse_footer_disabled_and_muted_guards() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_config = Some(bpmute::config::Config::default());
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:01",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        None,
    )];
    let active_session = SessionListEntry {
        id: "hist-active".into(),
        wearable_id: "w-1".into(),
        wearable_alias: Some("Polar H10".into()),
        wearable_mac_masked: "..:22:01".into(),
        status: SessionStatus::Active,
        start_ts_ms: 1_700_000_000_000,
        end_ts_ms: 1_700_000_600_000,
        end_reason: None,
        detection_sensitivity: Sensitivity::Balanced,
        had_rr: true,
        warmup_mu_hr: Some(72.0),
        sleep_mu_hr: None,
        onset_confirmed_ts_ms: None,
        sleeping_ts_ms: None,
        auto_anomalous: false,
        fp_count: 0,
        epochs_count: 5,
        raw_samples_count: 100,
        feedback: None,
    };
    app.history_sessions = vec![active_session];
    app.storage_stats = Some(bpmute::storage::StorageStats {
        db_size_bytes: 1024,
        wal_size_bytes: 0,
        db_size_formatted: "1.0 KB".into(),
        sessions_count: 1,
        session_epochs_count: 5,
        raw_samples_count: 100,
        session_feedback_count: 0,
        calibration_boundaries_count: 1,
        wearables_count: 1,
        session_epochs_retention: "90d".into(),
        raw_samples_retention: "7d".into(),
        calibration_maturity: CalibrationMaturity::Calibrated,
        active_calibration_boundary: Some(CalibrationBoundaryRecord {
            id: "bnd-1".into(),
            effective_ts_ms: 1_700_000_000_000,
            created_at_ms: 1_700_000_000_000,
            reason: BoundaryReason::ManualRebaseline,
            seed_sleep_hr: Some(55.0),
            reset_floors: false,
            reverted_at_ms: None,
        }),
    });

    let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();

    // 1. Active Monitoring Session in History: [f], [x], [D] must be muted and ignore clicks
    app.active_tab = Tab::History;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for label in ["[f] Feedback", "[x] Export", "[D] Delete"] {
        let (col, row) = find_footer_coords(terminal.backend().buffer(), label).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(col, row)].fg,
            Color::DarkGray,
            "{label} must render muted when selected session is Active"
        );
        assert_eq!(app.handle_event(mouse_left_click(col + 1, row)), None);
        assert_eq!(app.modal, ModalState::None);
    }

    // 2. Settings: remove active Calibration Boundary -> [U] disappears and stale coords ignore clicks
    app.active_tab = Tab::Settings;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (undo_col, undo_row) =
        find_footer_coords(terminal.backend().buffer(), "[U] Undo Boundary").unwrap();
    app.storage_stats
        .as_mut()
        .unwrap()
        .active_calibration_boundary = None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(find_footer_coords(terminal.backend().buffer(), "[U] Undo Boundary").is_none());
    // Because [U] disappeared, [?] Help shifted into [U]'s former columns (proving dynamic HitMap alignment)
    assert_ne!(
        app.handle_event(mouse_left_click(undo_col + 1, undo_row)),
        Some(TuiCommand::CalibrationUndoBoundary)
    );
    app.modal = ModalState::None;

    // 3. Wearables with empty enrolled list: [n], [c], [u] are muted and ignore clicks
    app.active_tab = Tab::Wearables;
    app.enrolled_wearables.clear();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for label in ["[n] Rename", "[c] Reconnect", "[u] Unenroll"] {
        let (col, row) = find_footer_coords(terminal.backend().buffer(), label).unwrap();
        assert_eq!(terminal.backend().buffer()[(col, row)].fg, Color::DarkGray);
        assert_eq!(app.handle_event(mouse_left_click(col + 1, row)), None);
        assert_eq!(app.modal, ModalState::None);
    }

    // 4. Daemon offline: mutating footer actions across tabs are muted and ignore clicks,
    // while local actions ([d] Diagnostics, [E] Editor, [?] Help, [q] Quit) still work.
    app.daemon_connected = false;

    app.active_tab = Tab::Dashboard;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (pause_col, pause_row) =
        find_footer_coords(terminal.backend().buffer(), "[p] Pause").unwrap();
    assert_eq!(
        terminal.backend().buffer()[(pause_col, pause_row)].fg,
        Color::DarkGray
    );
    assert_eq!(
        app.handle_event(mouse_left_click(pause_col + 1, pause_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    let (diag_col, diag_row) =
        find_footer_coords(terminal.backend().buffer(), "[d] Diagnostics").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(diag_col + 1, diag_row)),
        None
    );
    assert!(app.is_diagnostics_open);

    app.active_tab = Tab::Wearables;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (enroll_col, enroll_row) =
        find_footer_coords(terminal.backend().buffer(), "[e] Enroll").unwrap();
    assert_eq!(
        terminal.backend().buffer()[(enroll_col, enroll_row)].fg,
        Color::DarkGray
    );
    assert_eq!(
        app.handle_event(mouse_left_click(enroll_col + 1, enroll_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    app.active_tab = Tab::Settings;
    app.storage_stats
        .as_mut()
        .unwrap()
        .active_calibration_boundary = Some(CalibrationBoundaryRecord {
        id: "bnd-1".into(),
        effective_ts_ms: 1_700_000_000_000,
        created_at_ms: 1_700_000_000_000,
        reason: BoundaryReason::ManualRebaseline,
        seed_sleep_hr: Some(55.0),
        reset_floors: false,
        reverted_at_ms: None,
    });
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for label in [
        "[Space] Toggle",
        "[B] Rebase",
        "[P] Purge",
        "[W] Wipe",
        "[U] Undo Boundary",
    ] {
        let (col, row) = find_footer_coords(terminal.backend().buffer(), label).unwrap();
        assert_eq!(terminal.backend().buffer()[(col, row)].fg, Color::DarkGray);
        assert_eq!(app.handle_event(mouse_left_click(col + 1, row)), None);
        assert_eq!(app.modal, ModalState::None);
    }
    let (ed_col, ed_row) = find_footer_coords(terminal.backend().buffer(), "[E] Editor").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(ed_col + 1, ed_row)),
        Some(TuiCommand::EditorHandoff)
    );
}

#[test]
fn test_mouse_modal_footer_actions_and_orientation_hint_ignores() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.discovered_wearables = vec![DiscoveredWearable {
        address: "AA:BB:CC:DD:EE:01".into(),
        address_type: "public".into(),
        name: Some("Polar H10".into()),
        rssi: Some(-55),
        paired: false,
        already_enrolled: false,
        uuids: vec![],
    }];
    let sample_wearable = make_wearable(
        "w-1",
        "AA:BB:CC:11:22:01",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        None,
    );

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();

    // 1. Help modal footer: [Esc/?] Close Help and [q] Quit
    app.modal = ModalState::Help;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (close_col, close_row) =
        find_footer_coords(terminal.backend().buffer(), "[Esc/?] Close Help").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(close_col + 1, close_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    app.modal = ModalState::Help;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (quit_col, quit_row) = find_footer_coords(terminal.backend().buffer(), "[q] Quit").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(quit_col + 1, quit_row)),
        None
    );
    assert!(app.should_quit);
    app.should_quit = false;

    // 2. PausePopover footer
    app.modal = ModalState::PausePopover { selected_idx: 2 };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (sel_col, sel_row) =
        find_footer_coords(terminal.backend().buffer(), "[1-5/j/k] Select").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(sel_col + 1, sel_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::PausePopover { .. }));

    let (enter_col, enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Confirm").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(enter_col + 1, enter_row)),
        Some(TuiCommand::Pause(PauseParams {
            mode: Some(PauseMode::Duration),
            duration_sec: Some(3600),
        }))
    );
    assert_eq!(app.modal, ModalState::None);

    app.modal = ModalState::PausePopover { selected_idx: 0 };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (esc_col, esc_row) =
        find_footer_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(esc_col + 1, esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    // 3. EnrollmentModal footer: navigation hint ignored, [r] Rescan, [Enter] Enroll, [Esc] Cancel
    app.active_tab = Tab::Wearables;
    app.modal = ModalState::EnrollmentModal {
        scanning_remaining_ms: 5_000,
        selected_discovered_idx: 0,
        alias_input: "Sleep Strap".into(),
        alias_focused: false,
        enrolling: false,
        error_message: Some("old err".into()),
        pairing_prompt: None,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (nav_col, nav_row) =
        find_footer_coords(terminal.backend().buffer(), "[j/k/↑/↓] Select Wearable").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(nav_col + 1, nav_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));

    let (rescan_col, rescan_row) =
        find_footer_coords(terminal.backend().buffer(), "[r] Rescan").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(rescan_col + 1, rescan_row)),
        Some(TuiCommand::EnrollStart(Some(30)))
    );
    if let ModalState::EnrollmentModal {
        scanning_remaining_ms,
        ref error_message,
        ..
    } = app.modal
    {
        assert_eq!(scanning_remaining_ms, 30_000);
        assert_eq!(*error_message, None);
    } else {
        panic!("EnrollmentModal should stay open on rescan");
    }

    let (enroll_col, enroll_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Enroll").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(enroll_col + 1, enroll_row)),
        Some(TuiCommand::EnrollSelect {
            address: "AA:BB:CC:DD:EE:01".into(),
            alias: Some("Sleep Strap".into()),
        })
    );

    let (e_esc_col, e_esc_row) =
        find_footer_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(e_esc_col + 1, e_esc_row)),
        Some(TuiCommand::EnrollCancel)
    );
    assert_eq!(app.modal, ModalState::None);

    // 4. RenameModal footer
    app.modal = ModalState::RenameModal {
        wearable_id: "w-1".into(),
        current_alias: "Old".into(),
        input: "New Name".into(),
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (save_col, save_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Save Alias").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(save_col + 1, save_row)),
        Some(TuiCommand::WearableRename {
            id: "w-1".into(),
            alias: Some("New Name".into()),
        })
    );
    assert_eq!(app.modal, ModalState::None);

    app.modal = ModalState::RenameModal {
        wearable_id: "w-1".into(),
        current_alias: "Old".into(),
        input: "New Name".into(),
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (r_esc_col, r_esc_row) =
        find_footer_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(r_esc_col + 1, r_esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    // 5. UnenrollModal footer
    app.modal = ModalState::UnenrollModal {
        wearable: sample_wearable.clone(),
        forget_bluez: true,
        delete_sessions: false,
        selected_button: UnenrollButton::Confirm,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for hint in ["[1/2/Space] Toggle", "[Tab/←/→] Button"] {
        let (h_col, h_row) = find_footer_coords(terminal.backend().buffer(), hint).unwrap();
        assert_eq!(app.handle_event(mouse_left_click(h_col + 1, h_row)), None);
        assert!(matches!(app.modal, ModalState::UnenrollModal { .. }));
    }
    let (u_enter_col, u_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Confirm").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(u_enter_col + 1, u_enter_row)),
        Some(TuiCommand::WearableUnenroll {
            id: "w-1".into(),
            forget_bluez: true,
            delete_sessions: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    app.modal = ModalState::UnenrollModal {
        wearable: sample_wearable,
        forget_bluez: false,
        delete_sessions: false,
        selected_button: UnenrollButton::Confirm,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (u_esc_col, u_esc_row) =
        find_footer_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(u_esc_col + 1, u_esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    // 6. FeedbackModal footer
    app.active_tab = Tab::History;
    app.modal = ModalState::FeedbackModal {
        session_id: "s-1".into(),
        session_start_ts_ms: 1_700_000_000_000,
        session_end_ts_ms: 1_700_003_600_000,
        epochs_pruned: false,
        verdict_idx: 1,
        onset_hhmm_input: String::new(),
        avg_sleep_hr_input: "56.0".into(),
        active_field: bpmute::tui::app::FeedbackModalField::Verdict,
        error_message: None,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for hint in ["[Tab] Field", "[←/→/Space] Option"] {
        let (h_col, h_row) = find_footer_coords(terminal.backend().buffer(), hint).unwrap();
        assert_eq!(app.handle_event(mouse_left_click(h_col + 1, h_row)), None);
        assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));
    }
    let (f_enter_col, f_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Submit").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(f_enter_col + 1, f_enter_row)),
        Some(TuiCommand::SessionSetFeedback {
            session_id: "s-1".into(),
            verdict: Some(FeedbackVerdict::FalsePositive),
            onset_hhmm: None,
            avg_sleep_hr: Some(56.0),
            clear: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 7. ExportTraceModal footer
    app.modal = ModalState::ExportTraceModal {
        session_id: "s-1".into(),
        format_idx: 0,
        destination_path: "/tmp/out.csv".into(),
        anonymize: true,
        active_field: bpmute::tui::app::ExportModalField::Submit,
        error_message: None,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for hint in ["[Tab] Field", "[←/→/Space] Toggle"] {
        let (h_col, h_row) = find_footer_coords(terminal.backend().buffer(), hint).unwrap();
        assert_eq!(app.handle_event(mouse_left_click(h_col + 1, h_row)), None);
        assert!(matches!(app.modal, ModalState::ExportTraceModal { .. }));
    }
    let (x_enter_col, x_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Export").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(x_enter_col + 1, x_enter_row)),
        Some(TuiCommand::SessionExportTrace {
            id: "s-1".into(),
            path: "/tmp/out.csv".into(),
            format: bpmute::replay::TraceFormat::Csv,
            no_anonymize: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 8. DeleteSessionModal footer
    app.modal = ModalState::DeleteSessionModal {
        session_id: "s-1".into(),
        confirmed: true,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (d_hint_col, d_hint_row) =
        find_footer_coords(terminal.backend().buffer(), "[Tab/←/→] Button").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(d_hint_col + 1, d_hint_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::DeleteSessionModal { .. }));
    let (d_enter_col, d_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Confirm Delete").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(d_enter_col + 1, d_enter_row)),
        Some(TuiCommand::SessionDelete { id: "s-1".into() })
    );
    assert_eq!(app.modal, ModalState::None);

    // 9. SoftRebaselineModal footer
    app.active_tab = Tab::Settings;
    app.modal = ModalState::SoftRebaselineModal {
        seed_sleep_hr_input: "54.5".into(),
        reset_floors: true,
        active_field: bpmute::tui::app::SoftRebaselineField::SeedSleepHr,
        error_message: None,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for hint in ["[Tab] Field", "[Space] Toggle"] {
        let (h_col, h_row) = find_footer_coords(terminal.backend().buffer(), hint).unwrap();
        assert_eq!(app.handle_event(mouse_left_click(h_col + 1, h_row)), None);
        assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));
    }
    let (b_enter_col, b_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Submit").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(b_enter_col + 1, b_enter_row)),
        Some(TuiCommand::CalibrationSetBoundary {
            effective_ts_ms: None,
            seed_sleep_hr: Some(54.5),
            reset_floors: true,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 10. PurgeRawModal footer
    app.modal = ModalState::PurgeRawModal { confirmed: true };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (p_enter_col, p_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Confirm Purge").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(p_enter_col + 1, p_enter_row)),
        Some(TuiCommand::DataPurgeRaw)
    );
    assert_eq!(app.modal, ModalState::None);

    // 11. HardWipeModal footer (validation error then valid WIPE, plus Esc cancel)
    app.modal = ModalState::HardWipeModal {
        typed_input: "nope".into(),
        error_message: None,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (w_enter_col, w_enter_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Confirm Wipe").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(w_enter_col + 1, w_enter_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::HardWipeModal {
            error_message: Some(_),
            ..
        }
    ));

    if let ModalState::HardWipeModal {
        ref mut typed_input,
        ..
    } = app.modal
    {
        *typed_input = "WIPE".into();
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(w_enter_col + 1, w_enter_row)),
        Some(TuiCommand::DataHardWipe {
            confirm: "WIPE".into(),
            wipe_all: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    app.modal = ModalState::HardWipeModal {
        typed_input: String::new(),
        error_message: None,
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (w_esc_col, w_esc_row) =
        find_footer_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(w_esc_col + 1, w_esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_help_modal_scrolling_and_backdrop_dismissal() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    app.enrolled_wearables = vec![
        make_wearable(
            "w-1",
            "AA:BB:CC:11:22:01",
            Some("Strap 1"),
            StreamStatus::Streaming,
            Some(90),
            None,
        ),
        make_wearable(
            "w-2",
            "AA:BB:CC:11:22:02",
            Some("Strap 2"),
            StreamStatus::Disconnected,
            Some(70),
            None,
        ),
    ];

    // Open Help at minimum 60x18 viewport
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.modal, ModalState::Help);

    let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let initial_out = buffer_to_string(terminal.backend().buffer());
    assert!(initial_out.contains("Global Navigation:"));
    assert!(
        !initial_out.contains("Mouse: Click tabs/buttons/rows or scroll with mouse wheel."),
        "Bottom Help line overflows 60x18 viewport before scrolling"
    );

    // ScrollUp at top clamps cleanly at 0
    let (title_col, title_row) = find_text_coords(
        terminal.backend().buffer(),
        "BPMute Keyboard & Mouse Shortcuts",
    )
    .unwrap();
    app.handle_event(mouse_scroll_up(title_col + 2, title_row + 2));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Global Navigation:"));

    // ScrollDown from inside the modal and from over the shadowed backdrop (0, 5)
    // reaches the bottom lines while leaving background Wearables selection untouched.
    app.handle_event(mouse_scroll_down(title_col + 2, title_row + 2));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let after_one = buffer_to_string(terminal.backend().buffer());
    assert!(!after_one.contains("Global Navigation:"));

    // Scroll down past the end (e.g., 40 ticks) from shadowed background area (0, 5)
    for _ in 0..40 {
        app.handle_event(mouse_scroll_down(0, 5));
    }
    assert_eq!(
        app.selected_wearable_idx, 0,
        "Help wheel scrolling must not leak to background Wearables table"
    );
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let bottom_out = buffer_to_string(terminal.backend().buffer());
    assert!(
        bottom_out.contains("Unenroll selected wearable"),
        "Wearables Help section must be reachable by scrolling at 60x18"
    );
    assert!(
        bottom_out.contains("Click tabs/buttons/rows") && bottom_out.contains("wheel."),
        "Final Help line must be reachable by scrolling at 60x18"
    );

    // Scrolling up once from clamped bottom immediately moves up (no unclamped dead zone)
    app.handle_event(mouse_scroll_up(title_col + 2, title_row + 2));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let scrolled_up_one = buffer_to_string(terminal.backend().buffer());
    assert_ne!(
        scrolled_up_one, bottom_out,
        "Single ScrollUp from clamped bottom must immediately scroll back up"
    );

    // Left-clicking inside the Help modal box does NOT close Help
    app.handle_event(mouse_left_click(title_col + 2, title_row + 3));
    assert_eq!(
        app.modal,
        ModalState::Help,
        "Clicking inside the Help modal box must not dismiss it"
    );

    // Left-clicking outside the Help modal box (on the backdrop) closes Help
    app.handle_event(mouse_left_click(0, 0));
    assert_eq!(
        app.modal,
        ModalState::None,
        "Clicking the backdrop outside the Help modal box must close Help"
    );

    // Reopening Help resets scroll offset back to top
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.modal, ModalState::Help);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Global Navigation:"));

    // Backdrop click on left margin (0, 8) also dismisses Help
    app.handle_event(mouse_left_click(0, 8));
    assert_eq!(app.modal, ModalState::None);

    // Resize below 60x18 clears Help backdrop & scroll HitMap targets
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.modal, ModalState::Help);
    let mut small_term = Terminal::new(TestBackend::new(55, 15)).unwrap();
    small_term
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    app.handle_event(mouse_scroll_down(10, 5));
    assert_eq!(app.help_scroll_offset, 0);
    app.handle_event(mouse_left_click(0, 0));
    assert_eq!(app.modal, ModalState::Help);

    // --no-mouse suppresses Help wheel scroll and backdrop dismissal
    let mut app_nm = TuiApp::new(true, false, false);
    app_nm.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    terminal
        .draw(|f| app_nm.render(f.area(), f.buffer_mut()))
        .unwrap();
    app_nm.handle_event(mouse_scroll_down(10, 5));
    assert_eq!(app_nm.help_scroll_offset, 0);
    app_nm.handle_event(mouse_left_click(0, 0));
    assert_eq!(app_nm.modal, ModalState::Help);
}

#[test]
fn test_mouse_dashboard_alert_strip_actions() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.alerts = vec![DaemonAlert {
        id: alert_ids::PERSISTENT_SHIFT.into(),
        kind: "calibration".into(),
        severity: AlertSeverity::Info,
        message: "Persistent baseline shift detected".into(),
        dismissible: true,
    }];

    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    let (accept_col, accept_row) = find_text_coords(buf, "[a] Accept").unwrap();
    let (ignore_col, ignore_row) = find_text_coords(buf, "[i] Ignore").unwrap();
    let (dismiss_col, dismiss_row) = find_text_coords(buf, "[x] Dismiss").unwrap();
    let (msg_col, msg_row) = find_text_coords(buf, "Persistent").unwrap();

    // Clicking message body or border does nothing
    assert_eq!(
        app.handle_event(mouse_left_click(msg_col + 1, msg_row)),
        None
    );
    assert_eq!(app.handle_event(mouse_left_click(0, msg_row - 1)), None);

    // [a] Accept and [i] Ignore dispatch AcceptShift and DismissShift
    assert_eq!(
        app.handle_event(mouse_left_click(accept_col + 1, accept_row)),
        Some(TuiCommand::AcceptShift)
    );
    assert_eq!(
        app.handle_event(mouse_left_click(ignore_col + 1, ignore_row)),
        Some(TuiCommand::DismissShift)
    );

    // [x] Dismiss dispatches DismissAlert and removes the alert locally
    assert_eq!(
        app.handle_event(mouse_left_click(dismiss_col + 1, dismiss_row)),
        Some(TuiCommand::DismissAlert {
            id: alert_ids::PERSISTENT_SHIFT.into(),
        })
    );
    assert!(app.alerts.is_empty());

    // Non-shift dismissible alert: [a] and [i] are absent, [x] Dismiss works
    app.alerts = vec![DaemonAlert {
        id: "warn-battery".into(),
        kind: "wearable".into(),
        severity: AlertSeverity::Warn,
        message: "Low battery".into(),
        dismissible: true,
    }];
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    assert!(find_text_coords_in_row(buf, accept_row, "[a] Accept").is_none());
    assert!(find_text_coords_in_row(buf, ignore_row, "[i] Ignore").is_none());
    let (d_col, d_row) = find_text_coords_in_row(buf, accept_row, "[x] Dismiss").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(d_col + 1, d_row)),
        Some(TuiCommand::DismissAlert {
            id: "warn-battery".into(),
        })
    );
    assert!(app.alerts.is_empty());

    // Non-dismissible non-shift alert: no action hints, clicks inside alert strip are ignored
    app.alerts = vec![DaemonAlert {
        id: alert_ids::LOGIND_INHIBITED.into(),
        kind: "action".into(),
        severity: AlertSeverity::Warn,
        message: "Desktop idle inhibited".into(),
        dismissible: false,
    }];
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    let (inh_col, inh_row) = find_text_coords(buf, "Desktop idle").unwrap();
    assert!(find_text_coords_in_row(buf, inh_row, "[x] Dismiss").is_none());
    assert_eq!(
        app.handle_event(mouse_left_click(inh_col + 2, inh_row)),
        None
    );
    assert_eq!(app.alerts.len(), 1);

    // Offline guard: even if an alert is present while disconnected, clicks do not dispatch
    app.daemon_connected = false;
    app.alerts = vec![DaemonAlert {
        id: alert_ids::PERSISTENT_SHIFT.into(),
        kind: "calibration".into(),
        severity: AlertSeverity::Info,
        message: "Persistent baseline shift detected".into(),
        dismissible: true,
    }];
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    let (off_a_col, off_a_row) = find_text_coords(buf, "[a] Accept").unwrap();
    let (off_i_col, off_i_row) = find_text_coords(buf, "[i] Ignore").unwrap();
    let (off_x_col, off_x_row) = find_text_coords(buf, "[x] Dismiss").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(off_a_col + 1, off_a_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(off_i_col + 1, off_i_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(off_x_col + 1, off_x_row)),
        None
    );
    assert_eq!(app.alerts.len(), 1);
}

#[test]
fn test_mouse_dashboard_zero_wearables_cta() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;

    // 1. At 100x24 online with no enrolled wearables, Getting Started CTA is shown
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    // Non-interactive CTA prose ignores clicks
    let (msg_col, msg_row) = find_text_coords(buf, "No BLE Heart Rate").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(msg_col + 1, msg_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Dashboard);
    assert_eq!(app.modal, ModalState::None);

    // Clicking "Wearables tab (3)" switches to Wearables without opening EnrollmentModal
    let (tab_cta_col, tab_cta_row) = find_text_coords(buf, "Wearables tab (3)").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(tab_cta_col + 2, tab_cta_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Wearables);
    assert_eq!(app.modal, ModalState::None);

    // Return to Dashboard, re-render, and click [e] -> switches to Wearables + opens EnrollmentModal + EnrollStart
    app.active_tab = Tab::Dashboard;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (e_col, e_row) =
        find_text_coords(terminal.backend().buffer(), "[e] on the Dashboard").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(e_col + 1, e_row)),
        Some(TuiCommand::EnrollStart(Some(30)))
    );
    assert_eq!(app.active_tab, Tab::Wearables);
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));

    // 2. Narrow 60x18 viewport still keeps [e] and clipped "Wearables ta" clickable
    app.modal = ModalState::None;
    app.active_tab = Tab::Dashboard;
    let mut narrow = Terminal::new(TestBackend::new(60, 18)).unwrap();
    narrow
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let narrow_buf = narrow.backend().buffer();
    let (nw_col, nw_row) = find_text_coords(narrow_buf, "Wearables ta").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(nw_col + 1, nw_row)), None);
    assert_eq!(app.active_tab, Tab::Wearables);

    // 3. Offline renders sparkline instead of CTA
    app.active_tab = Tab::Dashboard;
    app.daemon_connected = false;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(find_text_coords(terminal.backend().buffer(), "Getting Started").is_none());
}

#[test]
fn test_mouse_dashboard_system_card_diagnostics_and_readonly_cards() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.armed = true;
    app.latest_bpm = Some(62.0);
    app.latest_p_sleep = Some(0.18);
    app.is_diagnostics_open = true;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(88),
        None,
    )];
    app.daemon_status = Some(DaemonStatus {
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
        dry_run: false,
        schedule: ScheduleConfig::default(),
        config_path: String::new(),
        config_loaded_from_file: false,
        socket_path: String::new(),
        log_biometrics: false,
        alerts: Vec::new(),
        idle_backend: Some("ext-idle-notify".into()),
        playing_players_count: Some(1),
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Read-only cards ignore clicks and wheel scroll
    for needle in [
        "Live Heart Rate",
        "62",
        "Sleep State",
        "P(SleepNREM)",
        "30-Minute Heart Rate Trace",
    ] {
        let (col, row) = find_text_coords(terminal.backend().buffer(), needle).unwrap();
        let target_row = if needle.contains("Heart Rate") || needle == "Sleep State" {
            row + 1
        } else {
            row
        };
        assert_eq!(
            app.handle_event(mouse_left_click(col + 1, target_row)),
            None
        );
        assert_eq!(
            app.handle_event(mouse_scroll_down(col + 1, target_row)),
            None
        );
        assert_eq!(app.handle_event(mouse_scroll_up(col + 1, target_row)), None);
        assert_eq!(app.active_tab, Tab::Dashboard);
        assert_eq!(app.modal, ModalState::None);
        assert!(app.is_diagnostics_open);
    }

    // 2. Wearable & System card: middle status line is read-only; Wearable: switches to Wearables
    let (stream_col, stream_row) =
        find_text_coords(terminal.backend().buffer(), "Streaming |").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(stream_col + 1, stream_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Dashboard);

    let (w_col, w_row) = find_text_coords(terminal.backend().buffer(), "Wearable: Polar").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(w_col + 1, w_row)), None);
    assert_eq!(app.active_tab, Tab::Wearables);

    app.active_tab = Tab::Dashboard;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 3. Armed / Paused / Disarmed line opens PausePopover when connected
    let (armed_col, armed_row) = find_text_coords(terminal.backend().buffer(), "Armed |").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(armed_col + 1, armed_row)),
        None
    );
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 0 });
    app.modal = ModalState::None;

    app.daemon_status.as_mut().unwrap().paused = true;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (paused_col, paused_row) =
        find_text_coords(terminal.backend().buffer(), "Paused |").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(paused_col + 1, paused_row)),
        None
    );
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 0 });
    app.modal = ModalState::None;

    app.daemon_status.as_mut().unwrap().paused = false;
    app.armed = false;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (dis_col, dis_row) = find_text_coords(terminal.backend().buffer(), "Disarmed |").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(dis_col + 1, dis_row)),
        None
    );
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 0 });
    app.modal = ModalState::None;

    // 4. Engine Diagnostics [d] header collapses diagnostics pane; body rows do not
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (diag_body_col, diag_body_row) =
        find_text_coords(terminal.backend().buffer(), "Idle Probe:").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(diag_body_col + 1, diag_body_row)),
        None
    );
    assert!(app.is_diagnostics_open);

    let (diag_hdr_col, diag_hdr_row) =
        find_text_coords(terminal.backend().buffer(), "Engine Diagnostics [d]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(diag_hdr_col + 1, diag_hdr_row)),
        None
    );
    assert!(!app.is_diagnostics_open);

    // Re-rendering after collapse removes diagnostics header hit region
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(find_text_coords(terminal.backend().buffer(), "Engine Diagnostics [d]").is_none());
    assert_eq!(
        app.handle_event(mouse_left_click(diag_hdr_col + 1, diag_hdr_row)),
        None
    );
    assert!(!app.is_diagnostics_open);

    // 5. Offline: Wearable: still switches to Wearables, Disarmed line is a no-op
    app.daemon_connected = false;
    app.daemon_status = None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (off_dis_col, off_dis_row) =
        find_text_coords(terminal.backend().buffer(), "Disarmed |").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(off_dis_col + 1, off_dis_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    let (off_w_col, off_w_row) =
        find_text_coords(terminal.backend().buffer(), "Wearable: Polar").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(off_w_col + 1, off_w_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Wearables);

    // 6. Below 80 cols (79x24): Wearable & System collapses so old coordinates no-op
    app.active_tab = Tab::Dashboard;
    let mut narrow = Terminal::new(TestBackend::new(79, 24)).unwrap();
    narrow
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(find_text_coords(narrow.backend().buffer(), "Wearable & System").is_none());
    assert_eq!(
        app.handle_event(mouse_left_click(off_w_col + 1, off_w_row)),
        None
    );
    assert_eq!(app.active_tab, Tab::Dashboard);
}

#[test]
fn test_mouse_pause_popover_two_click_wheel_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.grace_state = Some(GraceState {
        session_id: "s-grace".into(),
        remaining_sec: 30,
        total_sec: 60,
        planned_actions: vec!["Pause Media".into()],
        dry_run: false,
        phase: GracePhase::Countdown,
    });
    app.modal = ModalState::PausePopover { selected_idx: 0 };

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // Verify all 5 options with state-based two-click selection and confirmation
    let expected_cases = [
        (0usize, "1. Resume monitoring", TuiCommand::Resume),
        (
            1usize,
            "2. Pause indefinitely",
            TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::Indefinite),
                duration_sec: None,
            }),
        ),
        (
            2usize,
            "3. Pause 1 hour",
            TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::Duration),
                duration_sec: Some(3600),
            }),
        ),
        (
            3usize,
            "4. Pause 2 hours",
            TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::Duration),
                duration_sec: Some(7200),
            }),
        ),
        (
            4usize,
            "5. Pause until 08:00 (Active Hours end)",
            TuiCommand::Pause(PauseParams {
                mode: Some(PauseMode::UntilMorning),
                duration_sec: None,
            }),
        ),
    ];

    for (idx, label, expected_cmd) in expected_cases {
        // Start with a different selected_idx when idx > 0 to prove first click selects, second confirms
        let initial_sel = if idx == 0 { 1 } else { 0 };
        app.modal = ModalState::PausePopover {
            selected_idx: initial_sel,
        };
        terminal
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let (col, row) = find_text_coords(terminal.backend().buffer(), label).unwrap();

        // First click selects without dispatching
        assert_eq!(app.handle_event(mouse_left_click(col + 1, row)), None);
        assert_eq!(app.modal, ModalState::PausePopover { selected_idx: idx });

        // Second click on already-selected option confirms and closes popover
        assert_eq!(
            app.handle_event(mouse_left_click(col + 1, row)),
            Some(expected_cmd)
        );
        assert_eq!(app.modal, ModalState::None);
    }

    // Mouse-wheel scrolling navigates 0..=4 and clamps at both ends
    app.modal = ModalState::PausePopover { selected_idx: 0 };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (pop_col, pop_row) =
        find_text_coords(terminal.backend().buffer(), "Pause / Resume Monitoring").unwrap();

    // ScrollUp at 0 clamps at 0
    assert_eq!(
        app.handle_event(mouse_scroll_up(pop_col + 2, pop_row + 2)),
        None
    );
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 0 });

    // ScrollDown steps 0 -> 1 -> 2 -> 3 -> 4 and clamps at 4
    for expected in 1..=4 {
        assert_eq!(
            app.handle_event(mouse_scroll_down(pop_col + 2, pop_row + 2)),
            None
        );
        assert_eq!(
            app.modal,
            ModalState::PausePopover {
                selected_idx: expected
            }
        );
    }
    assert_eq!(app.handle_event(mouse_scroll_down(0, 5)), None);
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 4 });

    // ScrollUp steps back from 4 -> 3 (immediate, no unclamped dead zone)
    assert_eq!(app.handle_event(mouse_scroll_up(0, 5)), None);
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 3 });

    // Clicking popover title, border, or empty inner row below option 5 consumes without closing
    assert_eq!(
        app.handle_event(mouse_left_click(pop_col + 1, pop_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(pop_col + 1, pop_row + 6)),
        None
    );
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 3 });

    // Clicking outside popover (dashboard area and header row) closes PausePopover
    assert_eq!(app.handle_event(mouse_left_click(0, 5)), None);
    assert_eq!(app.modal, ModalState::None);

    app.modal = ModalState::PausePopover { selected_idx: 0 };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.modal, ModalState::None);

    // Resize below 60x18 clears HitMap so stale backdrop/wheel coords are ignored
    app.modal = ModalState::PausePopover { selected_idx: 1 };
    let mut small = Terminal::new(TestBackend::new(55, 15)).unwrap();
    small
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(app.handle_event(mouse_scroll_down(10, 5)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 1 });

    // --no-mouse suppresses PausePopover wheel, option click, and backdrop dismissal
    let mut app_nm = TuiApp::new(true, false, false);
    app_nm.daemon_connected = true;
    app_nm.modal = ModalState::PausePopover { selected_idx: 0 };
    terminal
        .draw(|f| app_nm.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (nm_col, nm_row) =
        find_text_coords(terminal.backend().buffer(), "2. Pause indefinitely").unwrap();
    assert_eq!(
        app_nm.handle_event(mouse_left_click(nm_col + 1, nm_row)),
        None
    );
    assert_eq!(app_nm.handle_event(mouse_scroll_down(0, 5)), None);
    assert_eq!(app_nm.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app_nm.modal, ModalState::PausePopover { selected_idx: 0 });
}

#[test]
fn test_dashboard_active_hours_24_7_and_pause_popover_end_time() {
    use bpmute::config::{ClockTime, Config, ScheduleConfig};

    let mut app = TuiApp::new(false, false, false);
    app.handle_ipc(IpcEvent::Connected(make_daemon_status(
        ScheduleConfig::default(),
    )));
    app.active_config = Some(Config::default());

    let mut term80 = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term80
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_default = buffer_to_string(term80.backend().buffer());
    assert!(
        out_default.contains("Active: 24/7") && !out_default.contains("Sched: Off"),
        "System card must render glossary-aligned 24/7 status:\n{out_default}"
    );

    // ConfigLoaded updates schedule.end to 12:00 while Active Hours stays disabled.
    let cfg_12 = Config {
        schedule: ScheduleConfig {
            active_hours_enabled: false,
            start: ClockTime::new(23, 0).unwrap(),
            end: ClockTime::new(12, 0).unwrap(),
        },
        ..Default::default()
    };
    app.handle_ipc(IpcEvent::ConfigLoaded(cfg_12.clone()));

    app.modal = ModalState::PausePopover { selected_idx: 4 };
    for (w, h) in [(80, 24), (60, 18)] {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
        let out_pop = buffer_to_string(term.backend().buffer());
        assert!(
            out_pop.contains("5. Pause until 12:00 (Active Hours end)"),
            "PausePopover at {w}x{h} must show dynamic schedule.end without clipping:\n{out_pop}"
        );
    }

    // Enabling Active Hours switches System card to the configured window.
    let mut cfg_on = cfg_12;
    cfg_on.schedule.active_hours_enabled = true;
    app.handle_ipc(IpcEvent::ConfigLoaded(cfg_on));
    app.modal = ModalState::None;
    term80
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_on = buffer_to_string(term80.backend().buffer());
    assert!(
        out_on.contains("23:00-12:00"),
        "System card must reflect ConfigLoaded Active Hours window:\n{out_on}"
    );
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_op_error_surfaces_feedback_banner() {
    let mut h = RealSocketTuiHarness::spawn(1, false).await;

    let export_path = h._temp.path().join("missing-trace.csv");
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('x'))));
    if let ModalState::ExportTraceModal {
        ref mut destination_path,
        ..
    } = h.app.modal
    {
        *destination_path = export_path.to_string_lossy().into_owned();
    }
    let export_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .unwrap();
    h.cmd_tx.send(export_cmd).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.op_feedback.as_ref().is_some_and(|fb| fb.is_error)
    })
    .await;

    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal
        .draw(|f| h.app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let rendered = buffer_to_string(terminal.backend().buffer());
    assert!(rendered.contains("Error: no trace data available"));

    // Next key clears the transient op_feedback banner.
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert!(h.app.op_feedback.is_none());

    h.shutdown().await;
}

#[test]
fn test_dashboard_braille_trace_title_guide_and_empty_fallback() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        Some("Chest"),
    )];

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let empty_buf = terminal.backend().buffer();
    let empty_out = buffer_to_string(empty_buf);

    assert!(empty_out.contains("30-Minute Heart Rate Trace"));
    assert!(!empty_out.contains("30-Minute Heart Rate Sparkline"));
    assert!(empty_out.contains("100bpm"));
    assert!(empty_out.contains("50bpm"));

    // Midpoint -15m vertical guide ('┆') must be rendered across plot rows in muted style
    let (_, top_lbl_row) = find_text_coords(empty_buf, "100bpm").unwrap();
    let (_, bot_lbl_row) = find_text_coords(empty_buf, "50bpm").unwrap();
    let mut guide_rows = 0;
    for y in top_lbl_row..=bot_lbl_row {
        let has_guide = (0..empty_buf.area.width).any(|x| empty_buf[(x, y)].symbol() == "┆");
        if has_guide {
            guide_rows += 1;
        }
    }
    assert_eq!(
        guide_rows,
        (bot_lbl_row - top_lbl_row + 1) as usize,
        "Expected -15m vertical guide '┆' on every plot row:\n{empty_out}"
    );

    // Feed 30 minutes of 1 Hz readings declining from 72 bpm to 56 bpm
    let t0 = 1_700_000_000_000i64;
    for sec in 0..=1800 {
        let bpm = 72.0 - (sec as f64 / 1800.0) * 16.0;
        app.handle_event(TuiEvent::ipc(IpcEvent::HrSample(HrSampleEvent {
            ts_ms: t0 + sec * 1_000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "w-1".into(),
            session_id: Some("s-1".into()),
        })));
    }

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    let out = buffer_to_string(buf);

    let (_, top_row) = find_text_coords(buf, "74bpm").expect("auto-scaled top label 74bpm");
    let (_, bot_row) = find_text_coords(buf, "54bpm").expect("auto-scaled bottom label 54bpm");

    // Every chart column (from chart_x to chart_x + chart_w) should have at least one Braille cell
    // for a continuous 30-minute 1 Hz trace.
    let (now_col, _) = find_text_coords(buf, "Now").unwrap();
    let (start_col, _) = find_text_coords(buf, "-30m").unwrap();
    let chart_right = now_col + 2; // "Now" ends at chart_x + chart_w - 1
    let mut cols_with_braille = 0usize;
    let total_cols = (chart_right - start_col + 1) as usize;
    for x in start_col..=chart_right {
        let has_braille = (top_row..=bot_row).any(|y| {
            buf[(x, y)]
                .symbol()
                .chars()
                .next()
                .is_some_and(is_non_blank_braille)
        });
        if has_braille {
            cols_with_braille += 1;
        }
    }
    assert_eq!(
        cols_with_braille, total_cols,
        "Expected continuous Braille trace across all {total_cols} columns, got {cols_with_braille}:\n{out}"
    );
}

#[test]
fn test_dashboard_fixed_30m_axis_tick_drift_and_equal_timestamps() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        Some("Chest"),
    )];

    // 1. Feed a 2-minute (120s) burst of 1 Hz readings
    let t0 = 1_700_000_000_000i64;
    for sec in 0..=120 {
        let bpm = 60.0 + (sec as f64 / 120.0) * 6.0;
        app.handle_event(TuiEvent::ipc(IpcEvent::HrSample(HrSampleEvent {
            ts_ms: t0 + sec * 1_000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "w-1".into(),
            session_id: Some("s-1".into()),
        })));
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf1 = terminal.backend().buffer().clone();
    let out1 = buffer_to_string(&buf1);

    let (start_col, _) = find_text_coords(&buf1, "-30m").unwrap();
    let (now_col, _) = find_text_coords(&buf1, "Now").unwrap();
    let chart_right = now_col + 2;
    let (_, top_row) = find_text_coords(&buf1, "69bpm").unwrap();
    let (_, bot_row) = find_text_coords(&buf1, "57bpm").unwrap();

    let braille_cols = |buf: &Buffer| -> Vec<u16> {
        (start_col..=chart_right)
            .filter(|&x| {
                (top_row..=bot_row).any(|y| {
                    buf[(x, y)]
                        .symbol()
                        .chars()
                        .next()
                        .is_some_and(is_non_blank_braille)
                })
            })
            .collect()
    };

    let cols_initial = braille_cols(&buf1);
    assert!(
        !cols_initial.is_empty(),
        "2-minute burst should render Braille trace:\n{out1}"
    );
    let burst_cutoff = chart_right.saturating_sub(8);
    assert!(
        cols_initial.iter().all(|&x| x >= burst_cutoff),
        "2-minute burst must stay anchored near Now (>= {burst_cutoff}), got {cols_initial:?}:\n{out1}"
    );

    // 2. Advance 20 minutes (4800 ticks * 250ms) during Signal Lost while connected -> trace drifts leftward
    app.enrolled_wearables[0].status = StreamStatus::SignalLost;
    for _ in 0..4800 {
        app.handle_event(TuiEvent::Tick);
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf2 = terminal.backend().buffer().clone();
    let out2 = buffer_to_string(&buf2);
    let cols_shifted = braille_cols(&buf2);
    assert!(
        !cols_shifted.is_empty(),
        "Shifted 2-minute trace should still be visible at -22m..-20m:\n{out2}"
    );
    let mid_col = start_col + (chart_right - start_col) / 2;
    assert!(
        cols_shifted.iter().all(|&x| x < mid_col),
        "After 20m of ticks, 2m trace must sit left of -15m (< {mid_col}), leaving trailing dropout at Now; got {cols_shifted:?}:\n{out2}"
    );

    // 3. While disconnected, ticks must NOT advance window_end_ms
    app.daemon_connected = false;
    for _ in 0..4800 {
        app.handle_event(TuiEvent::Tick);
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf3 = terminal.backend().buffer().clone();
    assert_eq!(
        braille_cols(&buf3),
        cols_shifted,
        "Ticks while disconnected must not advance the window"
    );

    // 4. Equal-timestamp fixture renders safely at the right edge without panics
    let mut eq_app = TuiApp::new(false, false, false);
    eq_app.daemon_connected = true;
    eq_app.enrolled_wearables = app.enrolled_wearables.clone();
    for bpm in [70.0, 65.0, 60.0] {
        eq_app.handle_event(TuiEvent::ipc(IpcEvent::HrSample(HrSampleEvent {
            ts_ms: 1_700_000_000_000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "w-1".into(),
            session_id: Some("s-1".into()),
        })));
    }
    terminal
        .draw(|f| eq_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let eq_buf = terminal.backend().buffer();
    let eq_cols: Vec<u16> = (start_col..=chart_right)
        .filter(|&x| {
            (top_row..=bot_row).any(|y| {
                eq_buf[(x, y)]
                    .symbol()
                    .chars()
                    .next()
                    .is_some_and(is_non_blank_braille)
            })
        })
        .collect();
    assert_eq!(
        eq_cols,
        vec![chart_right],
        "Equal-timestamp fixture must plot safely at the right edge"
    );
}

#[test]
fn test_dashboard_gap_breaking_and_dynamic_y_autoscaling() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        Some("Chest"),
    )];

    // 4 segments across 24 minutes separated by:
    // - Gap 1: > 5s silent timestamp jump (minutes 3..6)
    // - Gap 2: NoContact with bogus 140 bpm (minutes 9..12)
    // - Gap 3: bpm <= 20.0 & NaN (minutes 15..18)
    // Valid HRs stay strictly within 58.0..62.0 bpm -> padded 56..64 -> min span 12 -> 54..66 bpm
    let t0 = 1_700_000_000_000i64;
    let push = |app: &mut TuiApp, sec: i64, bpm: f64, contact: ContactStatus| {
        app.handle_event(TuiEvent::ipc(IpcEvent::HrSample(HrSampleEvent {
            ts_ms: t0 + sec * 1_000,
            bpm,
            rr_ms: vec![],
            contact_status: contact,
            wearable_id: "w-1".into(),
            session_id: Some("s-1".into()),
        })));
    };

    // Segment 1: 0..180s
    for sec in 0..=180 {
        push(
            &mut app,
            sec,
            58.0 + (sec as f64 / 180.0) * 4.0,
            ContactStatus::Contact,
        );
    }
    // Gap 1: silent jump 181..360s (180s > 5s)
    // Segment 2: 360..540s
    for sec in 360..=540 {
        push(
            &mut app,
            sec,
            62.0 - ((sec - 360) as f64 / 180.0) * 4.0,
            ContactStatus::Contact,
        );
    }
    // Gap 2: NoContact 541..720s (with bogus 140 bpm that must NOT skew Y-axis)
    for sec in 541..720 {
        push(&mut app, sec, 140.0, ContactStatus::NoContact);
    }
    // Segment 3: 720..900s
    for sec in 720..=900 {
        push(
            &mut app,
            sec,
            58.0 + ((sec - 720) as f64 / 180.0) * 4.0,
            ContactStatus::Contact,
        );
    }
    // Gap 3: invalid bpm <= 20.0 and NaN 901..1080s
    for sec in 901..1080 {
        let bpm = if sec % 2 == 0 { 18.0 } else { f64::NAN };
        push(&mut app, sec, bpm, ContactStatus::Contact);
    }
    // Segment 4: 1080..1260s
    for sec in 1080..=1260 {
        push(
            &mut app,
            sec,
            62.0 - ((sec - 1080) as f64 / 180.0) * 4.0,
            ContactStatus::Contact,
        );
    }

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    let out = buffer_to_string(buf);

    // Dynamic Y-axis auto-scaling: [58, 62] -> padded [56, 64] -> expanded to 12 bpm span [54, 66]
    let (_, top_row) = find_text_coords(buf, "66bpm").expect("auto-scaled 66bpm label");
    let (_, bot_row) = find_text_coords(buf, "54bpm").expect("auto-scaled 54bpm label");

    let (start_col, _) = find_text_coords(buf, "-30m").unwrap();
    let (now_col, _) = find_text_coords(buf, "Now").unwrap();
    let chart_right = now_col + 2;

    // Count distinct contiguous column runs of Braille glyphs across the chart.
    // 4 valid segments separated by 3 outages must produce 4 separate runs with empty gap columns between them!
    let mut runs = 0usize;
    let mut in_run = false;
    for x in start_col..=chart_right {
        let has_braille = (top_row..=bot_row).any(|y| {
            buf[(x, y)]
                .symbol()
                .chars()
                .next()
                .is_some_and(is_non_blank_braille)
        });
        if has_braille && !in_run {
            runs += 1;
            in_run = true;
        } else if !has_braille {
            in_run = false;
        }
    }
    assert_eq!(
        runs, 4,
        "Expected 4 disconnected Braille segments separated by empty gap columns:\n{out}"
    );
}

#[test]
fn test_grace_tick_activity_hold_banner_and_hit_map() {
    use bpmute::ipc::GraceTickEvent;

    // 1. Legacy GraceTickEvent JSON without `phase` deserializes with `GracePhase::Countdown`
    let legacy_tick: GraceTickEvent = serde_json::from_value(serde_json::json!({
        "session_id": "s-legacy",
        "remaining_sec": 23,
        "total_sec": 30,
        "power_action": "suspend",
        "dry_run": false,
        "planned_steps": ["Pause Media", "Suspend"]
    }))
    .expect("legacy GraceTickEvent JSON without phase must deserialize");
    assert_eq!(legacy_tick.phase, GracePhase::Countdown);

    for (ascii, no_color) in [(false, false), (true, true)] {
        let mut app = TuiApp::new(false, ascii, no_color);
        app.daemon_connected = true;

        // Countdown GraceTick -> GRACE: 23s
        app.handle_ipc(IpcEvent::GraceTick(legacy_tick.clone()));
        let mut term_80 = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term_80
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let out_countdown = buffer_to_string(term_80.backend().buffer());
        assert!(
            out_countdown.contains("GRACE: 23s"),
            "countdown tick must render GRACE: 23s (ascii={ascii}, no_color={no_color}):\n{out_countdown}"
        );
        assert!(!out_countdown.contains("ACTIVITY HOLD:"));

        // ActivityHold GraceTick -> ACTIVITY HOLD: 10s (with populated steps exercising clipping at 80x24)
        let hold_tick = GraceTickEvent {
            session_id: "s-hold".into(),
            remaining_sec: 10,
            total_sec: 30,
            power_action: bpmute::config::PowerAction::Suspend,
            dry_run: false,
            planned_steps: vec![
                "Pause Media".into(),
                "Custom Hook".into(),
                "Lock Screen".into(),
                "Suspend".into(),
            ],
            phase: GracePhase::ActivityHold,
        };
        app.handle_ipc(IpcEvent::GraceTick(hold_tick));
        term_80
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let out_hold_80 = buffer_to_string(term_80.backend().buffer());
        assert!(
            out_hold_80.contains("ACTIVITY HOLD: 10s"),
            "activity hold tick must render ACTIVITY HOLD: 10s at 80x24 (ascii={ascii}, no_color={no_color}):\n{out_hold_80}"
        );
        assert!(out_hold_80.contains("[c] Cancel"));
        assert!(out_hold_80.contains("[s] Snooze"));

        // Verify HitMap targets on 80x24
        let buf_80 = term_80.backend().buffer();
        let (c_col, c_row) = find_text_coords(buf_80, "[c] Cancel").unwrap();
        let (s_col, s_row) = find_text_coords(buf_80, "[s] Snooze").unwrap();
        assert_eq!(
            app.handle_event(mouse_left_click(c_col + 1, c_row)),
            Some(TuiCommand::CancelGrace)
        );
        assert_eq!(
            app.handle_event(mouse_left_click(s_col + 1, s_row)),
            Some(TuiCommand::SnoozeGrace(None))
        );

        // Verify wide layout (>= 96 cols) renders full [c] Cancel (I'm Awake) and HitMap
        let mut term_wide = Terminal::new(TestBackend::new(104, 24)).unwrap();
        term_wide
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let buf_wide = term_wide.backend().buffer();
        let (wc_col, wc_row) = find_text_coords(buf_wide, "[c] Cancel (I'm Awake)").unwrap();
        let (ws_col, ws_row) = find_text_coords(buf_wide, "[s] Snooze").unwrap();
        assert_eq!(
            app.handle_event(mouse_left_click(wc_col + 18, wc_row)),
            Some(TuiCommand::CancelGrace)
        );
        assert_eq!(
            app.handle_event(mouse_left_click(ws_col + 2, ws_row)),
            Some(TuiCommand::SnoozeGrace(None))
        );

        // Terminal GraceTick(0) clears grace_state
        app.handle_ipc(IpcEvent::GraceTick(GraceTickEvent {
            session_id: "s-hold".into(),
            remaining_sec: 0,
            total_sec: 30,
            power_action: bpmute::config::PowerAction::Suspend,
            dry_run: false,
            planned_steps: vec!["Pause Media".into(), "Suspend".into()],
            phase: GracePhase::ActivityHold,
        }));
        assert!(app.grace_state.is_none());
    }
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_tui_reopen_preserves_30m_trailing_gap_after_disconnect() {
    use bpmute::daemon::WearableEvent;
    use bpmute::sleep_engine::{HrPacket, StreamState};
    use bpmute::storage::WearableDescriptor;
    use chrono::TimeZone;
    use common::daemon::TestWearableSource;

    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 22, 10, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let (source, w_tx) = TestWearableSource::new();

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_wearable_source(Arc::new(source)),
    )
    .await
    .unwrap();

    let desc = WearableDescriptor {
        mac: "EC:09:86:B0:EC:05".to_string(),
        address_type: "random".to_string(),
        alias: Some("Watch".to_string()),
        advertised_name: Some("HUAWEI WATCH HR-C05".to_string()),
        supports_rr: false,
        supports_contact: true,
    };
    let t0 = start_utc.timestamp_millis();
    daemon
        .core()
        .persistence
        .upsert_wearable(desc.clone(), t0)
        .await
        .unwrap();

    // 1. Open TUI #1 while connected and stream a 2-minute HR burst
    let (mut app1, mut event_rx1, shutdown_tx1, bridge_task1) =
        spawn_tui_bridge_session(&paths.socket_path);
    wait_for_app_state(&mut app1, &mut event_rx1, |a| {
        a.daemon_connected && !a.enrolled_wearables.is_empty()
    })
    .await;

    w_tx.send(WearableEvent::StreamState {
        wearable: desc.clone(),
        state: StreamState {
            status: StreamStatus::Streaming,
            ts_ms: t0,
        },
    })
    .await
    .unwrap();

    for sec in 0..=120 {
        let bpm = 60.0 + (sec as f64 / 120.0) * 6.0;
        w_tx.send(WearableEvent::HrPacket {
            wearable: desc.clone(),
            packet: HrPacket {
                ts_ms: t0 + sec * 1_000,
                bpm,
                rr_ms: vec![],
                contact_status: ContactStatus::Contact,
            },
        })
        .await
        .unwrap();
    }

    let last_sample_ts = t0 + 120_000;
    wait_for_app_state(&mut app1, &mut event_rx1, |a| {
        a.sparkline_data
            .samples
            .back()
            .is_some_and(|s| s.ts_ms == last_sample_ts)
    })
    .await;

    // Advance virtual time to match the 2-minute burst and disconnect wearable
    tokio::time::advance(Duration::from_secs(120)).await;
    w_tx.send(WearableEvent::StreamState {
        wearable: desc.clone(),
        state: StreamState {
            status: StreamStatus::Disconnected,
            ts_ms: last_sample_ts,
        },
    })
    .await
    .unwrap();
    wait_for_app_state(&mut app1, &mut event_rx1, |a| {
        a.active_wearable()
            .is_some_and(|w| w.status == StreamStatus::Disconnected)
    })
    .await;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let (cols_initial, start_col, chart_right, out1) =
        render_dashboard_braille_cols(&mut terminal, &mut app1, "69bpm", "57bpm");
    let chart_w = chart_right - start_col + 1;
    assert!(
        !cols_initial.is_empty(),
        "Initial burst must render Braille trace:\n{out1}"
    );
    let burst_cutoff = chart_right.saturating_sub(8);
    assert!(
        cols_initial.iter().all(|&x| x >= burst_cutoff),
        "Initial burst must sit near Now (>= {burst_cutoff}), got {cols_initial:?}:\n{out1}"
    );

    // Close TUI #1 while wearable stays disconnected
    let _ = shutdown_tx1.send(true);
    let _ = bridge_task1.await;

    // 2. Advance virtual time 10 minutes while disconnected, then open a fresh TUI #2
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;

    let (mut app2, mut event_rx2, shutdown_tx2, bridge_task2) =
        spawn_tui_bridge_session(&paths.socket_path);
    wait_for_app_state(&mut app2, &mut event_rx2, |a| {
        a.daemon_connected
            && !a.enrolled_wearables.is_empty()
            && !a.sparkline_data.samples.is_empty()
    })
    .await;

    assert_eq!(
        app2.sparkline_data.samples.back().unwrap().ts_ms,
        last_sample_ts,
        "Daemon Snapshot must preserve original sample timestamps"
    );
    assert!(
        app2.sparkline_data
            .window_end_ms
            .is_some_and(|end| end >= last_sample_ts + 600_000),
        "Reopened TUI window_end_ms must reflect >=10m elapsed since disconnect (expected >= {}, got {:?})",
        last_sample_ts + 600_000,
        app2.sparkline_data.window_end_ms
    );
    assert_eq!(
        app2.latest_bpm, None,
        "Reopened TUI must not hydrate stale BPM while disconnected"
    );
    assert!(!app2.contact_ok);

    let (cols_reopen, _, _, out2) =
        render_dashboard_braille_cols(&mut terminal, &mut app2, "69bpm", "57bpm");
    assert!(
        !cols_reopen.is_empty(),
        "Reopened TUI after 10m disconnect must still show historical trace:\n{out2}"
    );
    let gap_cutoff = chart_right.saturating_sub(chart_w / 4);
    assert!(
        cols_reopen.iter().all(|&x| x < gap_cutoff),
        "Reopened TUI trace must sit left of >=10m trailing gap (< {gap_cutoff}), not flush at Now; got {cols_reopen:?}:\n{out2}"
    );

    let _ = shutdown_tx2.send(true);
    let _ = bridge_task2.await;

    // 3. Advance another 25 minutes (>= 35m after disconnect) and reopen TUI #3 -> stale trace clipped empty
    tokio::time::advance(Duration::from_secs(1500)).await;
    tokio::task::yield_now().await;

    let (mut app3, mut event_rx3, shutdown_tx3, bridge_task3) =
        spawn_tui_bridge_session(&paths.socket_path);
    wait_for_app_state(&mut app3, &mut event_rx3, |a| {
        a.daemon_connected
            && !a.enrolled_wearables.is_empty()
            && !a.sparkline_data.samples.is_empty()
    })
    .await;

    assert_eq!(app3.latest_bpm, None);
    assert!(!app3.contact_ok);

    let (cols_expired, _, _, out3) =
        render_dashboard_braille_cols(&mut terminal, &mut app3, "100bpm", "50bpm");
    assert!(
        cols_expired.is_empty(),
        "After >30m disconnect, reopened TUI must clip stale trace empty, got {cols_expired:?}:\n{out3}"
    );

    let _ = shutdown_tx3.send(true);
    let _ = bridge_task3.await;
    daemon.shutdown().await;
}

#[test]
fn test_dashboard_snapshot_server_now_ms_edge_cases() {
    use bpmute::ipc::{EnrolledWearableStatus, SnapshotEvent};

    let base_status = make_daemon_status(ScheduleConfig::default());
    let disconnected_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Disconnected,
        None,
        Some("Chest"),
    )];
    let streaming_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(88),
        Some("Chest"),
    )];
    let connecting_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Connecting,
        Some(88),
        Some("Chest"),
    )];

    let t0 = 1_700_000_000_000i64;
    let last_ts = t0 + 120_000;
    let ring: Vec<HrSampleEvent> = (0..=120)
        .map(|sec| HrSampleEvent {
            ts_ms: t0 + sec * 1_000,
            bpm: 60.0 + (sec as f64 / 120.0) * 6.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "w-1".into(),
            session_id: Some("s-1".into()),
        })
        .collect();

    let apply_snapshot = |wearables: Vec<EnrolledWearableStatus>,
                          hr_ring_buffer: Vec<HrSampleEvent>,
                          server_now_ms: Option<i64>| {
        let mut app = TuiApp::new(false, false, false);
        app.enrolled_wearables = wearables;
        app.handle_ipc(IpcEvent::Snapshot(Box::new(SnapshotEvent {
            status: base_status.clone(),
            hr_ring_buffer,
            alerts: vec![],
            server_now_ms,
        })));
        app
    };

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // 1. server_now_ms = last_ts + 20m -> trace sits left of -15m, Tick advances further, stale BPM cleared
    let now_20m = last_ts + 20 * 60_000;
    let mut app_20m = apply_snapshot(disconnected_wearables.clone(), ring.clone(), Some(now_20m));
    assert_eq!(app_20m.sparkline_data.window_end_ms, Some(now_20m));
    assert_eq!(app_20m.latest_bpm, None);
    assert!(!app_20m.contact_ok);
    let (cols_20m, start_col, chart_right, out_20m) =
        render_dashboard_braille_cols(&mut terminal, &mut app_20m, "69bpm", "57bpm");
    let mid_col = start_col + (chart_right - start_col) / 2;
    assert!(
        !cols_20m.is_empty() && cols_20m.iter().all(|&x| x < mid_col),
        "Snapshot at +20m must sit left of -15m (< {mid_col}), got {cols_20m:?}:\n{out_20m}"
    );
    app_20m.handle_event(TuiEvent::Tick);
    assert_eq!(app_20m.sparkline_data.window_end_ms, Some(now_20m + 250));

    // 2. server_now_ms = last_ts + 35m -> all stale samples clipped empty, default 100..50bpm Y bounds
    let mut app_35m = apply_snapshot(
        disconnected_wearables.clone(),
        ring.clone(),
        Some(last_ts + 35 * 60_000),
    );
    assert_eq!(app_35m.latest_bpm, None);
    assert!(!app_35m.contact_ok);
    let (cols_35m, _, _, out_35m) =
        render_dashboard_braille_cols(&mut terminal, &mut app_35m, "100bpm", "50bpm");
    assert!(
        cols_35m.is_empty(),
        "Snapshot at +35m must clip stale samples empty, got {cols_35m:?}:\n{out_35m}"
    );

    // 3. server_now_ms = None (legacy JSON payload without field) -> anchors at max_sample_ts (near Now)
    let legacy_snapshot: SnapshotEvent = serde_json::from_value(json!({
        "status": base_status,
        "hr_ring_buffer": ring,
        "alerts": []
    }))
    .expect("legacy Snapshot JSON without server_now_ms must deserialize");
    assert_eq!(legacy_snapshot.server_now_ms, None);
    let mut app_none = apply_snapshot(streaming_wearables.clone(), ring.clone(), None);
    assert_eq!(app_none.sparkline_data.window_end_ms, Some(last_ts));
    assert_eq!(app_none.latest_bpm, Some(66.0));
    assert!(app_none.contact_ok);
    let (cols_none, _, chart_right, out_none) =
        render_dashboard_braille_cols(&mut terminal, &mut app_none, "69bpm", "57bpm");
    let burst_cutoff = chart_right.saturating_sub(8);
    assert!(
        !cols_none.is_empty() && cols_none.iter().all(|&x| x >= burst_cutoff),
        "Legacy Snapshot without server_now_ms must stay anchored near Now (>= {burst_cutoff}):\n{out_none}"
    );

    // 4. server_now_ms < last_ts -> clamped to max_ts via max_ts.max(now)
    let app_skew = apply_snapshot(
        streaming_wearables.clone(),
        ring.clone(),
        Some(last_ts - 60_000),
    );
    assert_eq!(app_skew.sparkline_data.window_end_ms, Some(last_ts));
    assert_eq!(app_skew.latest_bpm, Some(66.0));
    assert!(app_skew.contact_ok);

    // 5. Empty hr_ring_buffer with server_now_ms: Some(now) -> window_end_ms stays None
    let app_empty = apply_snapshot(
        disconnected_wearables.clone(),
        vec![],
        Some(last_ts + 600_000),
    );
    assert_eq!(app_empty.sparkline_data.window_end_ms, None);
    assert_eq!(app_empty.latest_bpm, None);
    assert!(!app_empty.contact_ok);

    // 6. Fresh live Snapshot (<= 5s old, Streaming) hydrates latest_bpm and contact_ok
    let app_live = apply_snapshot(
        streaming_wearables.clone(),
        ring.clone(),
        Some(last_ts + 1_000),
    );
    assert_eq!(app_live.latest_bpm, Some(66.0));
    assert!(app_live.contact_ok);

    // 7. Stale Snapshot (> 5s old) even while Connecting/Streaming clears latest_bpm and contact_ok
    let app_connecting_stale = apply_snapshot(connecting_wearables, ring.clone(), Some(now_20m));
    assert_eq!(app_connecting_stale.latest_bpm, None);
    assert!(!app_connecting_stale.contact_ok);
    let app_streaming_stale = apply_snapshot(streaming_wearables, ring.clone(), Some(now_20m));
    assert_eq!(app_streaming_stale.latest_bpm, None);
    assert!(!app_streaming_stale.contact_ok);

    // 8. Disconnected wearable clears latest_bpm and contact_ok even if sample timestamp is 1s old
    let app_dc_race = apply_snapshot(disconnected_wearables, ring, Some(last_ts + 1_000));
    assert_eq!(app_dc_race.latest_bpm, None);
    assert!(!app_dc_race.contact_ok);
}
