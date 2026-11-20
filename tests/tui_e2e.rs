use std::fs;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
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
    WearableDescriptor, WearableEvent, WearableSource,
};
use bpmute::exit_code::ExitCode;
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::{
    AlertSeverity, DaemonAlert, DaemonStatus, DiscoveredWearable, EnrollAuthResponseParams,
    EnrolledWearableStatus, HrSampleEvent, MonitoringState, PauseMode, PauseParams, alert_ids,
};
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{
    CalibrationMaturity, ContactStatus, DetectorState, Epoch, EpochFeatures, HrPacket,
    SessionEndReason, SessionSummary, StreamState, StreamStatus,
};
use bpmute::storage::EnrolledWearable;
use bpmute::tui::{
    GraceState, IpcEvent, ModalState, PairingPromptModalState, Tab, Theme, TuiApp, TuiCommand,
    TuiEvent, TuiIpcBridge, UnenrollButton, detect_systemd_service_installed,
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

fn find_text_coords_in_row(buf: &Buffer, row: u16, needle: &str) -> Option<(u16, u16)> {
    if row >= buf.area.height {
        return None;
    }
    let mut row_str = String::new();
    let mut col_starts = Vec::new();
    for x in 0..buf.area.width {
        let sym = buf[(x, row)].symbol();
        col_starts.push((row_str.len(), x));
        row_str.push_str(sym);
    }
    let byte_idx = row_str.find(needle)?;
    let &(_, col) = col_starts.iter().find(|&&(b, _)| b == byte_idx)?;
    Some((col, row))
}

fn find_text_coords(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    for y in 0..buf.area.height {
        if let Some(coords) = find_text_coords_in_row(buf, y, needle) {
            return Some(coords);
        }
    }
    None
}

fn find_footer_coords(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    find_text_coords_in_row(buf, buf.area.height.saturating_sub(1), needle)
}

fn mouse_left_click(col: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn mouse_scroll_down(col: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

fn mouse_scroll_up(col: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
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

async fn wait_for_app_state_with_cmds<F>(
    app: &mut TuiApp,
    event_rx: &mut mpsc::Receiver<TuiEvent>,
    cmd_tx: Option<&mpsc::Sender<TuiCommand>>,
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
        if let Some(cmd) = app.handle_event(ev)
            && let Some(tx) = cmd_tx
        {
            let _ = tx.send(cmd).await;
        }
    }
    panic!("TUI app did not reach expected state within event budget");
}

async fn wait_for_app_state<F>(app: &mut TuiApp, event_rx: &mut mpsc::Receiver<TuiEvent>, pred: F)
where
    F: FnMut(&TuiApp) -> bool,
{
    wait_for_app_state_with_cmds(app, event_rx, None, pred).await;
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
    app.grace_state = Some(GraceState {
        session_id: "s-1".into(),
        remaining_sec: 45,
        total_sec: 60,
        planned_actions: vec!["Pause Media".into(), "Suspend".into()],
        dry_run: false,
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

    // When unit is NOT installed, hint directs users to `bpmute service install`
    let mut app_uninstalled = TuiApp::new(false, false, false);
    app_uninstalled.daemon_connected = true;
    app_uninstalled.systemd_service_installed = false;
    terminal
        .draw(|f| app_uninstalled.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("No BLE Heart Rate wearables enrolled yet."));
    assert!(out.contains("systemd --user unit not found"));
    assert!(out.contains("bpmute service install"));

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

use bpmute::calibration::{
    BoundaryReason, CalibrationBoundaryRecord, FeedbackVerdict, SessionFeedbackRecord,
    SessionLedgerRecord,
};
use bpmute::config::Sensitivity;
use bpmute::storage::{SessionFullDetail, SessionListEntry, SessionStatus, WearableInfo};

#[test]
fn test_history_tab_master_detail_chart_and_boundary_dividers() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let s1 = SessionListEntry {
        id: "sess-001".into(),
        wearable_id: "w-1".into(),
        wearable_alias: Some("Polar H10".into()),
        wearable_mac_masked: "..:22:33".into(),
        status: SessionStatus::Closed,
        start_ts_ms: 1_700_100_000_000,
        end_ts_ms: 1_700_102_700_000, // 45 min
        end_reason: Some(SessionEndReason::HostSuspend),
        detection_sensitivity: Sensitivity::Balanced,
        had_rr: true,
        warmup_mu_hr: Some(72.0),
        sleep_mu_hr: Some(58.0),
        onset_confirmed_ts_ms: Some(1_700_100_900_000), // 15m in
        sleeping_ts_ms: Some(1_700_101_800_000),        // 30m in
        auto_anomalous: false,
        fp_count: 0,
        epochs_count: 30,
        raw_samples_count: 2700,
        feedback: None,
    };

    let boundary = CalibrationBoundaryRecord {
        id: "bnd-001".into(),
        effective_ts_ms: 1_700_050_000_000,
        reason: BoundaryReason::ManualRebaseline,
        seed_sleep_hr: Some(55.0),
        reset_floors: false,
        created_at_ms: 1_700_050_000_000,
        reverted_at_ms: None,
    };

    let s2 = SessionListEntry {
        id: "sess-002".into(),
        wearable_id: "w-2".into(),
        wearable_alias: Some("GT 5".into()),
        wearable_mac_masked: "..:44:55".into(),
        status: SessionStatus::Closed,
        start_ts_ms: 1_700_000_000_000,
        end_ts_ms: 1_700_003_600_000, // 60 min
        end_reason: Some(SessionEndReason::StreamEnded),
        detection_sensitivity: Sensitivity::Balanced,
        had_rr: false,
        warmup_mu_hr: Some(68.0),
        sleep_mu_hr: Some(56.0),
        onset_confirmed_ts_ms: None,
        sleeping_ts_ms: None,
        auto_anomalous: true,
        fp_count: 1,
        epochs_count: 40,
        raw_samples_count: 0,
        feedback: Some(SessionFeedbackRecord {
            id: "fb-002".into(),
            session_id: "sess-002".into(),
            verdict: Some(FeedbackVerdict::FalsePositive),
            manual_onset_ts_ms: None,
            manual_avg_sleep_hr: None,
            notes: None,
            updated_at_ms: 1_700_004_000_000,
        }),
    };

    app.history_sessions = vec![s1.clone(), s2.clone()];
    app.history_boundaries = vec![boundary.clone()];
    app.selected_session_idx = 0;

    // Populate detail for selected session (s1)
    let epochs = (0..30)
        .map(|i| {
            let ts = s1.start_ts_ms + (i as i64) * 15_000;
            let (hr, p_sleep) = if i < 10 {
                (72.0 - (i as f64) * 0.5, 0.1)
            } else if i < 20 {
                (67.0 - ((i - 10) as f64) * 0.8, 0.6)
            } else {
                (58.0, 0.95)
            };
            Epoch {
                session_index: 1,
                ts_ms: ts,
                features: EpochFeatures {
                    hr_bpm_60s: hr,
                    z_hr: -1.0,
                    hr_slope_4m: -0.2,
                    r_var_120s: 0.3,
                    ln_rmssd_60s: Some(3.0),
                    delta_ln_rmssd_60s: Some(-0.1),
                    rmssd_sdnn_120s: Some(0.8),
                },
                hrv_valid: true,
                rr_artifact_frac: 0.0,
                coverage: 1.0,
                flatline_stale: false,
                accumulators_frozen: false,
                baseline_mu_hr: 72.0,
                baseline_sigma_hr: 3.0,
                baseline_frozen: true,
                p_active_wake: 0.05,
                p_quiet_wake: 0.1,
                p_sleep,
                state: if i < 10 {
                    DetectorState::Warmup
                } else if i < 20 {
                    DetectorState::Candidate
                } else {
                    DetectorState::Confirmed
                },
            }
        })
        .collect();

    app.selected_session_detail = Some(SessionFullDetail {
        session: SessionLedgerRecord {
            id: s1.id.clone(),
            wearable_id: s1.wearable_id.clone(),
            start_ts_ms: s1.start_ts_ms,
            end_ts_ms: s1.end_ts_ms,
            end_reason: SessionEndReason::HostSuspend,
            detection_sensitivity: Sensitivity::Balanced,
            had_rr: true,
            warmup_mu_hr: Some(72.0),
            warmup_sigma_hr: Some(3.0),
            warmup_ln_rmssd: Some(3.5),
            onset_candidate_ts_ms: Some(s1.start_ts_ms + 10 * 15_000),
            onset_confirmed_ts_ms: Some(s1.start_ts_ms + 12 * 15_000),
            sleeping_ts_ms: Some(s1.start_ts_ms + 20 * 15_000),
            sleep_mu_hr: Some(58.0),
            onset_hr_bpm: Some(64.0),
            delta_hr_onset: Some(8.0),
            sleep_ln_rmssd: Some(3.8),
            sleep_hr_var_ratio: Some(0.25),
            typical_sqi: Some(0.95),
            post_trigger_sleep_sec: 900,
            trigger_power_action: PowerAction::Suspend,
            rapid_resume_within_5m: false,
            grace_cancel_count: 0,
            rapid_wake_fp_count: 0,
            snooze_count: 0,
            auto_anomalous: false,
            epochs_count: 30,
            raw_samples_count: 2700,
        },
        status: SessionStatus::Closed,
        wearable: WearableInfo {
            id: "w-1".into(),
            mac_masked: "..:22:33".into(),
            alias: Some("Polar H10".into()),
            advertised_name: None,
            supports_rr: true,
            supports_contact: true,
            hr_var_floor: 0.25,
            typical_sqi: Some(0.95),
        },
        feedback: Some(SessionFeedbackRecord {
            id: "fb-001".into(),
            session_id: s1.id.clone(),
            verdict: None,
            manual_onset_ts_ms: Some(s1.start_ts_ms + 14 * 15_000),
            manual_avg_sleep_hr: Some(57.5),
            notes: None,
            updated_at_ms: s1.end_ts_ms + 60_000,
        }),
        epochs,
        has_raw_samples: true,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    // Master list verifications
    assert!(out.contains("Polar H10"));
    assert!(out.contains("45m") || out.contains("45 min"));
    assert!(out.contains("Calibration Boundary") || out.contains("Boundary"));
    assert!(out.contains("Rebaseline"));
    assert!(out.contains("GT 5"));
    assert!(out.contains("[FP]") || out.contains("False Positive"));
    assert!(out.contains("[ANOMALOUS]") || out.contains("Anomalous"));

    // Detail pane & chart verifications
    assert!(out.contains("sess-001") || out.contains("sess-001"));
    assert!(out.contains("Epoch Detail") || out.contains("HR + P(Sleep)"));
    assert!(out.contains("QW:72") || out.contains("72"));
    assert!(out.contains("Onset") || out.contains("[O]"));
    assert!(out.contains("Trigger") || out.contains("[T]"));
    assert!(out.contains("Manual") || out.contains("[M]"));

    // Navigation down to second session
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    assert_eq!(app.selected_session_idx, 1);
}

#[test]
fn test_history_session_modals_and_mutating_guards() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let active_session = SessionListEntry {
        id: "sess-active".into(),
        wearable_id: "w-1".into(),
        wearable_alias: Some("Polar H10".into()),
        wearable_mac_masked: "..:22:33".into(),
        status: SessionStatus::Active,
        start_ts_ms: 1_700_200_000_000,
        end_ts_ms: 1_700_201_000_000,
        end_reason: None,
        detection_sensitivity: Sensitivity::Balanced,
        had_rr: true,
        warmup_mu_hr: Some(70.0),
        sleep_mu_hr: None,
        onset_confirmed_ts_ms: None,
        sleeping_ts_ms: None,
        auto_anomalous: false,
        fp_count: 0,
        epochs_count: 10,
        raw_samples_count: 1000,
        feedback: None,
    };

    let closed_session = SessionListEntry {
        id: "sess-closed".into(),
        wearable_id: "w-1".into(),
        wearable_alias: Some("Polar H10".into()),
        wearable_mac_masked: "..:22:33".into(),
        status: SessionStatus::Closed,
        start_ts_ms: 1_700_100_000_000,
        end_ts_ms: 1_700_103_600_000, // 1 hour
        end_reason: Some(SessionEndReason::HostSuspend),
        detection_sensitivity: Sensitivity::Balanced,
        had_rr: true,
        warmup_mu_hr: Some(72.0),
        sleep_mu_hr: Some(58.0),
        onset_confirmed_ts_ms: Some(1_700_101_800_000),
        sleeping_ts_ms: Some(1_700_102_400_000),
        auto_anomalous: false,
        fp_count: 0,
        epochs_count: 30,
        raw_samples_count: 2700,
        feedback: None,
    };

    app.history_sessions = vec![active_session, closed_session.clone()];
    app.selected_session_idx = 0; // On active session!

    // 1. Guardrail: All mutating actions (f, x, D) MUST be disabled on actively recording sessions
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

    // 2. Select closed session (idx 1)
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    assert_eq!(app.selected_session_idx, 1);

    // 3. Feedback modal (f)
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('f')))),
        None
    );
    assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Manual Feedback") || out.contains("Wearable-Report"));
    assert!(out.contains("+2.5 bpm") || out.contains("nadir offset"));

    // Cycle verdict: None -> False Positive
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    // Move to HH:MM input
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert!(app.is_input_focused());

    // Enter invalid HH:MM format
    for c in "25:99".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    // Validation error prevents submit
    if let ModalState::FeedbackModal {
        ref error_message, ..
    } = app.modal
    {
        assert!(error_message.is_some());
    } else {
        panic!("Expected FeedbackModal to stay open on validation failure");
    }

    // Clear and enter valid HH:MM
    for _ in 0..5 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    // Calculate local HH:MM 30 mins after start
    let local_valid = chrono::Local
        .timestamp_millis_opt(closed_session.start_ts_ms + 1800 * 1000)
        .single()
        .unwrap()
        .format("%H:%M")
        .to_string();
    for c in local_valid.chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // Move to Avg Sleep HR field
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    for c in "58.5".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // Move to Submit button and press Enter
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    let submit_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert!(matches!(
        submit_cmd,
        Some(TuiCommand::SessionSetFeedback {
            ref session_id,
            verdict: Some(FeedbackVerdict::FalsePositive),
            ref onset_hhmm,
            avg_sleep_hr: Some(58.5),
            clear: false,
        }) if session_id == "sess-closed" && onset_hhmm.as_deref() == Some(&local_valid)
    ));
    assert_eq!(app.modal, ModalState::None);

    // 4. Export Trace modal (x)
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('x')))),
        None
    );
    assert!(matches!(app.modal, ModalState::ExportTraceModal { .. }));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_export = buffer_to_string(terminal.backend().buffer());
    assert!(out_export.contains("Export Trace"));
    assert!(out_export.contains("CSV") || out_export.contains("Format"));
    assert!(out_export.contains("[x] Anonymize") || out_export.contains("Anonymize: ON"));

    let export_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert!(matches!(
        export_cmd,
        Some(TuiCommand::SessionExportTrace {
            ref id,
            format: bpmute::replay::TraceFormat::Csv,
            no_anonymize: false,
            ..
        }) if id == "sess-closed"
    ));
    assert_eq!(app.modal, ModalState::None);

    // 5. Delete Session modal (D)
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('D')))),
        None
    );
    assert!(matches!(app.modal, ModalState::DeleteSessionModal { .. }));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_del = buffer_to_string(terminal.backend().buffer());
    assert!(out_del.contains("Delete session") || out_del.contains("Delete Session"));

    let del_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        del_cmd,
        Some(TuiCommand::SessionDelete {
            id: "sess-closed".into(),
        })
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_settings_live_reconfig_editor_and_calibration_storage() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;

    let mut cfg = bpmute::config::Config::default();
    cfg.detection.sensitivity = Sensitivity::Balanced;
    cfg.actions.pause_media = true;
    cfg.actions.lock_screen = true;
    cfg.actions.power_action = PowerAction::Suspend;
    cfg.actions.grace_period_sec = Some(30);
    cfg.actions.dry_run = false;
    cfg.actions.custom_command = vec!["pkill".into(), "-STOP".into(), "game".into()];
    cfg.actions.ignore_players = vec!["firefox".into(), "spotify".into()];
    cfg.bluetooth.adapter = Some("hci0".into());
    app.active_config = Some(cfg);

    app.storage_stats = Some(bpmute::storage::StorageStats {
        db_size_bytes: 145_408,
        db_size_formatted: "142 KB".into(),
        sessions_count: 12,
        session_epochs_count: 1420,
        raw_samples_count: 8500,
        session_feedback_count: 4,
        calibration_boundaries_count: 1,
        wearables_count: 2,
        raw_samples_retention: "14 days".into(),
        session_epochs_retention: "indefinite".into(),
        calibration_maturity: CalibrationMaturity::Calibrated,
        active_calibration_boundary: Some(CalibrationBoundaryRecord {
            id: "bnd-001".into(),
            effective_ts_ms: 1_700_000_000_000,
            reason: BoundaryReason::ManualRebaseline,
            seed_sleep_hr: Some(56.0),
            reset_floors: false,
            created_at_ms: 1_700_000_000_000,
            reverted_at_ms: None,
        }),
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    // 1. Verify Core Preferences display
    assert!(out.contains("Sensitivity: Balanced") || out.contains("Balanced"));
    assert!(out.contains("Pause Media") || out.contains("Media: ON"));
    assert!(out.contains("Lock Screen") || out.contains("Lock: ON"));
    assert!(out.contains("Suspend") || out.contains("Action: Suspend"));
    assert!(out.contains("30s") || out.contains("Grace: 30s"));
    assert!(out.contains("Dry Run: OFF") || out.contains("Dry Run"));

    // 2. Verify Advanced Settings read-only display + $EDITOR handoff hint
    assert!(out.contains("pkill") || out.contains("custom_command"));
    assert!(out.contains("firefox") || out.contains("ignore_players"));
    assert!(out.contains("hci0") || out.contains("adapter"));
    assert!(out.contains("[E] Editor") || out.contains("[E] Edit in $EDITOR"));

    // 3. Verify Calibration & Storage statistics display
    assert!(out.contains("142 KB"));
    assert!(out.contains("12") && out.contains("sessions"));
    assert!(out.contains("1420") || out.contains("epochs"));
    assert!(out.contains("8500") || out.contains("raw"));
    assert!(out.contains("Calibrated"));
    assert!(out.contains("[U] Undo Boundary") || out.contains("[U] Undo"));

    // 4. Test live toggle on core preference
    let toggle_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char(' '))));
    assert!(matches!(toggle_cmd, Some(TuiCommand::ConfigUpdate(_))));

    // 5. Test [E] launches editor handoff
    let editor_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('E'))));
    assert_eq!(editor_cmd, Some(TuiCommand::EditorHandoff));

    // 6. Test inline validation error display when config_error is present
    app.config_error = Some("unknown field `bad_key` in config.toml".into());
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_err = buffer_to_string(terminal.backend().buffer());
    assert!(out_err.contains("unknown field `bad_key`"));
    assert!(out_err.contains("preserved") || out_err.contains("active config preserved"));
    app.config_error = None;

    // 7. Test Soft Re-baseline modal [B]
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('B')))),
        None
    );
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));
    // Type seed 54.0
    for c in "54.0".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    // Tab to reset_floors checkbox and toggle with Space
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char(' '))));
    // Tab to Submit and Enter
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    let rebase_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        rebase_cmd,
        Some(TuiCommand::CalibrationSetBoundary {
            effective_ts_ms: None,
            seed_sleep_hr: Some(54.0),
            reset_floors: true,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 8. Test Purge Raw Samples modal [P]
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('P')))),
        None
    );
    assert!(matches!(app.modal, ModalState::PurgeRawModal { .. }));
    let purge_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(purge_cmd, Some(TuiCommand::DataPurgeRaw));
    assert_eq!(app.modal, ModalState::None);

    // 9. Test Hard Wipe modal requiring typed "WIPE" [W]
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('W')))),
        None
    );
    assert!(matches!(app.modal, ModalState::HardWipeModal { .. }));

    // Typing wrong text "NOPE" -> Enter fails validation
    for c in "NOPE".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    if let ModalState::HardWipeModal {
        ref error_message, ..
    } = app.modal
    {
        assert!(error_message.is_some());
    } else {
        panic!("HardWipeModal must stay open on invalid confirmation string");
    }

    // Clear and type "WIPE"
    for _ in 0..4 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for c in "WIPE".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let wipe_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        wipe_cmd,
        Some(TuiCommand::DataHardWipe {
            confirm: "WIPE".into(),
            wipe_all: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 10. Test Undo Boundary [U]
    let undo_cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('U'))));
    assert_eq!(undo_cmd, Some(TuiCommand::CalibrationUndoBoundary));
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

struct LiveStreamTestSource {
    event_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<WearableEvent>>>,
}

impl WearableSource for LiveStreamTestSource {
    fn subscribe(&self, mut shutdown_rx: watch::Receiver<bool>) -> mpsc::Receiver<WearableEvent> {
        let (out_tx, out_rx) = mpsc::channel(64);
        let rx_arc = Arc::clone(&self.event_rx);
        tokio::spawn(async move {
            let mut guard = rx_arc.lock().await;
            loop {
                tokio::select! {
                    ev = guard.recv() => {
                        match ev {
                            Some(event) => {
                                if out_tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        out_rx
    }
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_wearable_stream_status_and_disconnect_updates_live_tui() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 20, 18, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));

    let (w_tx, w_rx) = mpsc::channel(32);
    let source = Arc::new(LiveStreamTestSource {
        event_rx: Arc::new(tokio::sync::Mutex::new(w_rx)),
    });

    let daemon = DaemonHandle::spawn(
        DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock)
            .with_wearable_source(source),
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
    daemon
        .core()
        .persistence
        .upsert_wearable(desc.clone(), start_utc.timestamp_millis())
        .await
        .unwrap();

    let (event_tx, mut event_rx) = mpsc::channel(256);
    let (_cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.daemon_connected && !a.enrolled_wearables.is_empty()
    })
    .await;
    assert_eq!(
        app.active_wearable().map(|w| w.status),
        Some(StreamStatus::Disconnected)
    );

    let t0 = start_utc.timestamp_millis();
    w_tx.send(WearableEvent::StreamState {
        wearable: desc.clone(),
        state: StreamState {
            status: StreamStatus::Streaming,
            ts_ms: t0 + 1_000,
        },
    })
    .await
    .unwrap();
    w_tx.send(WearableEvent::HrPacket {
        wearable: desc.clone(),
        packet: HrPacket {
            ts_ms: t0 + 1_000,
            bpm: 65.0,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
        },
    })
    .await
    .unwrap();

    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.active_wearable()
            .is_some_and(|w| w.status == StreamStatus::Streaming && a.latest_bpm == Some(65.0))
    })
    .await;

    // Watch stops HR broadcast -> daemon emits Disconnected
    w_tx.send(WearableEvent::StreamState {
        wearable: desc,
        state: StreamState {
            status: StreamStatus::Disconnected,
            ts_ms: t0 + 5_000,
        },
    })
    .await
    .unwrap();

    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.active_wearable()
            .is_some_and(|w| w.status == StreamStatus::Disconnected)
    })
    .await;
    assert_eq!(app.latest_bpm, None);

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon.shutdown().await;
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
        "30-Minute Heart Rate Sparkline",
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
            "5. Pause until next morning",
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
fn test_mouse_wearables_table_row_click_and_overflow_scroll() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    app.enrolled_wearables = (0..15)
        .map(|i| {
            make_wearable(
                &format!("w-{i:02}"),
                &format!("AA:BB:CC:11:22:{i:02}"),
                Some(&format!("Strap {i:02}")),
                if i == 0 {
                    StreamStatus::Streaming
                } else {
                    StreamStatus::Disconnected
                },
                Some(90u8.saturating_sub(i as u8)),
                None,
            )
        })
        .collect();

    // At 60x18, Enrolled Wearables table viewport holds ~10 data rows (< 15), causing overflow
    let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    let initial_out = buffer_to_string(buf);
    assert!(initial_out.contains("Strap 00"));
    assert!(!initial_out.contains("Strap 14"));

    // Clicking table header row must not alter selection
    let (hdr_col, hdr_row) = find_text_coords(buf, "Alias / Name").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(hdr_col + 1, hdr_row)),
        None
    );
    assert_eq!(app.selected_wearable_idx, 0);

    // Clicking visible row 3 ("Strap 03") selects index 3 without scrolling away
    let (r3_col, r3_row) = find_text_coords(buf, "Strap 03").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(r3_col + 1, r3_row)), None);
    assert_eq!(app.selected_wearable_idx, 3);

    // Wheel scrolling down advances selection and scrolls overflowed rows into view
    for _ in 0..11 {
        app.handle_event(mouse_scroll_down(r3_col + 1, r3_row));
    }
    assert_eq!(app.selected_wearable_idx, 14);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let scrolled_out = buffer_to_string(terminal.backend().buffer());
    assert!(
        scrolled_out.contains("Strap 14"),
        "Bottom wearable must scroll into viewport when selected"
    );
    assert!(
        !scrolled_out.contains("Strap 00"),
        "Top wearable must scroll out of viewport when scrolled to bottom"
    );

    // Extra ScrollDown at bottom clamps cleanly without overshoot
    app.handle_event(mouse_scroll_down(r3_col + 1, r3_row));
    assert_eq!(app.selected_wearable_idx, 14);

    // Clicking a scrolled-into-view row ("Strap 12") selects its true index (12)
    let (r12_col, r12_row) = find_text_coords(terminal.backend().buffer(), "Strap 12").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(r12_col + 1, r12_row)),
        None
    );
    assert_eq!(app.selected_wearable_idx, 12);

    // Single ScrollUp immediately steps from 12 -> 11, and keyboard 'k' back to 0 brings Strap 00 back
    app.handle_event(mouse_scroll_up(r12_col + 1, r12_row));
    assert_eq!(app.selected_wearable_idx, 11);
    for _ in 0..11 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char('k'))));
    }
    assert_eq!(app.selected_wearable_idx, 0);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Strap 00"));
}

#[test]
fn test_mouse_wearables_action_bar_buttons_and_offline_guards() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Disconnected,
        Some(80),
        None,
    )];

    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    let (e_col, bar_row) = find_text_coords(buf, "[e] Enroll new").unwrap();
    assert!(bar_row < 23, "Action bar should sit above the footer");
    let (n_col, _) = find_text_coords_in_row(buf, bar_row, "[n] Rename alias").unwrap();
    let (c_col, _) = find_text_coords_in_row(buf, bar_row, "[c] Reconnect").unwrap();
    let (u_col, _) = find_text_coords_in_row(buf, bar_row, "[u] Unenroll").unwrap();

    // [c] Reconnect dispatches WearableConnect for selected wearable
    assert_eq!(
        app.handle_event(mouse_left_click(c_col + 5, bar_row)),
        Some(TuiCommand::WearableConnect { id: "w-1".into() })
    );

    // [n] Rename alias opens RenameModal
    assert_eq!(app.handle_event(mouse_left_click(n_col + 5, bar_row)), None);
    assert!(matches!(
        app.modal,
        ModalState::RenameModal {
            ref wearable_id,
            ..
        } if wearable_id == "w-1"
    ));
    app.modal = ModalState::None;

    // [u] Unenroll opens UnenrollModal
    assert_eq!(app.handle_event(mouse_left_click(u_col + 4, bar_row)), None);
    assert!(matches!(app.modal, ModalState::UnenrollModal { .. }));
    app.modal = ModalState::None;

    // [e] Enroll new opens EnrollmentModal and dispatches EnrollStart
    assert_eq!(
        app.handle_event(mouse_left_click(e_col + 4, bar_row)),
        Some(TuiCommand::EnrollStart(Some(30)))
    );
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));
    app.modal = ModalState::None;

    // Online with empty enrolled_wearables: [e] works, [n]/[c]/[u] are ignored
    app.enrolled_wearables.clear();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(app.handle_event(mouse_left_click(n_col + 1, bar_row)), None);
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(app.handle_event(mouse_left_click(c_col + 1, bar_row)), None);
    assert_eq!(app.handle_event(mouse_left_click(u_col + 1, bar_row)), None);
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(mouse_left_click(e_col + 1, bar_row)),
        Some(TuiCommand::EnrollStart(Some(30)))
    );
    app.modal = ModalState::None;

    // Offline guard: all action bar buttons and offline notice ignore clicks
    app.daemon_connected = false;
    app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Disconnected,
        Some(80),
        None,
    )];
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let off_buf = terminal.backend().buffer();
    let (off_col, off_row) = find_text_coords(off_buf, "(offline: read-only)").unwrap();
    for col in [e_col + 1, n_col + 1, c_col + 1, u_col + 1, off_col + 1] {
        assert_eq!(app.handle_event(mouse_left_click(col, off_row)), None);
        assert_eq!(app.modal, ModalState::None);
    }
}

#[test]
fn test_mouse_enrollment_modal_scroll_two_click_alias_focus_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    app.open_enrollment_modal();
    app.discovered_wearables = (0..12)
        .map(|i| DiscoveredWearable {
            address: format!("AA:BB:CC:DD:EE:{i:02}"),
            address_type: "public".into(),
            name: Some(format!("HRM-{i:02}")),
            rssi: Some(-50 - i as i16),
            paired: i % 2 == 0,
            already_enrolled: false,
            uuids: vec![],
        })
        .collect();

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let initial_out = buffer_to_string(terminal.backend().buffer());
    assert!(initial_out.contains("HRM-00"));
    assert!(!initial_out.contains("HRM-11"));

    // 1. Clicking backdrop outside Enrollment modal box must NOT dismiss it
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 10)), None);
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));

    // 2. Wheel scroll outside discovered list does not scroll; inside discovered list scrolls overflow
    let (hdr_col, hdr_row) =
        find_text_coords(terminal.backend().buffer(), "Select a discovered").unwrap();
    app.handle_event(mouse_scroll_down(hdr_col + 1, hdr_row));
    let (d0_col, d0_row) = find_text_coords(terminal.backend().buffer(), "HRM-00").unwrap();
    for _ in 0..11 {
        app.handle_event(mouse_scroll_down(d0_col + 1, d0_row));
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let scrolled_out = buffer_to_string(terminal.backend().buffer());
    assert!(
        scrolled_out.contains("HRM-11"),
        "Long scan list must scroll bottom wearable into view"
    );
    assert!(!scrolled_out.contains("HRM-00"));

    // Wheel overshoot clamps cleanly and single ScrollUp reverses immediately
    app.handle_event(mouse_scroll_down(d0_col + 1, d0_row));
    app.handle_event(mouse_scroll_up(d0_col + 1, d0_row));
    if let ModalState::EnrollmentModal {
        selected_discovered_idx,
        ..
    } = app.modal
    {
        assert_eq!(selected_discovered_idx, 10);
    } else {
        panic!("Expected EnrollmentModal");
    }

    // Scroll back up to top
    for _ in 0..10 {
        app.handle_event(mouse_scroll_up(d0_col + 1, d0_row));
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("HRM-00"));

    // 3. Clicking Custom Alias focuses text input and suspends global hotkeys
    let (alias_col, alias_row) =
        find_text_coords(terminal.backend().buffer(), "Custom Alias").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(alias_col + 2, alias_row + 1)),
        None
    );
    assert!(app.is_input_focused());
    for c in ['q', 'r', '1'] {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    assert!(!app.should_quit);
    assert_eq!(app.active_tab, Tab::Wearables);

    // Clicking a discovered row (even row 0!) while Custom Alias is focused unfocuses alias and selects row without enrolling
    let (d0_col, d0_row) = find_text_coords(terminal.backend().buffer(), "HRM-00").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(d0_col + 1, d0_row)), None);
    assert!(!app.is_input_focused());
    assert!(matches!(
        app.modal,
        ModalState::EnrollmentModal {
            selected_discovered_idx: 0,
            alias_focused: false,
            enrolling: false,
            ..
        }
    ));

    // Clicking another row (row 2) selects row 2 without enrolling; clicking row 2 again initiates EnrollSelect
    let (d2_col, d2_row) = find_text_coords(terminal.backend().buffer(), "HRM-02").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(d2_col + 1, d2_row)), None);
    assert!(matches!(
        app.modal,
        ModalState::EnrollmentModal {
            selected_discovered_idx: 2,
            enrolling: false,
            ..
        }
    ));
    assert_eq!(
        app.handle_event(mouse_left_click(d2_col + 1, d2_row)),
        Some(TuiCommand::EnrollSelect {
            address: "AA:BB:CC:DD:EE:02".into(),
            alias: Some("qr1".into()),
        })
    );
    assert!(matches!(
        app.modal,
        ModalState::EnrollmentModal {
            enrolling: true,
            ..
        }
    ));

    // While enrolling: true, clicking discovered rows, Custom Alias, [Enter] Enroll, or [r] Rescan is ignored; [Esc] Cancel cancels
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (m_enter_col, m_enter_row) =
        find_text_coords(terminal.backend().buffer(), "[Enter] Enroll").unwrap();
    assert!(m_enter_row < 23);
    let (m_rescan_col, m_rescan_row) =
        find_text_coords(terminal.backend().buffer(), "[r] Rescan").unwrap();
    assert!(m_rescan_row < 23);
    let (m_esc_col, m_esc_row) =
        find_text_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert!(m_esc_row < 23);

    assert_eq!(
        app.handle_event(mouse_left_click(m_enter_col + 1, m_enter_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(m_rescan_col + 1, m_rescan_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(m_esc_col + 1, m_esc_row)),
        Some(TuiCommand::EnrollCancel)
    );
    assert_eq!(app.modal, ModalState::None);

    // 4. Reopen EnrollmentModal: clicking [r] Rescan / [Enter] Enroll clears alias_focused and triggers action
    app.open_enrollment_modal();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (alias_col, alias_row) =
        find_text_coords(terminal.backend().buffer(), "Custom Alias").unwrap();
    app.handle_event(mouse_left_click(alias_col + 1, alias_row + 1));
    assert!(app.is_input_focused());

    // Clicking empty wearable list also clears alias_focused
    let saved_discovered = std::mem::take(&mut app.discovered_wearables);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (empty_col, empty_row) =
        find_text_coords(terminal.backend().buffer(), "Searching for advertising").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(empty_col + 2, empty_row)),
        None
    );
    assert!(!app.is_input_focused());

    // Refocus alias, click [r] Rescan -> clears alias_focused and sends EnrollStart
    app.handle_event(mouse_left_click(alias_col + 1, alias_row + 1));
    assert!(app.is_input_focused());
    let (m_rescan_col, m_rescan_row) =
        find_text_coords(terminal.backend().buffer(), "[r] Rescan").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(m_rescan_col + 1, m_rescan_row)),
        Some(TuiCommand::EnrollStart(Some(30)))
    );
    assert!(!app.is_input_focused());

    // Restore discovered list, refocus alias, click in-modal [Enter] Enroll -> clears alias_focused and initiates EnrollSelect
    app.discovered_wearables = saved_discovered;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    app.handle_event(mouse_left_click(alias_col + 1, alias_row + 1));
    assert!(app.is_input_focused());
    let (m_enter_col, m_enter_row) =
        find_text_coords(terminal.backend().buffer(), "[Enter] Enroll").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(m_enter_col + 1, m_enter_row)),
        Some(TuiCommand::EnrollSelect {
            address: "AA:BB:CC:DD:EE:00".into(),
            alias: None,
        })
    );
    assert!(!app.is_input_focused());
}

#[test]
fn test_mouse_agent1_pairing_prompts_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // 1. Confirmation prompt: [y], [n], [Esc], and backdrop no-dismiss
    app.modal = ModalState::EnrollmentModal {
        scanning_remaining_ms: 20_000,
        selected_discovered_idx: 0,
        alias_input: String::new(),
        alias_focused: false,
        enrolling: true,
        error_message: None,
        pairing_prompt: Some(PairingPromptModalState::Confirmation {
            passkey: 482_910,
            wearable: "Polar H10".into(),
        }),
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    // Backdrop click does not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));

    // Clicking [y] dispatches EnrollAuthReply(confirmed: Some(true))
    let (y_col, y_row) = find_text_coords(buf, "[y]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(y_col + 1, y_row)),
        Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
            confirmed: Some(true),
            passkey: None,
            pincode: None,
        }))
    );

    // Re-set Confirmation, click [n] -> confirmed: Some(false)
    if let ModalState::EnrollmentModal {
        ref mut pairing_prompt,
        ..
    } = app.modal
    {
        *pairing_prompt = Some(PairingPromptModalState::Confirmation {
            passkey: 482_910,
            wearable: "Polar H10".into(),
        });
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (n_col, n_row) = find_text_coords(terminal.backend().buffer(), "[n]/[Esc]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(n_col + 1, n_row)),
        Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
            confirmed: Some(false),
            passkey: None,
            pincode: None,
        }))
    );

    // Re-set Confirmation, click [Esc] inside modal -> closes modal and dispatches EnrollCancel
    if let ModalState::EnrollmentModal {
        ref mut pairing_prompt,
        ..
    } = app.modal
    {
        *pairing_prompt = Some(PairingPromptModalState::Confirmation {
            passkey: 482_910,
            wearable: "Polar H10".into(),
        });
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (ne_col, ne_row) = find_text_coords(terminal.backend().buffer(), "[n]/[Esc]").unwrap();
    assert!(ne_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(ne_col + 5, ne_row)),
        Some(TuiCommand::EnrollCancel)
    );
    assert_eq!(app.modal, ModalState::None);

    // 2. Passkey prompt: typing + [Enter] / [Esc] + backdrop no-dismiss
    app.modal = ModalState::EnrollmentModal {
        scanning_remaining_ms: 20_000,
        selected_discovered_idx: 0,
        alias_input: String::new(),
        alias_focused: false,
        enrolling: true,
        error_message: None,
        pairing_prompt: Some(PairingPromptModalState::Passkey {
            wearable: "Polar H10".into(),
            input: String::new(),
        }),
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    for c in "123456".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let (p_enter_col, p_enter_row) =
        find_text_coords(terminal.backend().buffer(), "[Enter] to submit").unwrap();
    assert!(p_enter_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(p_enter_col + 1, p_enter_row)),
        Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
            confirmed: Some(true),
            passkey: Some(123_456),
            pincode: None,
        }))
    );

    // 3. PinCode prompt: typing + [Esc]
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
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    for c in "0042".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let (pin_enter_col, pin_enter_row) =
        find_text_coords(terminal.backend().buffer(), "[Enter] to submit").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(pin_enter_col + 1, pin_enter_row)),
        Some(TuiCommand::EnrollAuthReply(EnrollAuthResponseParams {
            confirmed: Some(true),
            passkey: None,
            pincode: Some("0042".into()),
        }))
    );

    if let ModalState::EnrollmentModal {
        ref mut pairing_prompt,
        ..
    } = app.modal
    {
        *pairing_prompt = Some(PairingPromptModalState::PinCode {
            wearable: "Polar H10".into(),
            input: String::new(),
        });
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (pin_esc_col, pin_esc_row) =
        find_text_coords(terminal.backend().buffer(), "[Esc] to cancel").unwrap();
    assert!(pin_esc_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(pin_esc_col + 1, pin_esc_row)),
        Some(TuiCommand::EnrollCancel)
    );
    assert_eq!(app.modal, ModalState::None);
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_mouse_enrollment_and_agent1_pairing_e2e() {
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

    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();

    // 1. Click [3:Wearables] tab
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (tab_col, tab_row) =
        find_text_coords(terminal.backend().buffer(), "[3:Wearables]").unwrap();
    app.handle_event(mouse_left_click(tab_col + 1, tab_row));
    assert_eq!(app.active_tab, Tab::Wearables);

    // 2. Click [e] Enroll new in the Wearables action bar
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (enroll_btn_col, enroll_btn_row) =
        find_text_coords(terminal.backend().buffer(), "[e] Enroll new").unwrap();
    let start_cmd = app
        .handle_event(mouse_left_click(enroll_btn_col + 1, enroll_btn_row))
        .unwrap();
    cmd_tx.send(start_cmd).await.unwrap();

    wait_for_app_state(&mut app, &mut event_rx, |a| {
        !a.discovered_wearables.is_empty()
    })
    .await;

    // 3. Render EnrollmentModal, focus Custom Alias, type alias, unfocus via row click, and two-click EnrollSelect
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (alias_col, alias_row) =
        find_text_coords(terminal.backend().buffer(), "Custom Alias").unwrap();
    app.handle_event(mouse_left_click(alias_col + 1, alias_row + 1));
    for c in "Night Strap".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let (dev_col, dev_row) = find_text_coords(terminal.backend().buffer(), "Polar H10").unwrap();
    // First click unfocuses Custom Alias and selects row 0
    assert_eq!(
        app.handle_event(mouse_left_click(dev_col + 1, dev_row)),
        None
    );
    assert!(!app.is_input_focused());
    // Second click on already-selected row initiates EnrollSelect
    let select_cmd = app
        .handle_event(mouse_left_click(dev_col + 1, dev_row))
        .unwrap();
    cmd_tx.send(select_cmd).await.unwrap();

    // 4. Wait for Agent1 Confirmation prompt pushed over real Unix socket
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

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (y_col, y_row) = find_text_coords(terminal.backend().buffer(), "[y]").unwrap();
    let reply_cmd = app
        .handle_event(mouse_left_click(y_col + 1, y_row))
        .unwrap();
    cmd_tx.send(reply_cmd).await.unwrap();

    // 5. Enrollment finishes and newly enrolled Wearable with custom alias appears in Wearables table
    wait_for_app_state(&mut app, &mut event_rx, |a| {
        a.modal == ModalState::None && a.enrolled_wearables.len() == 1
    })
    .await;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Night Strap"));

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon.shutdown().await;
}

#[test]
fn test_mouse_rename_and_unenroll_modals_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;
    let w = make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Old Alias"),
        StreamStatus::Disconnected,
        Some(85),
        None,
    );
    app.enrolled_wearables = vec![w];

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // 1. RenameModal: backdrop no-dismiss, alias input focus, [Enter] Save, [Esc] Cancel
    app.modal = ModalState::RenameModal {
        wearable_id: "w-1".into(),
        current_alias: "Old Alias".into(),
        input: "Old".into(),
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let r_buf = terminal.backend().buffer();

    // Backdrop click does not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::RenameModal { .. }));

    // Clicking alias input focuses it and typing appends
    let (in_col, in_row) = find_text_coords(r_buf, "New alias:").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(in_col + 12, in_row)),
        None
    );
    assert!(app.is_input_focused());
    for c in " Strap".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // Clicking in-modal [Enter] Save executes immediately
    let (save_col, save_row) = find_text_coords(r_buf, "[Enter] Save").unwrap();
    assert!(save_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(save_col + 1, save_row)),
        Some(TuiCommand::WearableRename {
            id: "w-1".into(),
            alias: Some("Old Strap".into()),
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // Reopen RenameModal and click in-modal [Esc] Cancel -> closes immediately without command
    app.modal = ModalState::RenameModal {
        wearable_id: "w-1".into(),
        current_alias: "Old Alias".into(),
        input: "Draft".into(),
    };
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (esc_col, esc_row) = find_text_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert!(esc_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(esc_col + 1, esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    // 2. UnenrollModal: backdrop no-dismiss, [1]/[2] checkboxes, [ Confirm ] / [ Cancel ] buttons
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('u'))));
    assert!(matches!(app.modal, ModalState::UnenrollModal { .. }));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let u_buf = terminal.backend().buffer();

    // Backdrop click does not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::UnenrollModal { .. }));

    // Clicking [1] and [2] checkboxes toggles forget_bluez and delete_sessions
    let (cb1_col, cb1_row) = find_text_coords(u_buf, "[1] Also forget from BlueZ").unwrap();
    let (cb2_col, cb2_row) = find_text_coords(u_buf, "[2] Also delete historical").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(cb1_col + 1, cb1_row)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_left_click(cb2_col + 1, cb2_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::UnenrollModal {
            forget_bluez: true,
            delete_sessions: true,
            ..
        }
    ));

    // Toggle [2] back off
    assert_eq!(
        app.handle_event(mouse_left_click(cb2_col + 1, cb2_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::UnenrollModal {
            forget_bluez: true,
            delete_sessions: false,
            ..
        }
    ));

    // Even when selected_button is Cancel, clicking [ Confirm ] executes unenroll immediately
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert!(matches!(
        app.modal,
        ModalState::UnenrollModal {
            selected_button: UnenrollButton::Cancel,
            ..
        }
    ));
    let (conf_col, conf_row) = find_text_coords(u_buf, "[ Confirm ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(conf_col + 2, conf_row)),
        Some(TuiCommand::WearableUnenroll {
            id: "w-1".into(),
            forget_bluez: true,
            delete_sessions: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // Reopen UnenrollModal and click [ Cancel ] -> cancels immediately even when Confirm is highlighted
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('u'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (canc_col, canc_row) = find_text_coords(terminal.backend().buffer(), "[ Cancel ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(canc_col + 2, canc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

fn make_history_session(
    id: &str,
    alias: &str,
    start_ts_ms: i64,
    end_ts_ms: i64,
    status: SessionStatus,
    epochs_count: u32,
) -> SessionListEntry {
    SessionListEntry {
        id: id.into(),
        wearable_id: "w-1".into(),
        wearable_alias: Some(alias.into()),
        wearable_mac_masked: "..:22:33".into(),
        status,
        start_ts_ms,
        end_ts_ms,
        end_reason: (status == SessionStatus::Closed).then_some(SessionEndReason::StreamEnded),
        detection_sensitivity: bpmute::config::Sensitivity::Balanced,
        had_rr: true,
        warmup_mu_hr: Some(70.0),
        sleep_mu_hr: (status == SessionStatus::Closed).then_some(56.0),
        onset_confirmed_ts_ms: None,
        sleeping_ts_ms: None,
        auto_anomalous: false,
        fp_count: 0,
        epochs_count,
        raw_samples_count: 0,
        feedback: None,
    }
}

#[test]
fn test_mouse_history_master_list_click_online_offline_and_boundary_guard() {
    // 1. Online mode: clicking Calibration Boundary is ignored; clicking Monitoring Session line 1/2 selects and emits SessionGet
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let s0 = make_history_session(
        "sess-newest",
        "Polar H10",
        1_700_200_000_000,
        1_700_203_600_000,
        SessionStatus::Closed,
        24,
    );
    let s1 = make_history_session(
        "sess-older",
        "Garmin Strap",
        1_700_100_000_000,
        1_700_103_600_000,
        SessionStatus::Closed,
        20,
    );
    let boundary = CalibrationBoundaryRecord {
        id: "bnd-1".into(),
        effective_ts_ms: 1_700_150_000_000,
        reason: BoundaryReason::ManualRebaseline,
        seed_sleep_hr: Some(55.0),
        reset_floors: false,
        created_at_ms: 1_700_150_000_000,
        reverted_at_ms: None,
    };
    app.history_sessions = vec![s0, s1];
    app.history_boundaries = vec![boundary];
    app.selected_session_idx = 0;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // Clicking Calibration Boundary divider row does nothing
    let (bnd_col, bnd_row) =
        find_text_coords(terminal.backend().buffer(), "Boundary: Rebaseline").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(bnd_col, bnd_row)), None);
    assert_eq!(app.selected_session_idx, 0);

    // Clicking Monitoring Session 1 (line 2: wearable label) selects idx 1 and emits SessionGet
    let (s1_col, s1_row) = find_text_coords(terminal.backend().buffer(), "Garmin Strap").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(s1_col, s1_row)),
        Some(TuiCommand::SessionGet {
            id: "sess-older".into(),
        })
    );
    assert_eq!(app.selected_session_idx, 1);

    // Clicking Monitoring Session 0 line 1 (above "Polar H10") selects idx 0 and emits SessionGet
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (s0_col, s0_row) = find_text_coords(terminal.backend().buffer(), "Polar H10").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(s0_col, s0_row - 1)),
        Some(TuiCommand::SessionGet {
            id: "sess-newest".into(),
        })
    );
    assert_eq!(app.selected_session_idx, 0);

    // 2. Offline mode: clicking session row loads detail from local SQLite storage without SessionGet
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);
    fs::create_dir_all(&paths.data_dir).unwrap();
    let (off_s1_id, off_s2_id) = {
        let db_file = bpmute::storage::db_path_in_data_dir(&paths.data_dir);
        let db_lock = bpmute::storage::lock_path_in_data_dir(&paths.data_dir);
        let mut store = bpmute::storage::LedgerStore::open_read_write(&db_file, &db_lock).unwrap();
        let wid = store
            .upsert_wearable(
                &bpmute::storage::WearableDescriptor {
                    mac: "AA:BB:CC:DD:EE:01".into(),
                    address_type: "public".into(),
                    alias: Some("OfflineBand".into()),
                    advertised_name: None,
                    supports_rr: true,
                    supports_contact: true,
                },
                1_700_000_000_000,
            )
            .unwrap();
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
                        epochs_count: 10,
                    },
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                1_700_000_500_000,
            )
            .unwrap();

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
                        epochs_count: 12,
                    },
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                1_700_010_600_000,
            )
            .unwrap();
        (s1_id, s2_id)
    };

    let mut offline_app = TuiApp::new(false, false, false);
    offline_app.set_offline_paths(paths);
    offline_app.load_offline_data(&env);
    offline_app.active_tab = Tab::History;
    assert_eq!(offline_app.selected_session_idx, 0);
    assert_eq!(
        offline_app
            .selected_session_detail
            .as_ref()
            .map(|d| d.session.id.as_str()),
        Some(off_s2_id.as_str())
    );

    terminal
        .draw(|f| offline_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    // OfflineBand appears twice (newest idx 0 on top, older idx 1 below it); click the second row
    let buf = terminal.backend().buffer();
    let mut band_rows = Vec::new();
    for row in 0..buf.area.height {
        if let Some((col, r)) = find_text_coords_in_row(buf, row, "OfflineBand")
            && col < 34
        {
            band_rows.push((col, r));
        }
    }
    assert_eq!(band_rows.len(), 2);
    let (click_col, click_row) = band_rows[1];
    assert_eq!(
        offline_app.handle_event(mouse_left_click(click_col, click_row)),
        None
    );
    assert_eq!(offline_app.selected_session_idx, 1);
    assert_eq!(
        offline_app
            .selected_session_detail
            .as_ref()
            .map(|d| d.session.id.as_str()),
        Some(off_s1_id.as_str())
    );
}

#[test]
fn test_mouse_history_wheel_scoping_and_overflow_scroll() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    app.history_sessions = (0..14)
        .map(|i| {
            make_history_session(
                &format!("s-{i:02}"),
                &format!("Band-{i:02}"),
                1_700_300_000_000 - (i as i64) * 100_000,
                1_700_303_600_000 - (i as i64) * 100_000,
                SessionStatus::Closed,
                20,
            )
        })
        .collect();
    app.selected_session_idx = 0;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let initial_buf = buffer_to_string(terminal.backend().buffer());
    assert!(initial_buf.contains("Band-00"));
    assert!(!initial_buf.contains("Band-13"));

    // 1. Wheel over right Epoch Detail & Chart pane does nothing (no scroll, no SessionGet)
    let (detail_col, detail_row) =
        find_text_coords(terminal.backend().buffer(), "Epoch Detail & Chart").unwrap();
    assert_eq!(
        app.handle_event(mouse_scroll_down(detail_col + 2, detail_row + 4)),
        None
    );
    assert_eq!(
        app.handle_event(mouse_scroll_up(detail_col + 2, detail_row + 4)),
        None
    );
    assert_eq!(app.selected_session_idx, 0);

    // 2. Wheel ScrollUp at top bound (idx 0) over left master list is a no-op
    let (list_col, list_row) =
        find_text_coords(terminal.backend().buffer(), "Monitoring Sessions").unwrap();
    assert_eq!(
        app.handle_event(mouse_scroll_up(list_col + 1, list_row + 2)),
        None
    );
    assert_eq!(app.selected_session_idx, 0);

    // 3. Wheel ScrollDown over left master list navigates and scrolls Band-13 into view
    for expected in 1..14 {
        assert_eq!(
            app.handle_event(mouse_scroll_down(list_col + 1, list_row + 2)),
            Some(TuiCommand::SessionGet {
                id: format!("s-{expected:02}"),
            })
        );
        assert_eq!(app.selected_session_idx, expected);
    }
    // Scrolling past end is a no-op
    assert_eq!(
        app.handle_event(mouse_scroll_down(list_col + 1, list_row + 2)),
        None
    );
    assert_eq!(app.selected_session_idx, 13);

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let scrolled_buf = buffer_to_string(terminal.backend().buffer());
    assert!(scrolled_buf.contains("Band-13"));
    assert!(!scrolled_buf.contains("Band-00"));

    // 4. Wheel ScrollUp scrolls back up to Band-00
    for expected in (0..13).rev() {
        assert_eq!(
            app.handle_event(mouse_scroll_up(list_col + 1, list_row + 2)),
            Some(TuiCommand::SessionGet {
                id: format!("s-{expected:02}"),
            })
        );
        assert_eq!(app.selected_session_idx, expected);
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let top_buf = buffer_to_string(terminal.backend().buffer());
    assert!(top_buf.contains("Band-00"));
    assert!(!top_buf.contains("Band-13"));
}

#[test]
fn test_mouse_history_feedback_modal_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let s_with_epochs = make_history_session(
        "sess-fb-1",
        "Polar H10",
        1_700_100_000_000,
        1_700_103_600_000,
        SessionStatus::Closed,
        24,
    );
    let s_pruned = make_history_session(
        "sess-fb-pruned",
        "Polar H10",
        1_700_050_000_000,
        1_700_053_600_000,
        SessionStatus::Closed,
        0,
    );
    app.history_sessions = vec![s_with_epochs.clone(), s_pruned];
    app.selected_session_idx = 0;

    // Open FeedbackModal via [f]
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('f'))));
    assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Backdrop clicks do not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));

    // 2. Click Wearable Onset -> focuses Onset input, type invalid time first
    let (onset_col, onset_row) =
        find_text_coords(terminal.backend().buffer(), "[ HH:MM ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(onset_col + 2, onset_row)),
        None
    );
    assert!(app.is_input_focused());
    for c in "99:99".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // 3. Click Avg Sleep HR -> focuses SleepHr input, type "57.5"
    let (hr_col, hr_row) = find_text_coords(terminal.backend().buffer(), "[ 30–120 ]").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(hr_col + 2, hr_row)), None);
    assert!(app.is_input_focused());
    for c in "57.5".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // 4. Click '<' on Verdict -> focuses Verdict (unfocusing text input) and steps backward to Anomalous (Exclude)
    let (v_col, v_row) = find_text_coords(terminal.backend().buffer(), "< None >").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(v_col, v_row)), None);
    assert!(!app.is_input_focused());
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("< Anomalous (Exclude) >"));

    // 5. Click middle of Verdict -> steps forward to None
    let (mid_col, mid_row) =
        find_text_coords(terminal.backend().buffer(), "< Anomalous (Exclude) >").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(mid_col + 5, mid_row)),
        None
    );
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("< None >"));

    // 6. Click '>' of Verdict -> steps forward to False Positive
    let (gt_col, gt_row) = find_text_coords(terminal.backend().buffer(), "< None >").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(gt_col + 7, gt_row)), None);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("< False Positive >"));

    // 7. Click [ Submit ] with invalid onset -> stays open with Error:
    let (sub_col, sub_row) = find_text_coords(terminal.backend().buffer(), "[ Submit ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(sub_col + 2, sub_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::FeedbackModal {
            error_message: Some(_),
            ..
        }
    ));

    // Fix onset input via click and valid HH:MM inside session window
    let (onset_val_col, onset_val_row) =
        find_text_coords(terminal.backend().buffer(), "[ 99:99 ]").unwrap();
    app.handle_event(mouse_left_click(onset_val_col + 2, onset_val_row));
    for _ in 0..5 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    let valid_hhmm = chrono::Local
        .timestamp_millis_opt(s_with_epochs.start_ts_ms + 1_800_000)
        .single()
        .unwrap()
        .format("%H:%M")
        .to_string();
    for c in valid_hhmm.chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // Re-render and click [ Submit ] -> validates and emits SessionSetFeedback
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (sub_col2, sub_row2) = find_text_coords(terminal.backend().buffer(), "[ Submit ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(sub_col2 + 2, sub_row2)),
        Some(TuiCommand::SessionSetFeedback {
            session_id: "sess-fb-1".into(),
            verdict: Some(FeedbackVerdict::FalsePositive),
            onset_hhmm: Some(valid_hhmm),
            avg_sleep_hr: Some(57.5),
            clear: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 8. Pruned epochs session: clicking Wearable Onset does NOT focus Onset; clicking [ Cancel ] closes
    app.selected_session_idx = 1;
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('f'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (pruned_col, pruned_row) =
        find_text_coords(terminal.backend().buffer(), "Wearable Onset:").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(pruned_col + 2, pruned_row)),
        None
    );
    assert!(!app.is_input_focused());

    let (canc_col, canc_row) = find_text_coords(terminal.backend().buffer(), "[ Cancel ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(canc_col + 2, canc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_history_export_trace_modal_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;
    app.history_sessions = vec![make_history_session(
        "sess-exp-1",
        "Polar H10",
        1_700_100_000_000,
        1_700_103_600_000,
        SessionStatus::Closed,
        24,
    )];
    app.selected_session_idx = 0;

    // Open ExportTraceModal via [x]
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('x'))));
    assert!(matches!(app.modal, ModalState::ExportTraceModal { .. }));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Backdrop clicks do not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::ExportTraceModal { .. }));

    // 2. Click Path -> focuses text input; replace path with dot-directory filename without extension
    let (path_col, path_row) = find_text_coords(terminal.backend().buffer(), "Path:").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(path_col + 10, path_row)),
        None
    );
    assert!(app.is_input_focused());
    for _ in 0..32 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for c in "/tmp/.hidden/my_trace".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // 3. Click '<' on < CSV > -> focuses Format (unfocuses Path) and cycles to JSONL cleanly
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (fmt_col, fmt_row) = find_text_coords(terminal.backend().buffer(), "< CSV >").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(fmt_col, fmt_row)), None);
    assert!(!app.is_input_focused());
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf_jsonl = buffer_to_string(terminal.backend().buffer());
    assert!(buf_jsonl.contains("< JSONL >"));
    assert!(buf_jsonl.contains("/tmp/.hidden/my_trace.jsonl"));

    // 4. Click middle on < JSONL > -> cycles back to CSV
    let (mid_col, mid_row) = find_text_coords(terminal.backend().buffer(), "< JSONL >").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(mid_col + 3, mid_row)),
        None
    );
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf_csv = buffer_to_string(terminal.backend().buffer());
    assert!(buf_csv.contains("< CSV >"));
    assert!(buf_csv.contains("/tmp/.hidden/my_trace.csv"));

    // 5. Click '>' on < CSV > -> cycles to JSONL
    let (gt_col, gt_row) = find_text_coords(terminal.backend().buffer(), "< CSV >").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(gt_col + 6, gt_row)), None);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("< JSONL >"));

    // 6. Click [x] Anonymize -> toggles off and focuses Anonymize; click again -> toggles on; click again -> off
    let (anon_col, anon_row) =
        find_text_coords(terminal.backend().buffer(), "[x] Anonymize").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(anon_col + 1, anon_row)),
        None
    );
    assert!(!app.is_input_focused());
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("[ ] Anonymize"));

    assert_eq!(
        app.handle_event(mouse_left_click(anon_col + 1, anon_row)),
        None
    );
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("[x] Anonymize"));

    assert_eq!(
        app.handle_event(mouse_left_click(anon_col + 1, anon_row)),
        None
    );

    // 7. Click [ Export ] -> emits SessionExportTrace and closes modal
    let (exp_col, exp_row) = find_text_coords(terminal.backend().buffer(), "[ Export ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(exp_col + 2, exp_row)),
        Some(TuiCommand::SessionExportTrace {
            id: "sess-exp-1".into(),
            path: "/tmp/.hidden/my_trace.jsonl".into(),
            format: bpmute::replay::TraceFormat::Jsonl,
            no_anonymize: true,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 8. Reopen and click [ Cancel ] -> closes without command
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('x'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (canc_col, canc_row) = find_text_coords(terminal.backend().buffer(), "[ Cancel ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(canc_col + 2, canc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_history_delete_session_modal_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;
    app.history_sessions = vec![make_history_session(
        "sess-del-1",
        "Polar H10",
        1_700_100_000_000,
        1_700_103_600_000,
        SessionStatus::Closed,
        24,
    )];
    app.selected_session_idx = 0;

    // Open DeleteSessionModal via [D]
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('D'))));
    assert!(matches!(
        app.modal,
        ModalState::DeleteSessionModal {
            confirmed: true,
            ..
        }
    ));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Backdrop clicks do not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::DeleteSessionModal { .. }));

    // 2. Even when [ Cancel ] is focused (confirmed == false), clicking [ Delete ] dispatches SessionDelete immediately
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert!(matches!(
        app.modal,
        ModalState::DeleteSessionModal {
            confirmed: false,
            ..
        }
    ));
    let (del_col, del_row) = find_text_coords(terminal.backend().buffer(), "[ Delete ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(del_col + 2, del_row)),
        Some(TuiCommand::SessionDelete {
            id: "sess-del-1".into(),
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 3. Reopen (confirmed == true) and click [ Cancel ] -> closes without deleting
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('D'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (canc_col, canc_row) = find_text_coords(terminal.backend().buffer(), "[ Cancel ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(canc_col + 2, canc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_settings_preferences_two_click_and_wheel() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;

    let mut cfg = bpmute::config::Config::default();
    cfg.detection.sensitivity = Sensitivity::Balanced;
    cfg.actions.pause_media = true;
    cfg.actions.lock_screen = false;
    cfg.actions.power_action = PowerAction::Suspend;
    cfg.actions.grace_period_sec = Some(30);
    cfg.actions.dry_run = false;
    app.active_config = Some(cfg);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    assert_eq!(app.selected_setting_idx, 0);

    // 1. Clicking an unselected preference row (idx 1: Pause Media) selects it without mutating config
    let (pm_col, pm_row) = find_text_coords(buf, "Pause Media").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(pm_col + 2, pm_row)), None);
    assert_eq!(app.selected_setting_idx, 1);

    // 2. Clicking the already-selected row (idx 1) when connected emits ConfigUpdate
    assert_eq!(
        app.handle_event(mouse_left_click(pm_col + 2, pm_row)),
        Some(TuiCommand::ConfigUpdate(bpmute::config::ConfigPatch {
            actions: Some(bpmute::config::ActionsPatch {
                pause_media: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        }))
    );
    assert_eq!(app.selected_setting_idx, 1);

    // 3. Clicking another unselected row (idx 5: Dry Run) selects it without mutating config, then second click toggles
    let (dr_col, dr_row) = find_text_coords(buf, "Dry Run").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(dr_col + 2, dr_row)), None);
    assert_eq!(app.selected_setting_idx, 5);
    assert_eq!(
        app.handle_event(mouse_left_click(dr_col + 2, dr_row)),
        Some(TuiCommand::ConfigUpdate(bpmute::config::ConfigPatch {
            actions: Some(bpmute::config::ActionsPatch {
                dry_run: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }))
    );

    // 4. Offline guard: clicking unselected row selects it, clicking already-selected row is a no-op
    app.daemon_connected = false;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf_off = terminal.backend().buffer();
    let (ls_col, ls_row) = find_text_coords(buf_off, "Lock Screen").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(ls_col + 2, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 2);
    assert_eq!(app.handle_event(mouse_left_click(ls_col + 2, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 2);

    // 5. Mouse-wheel ScrollUp / ScrollDown over Core Preferences moves selected_setting_idx between 0..=5
    assert_eq!(app.handle_event(mouse_scroll_up(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 1);
    assert_eq!(app.handle_event(mouse_scroll_up(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 0);
    // Clamps at 0
    assert_eq!(app.handle_event(mouse_scroll_up(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 0);

    for expected in 1..=5 {
        assert_eq!(app.handle_event(mouse_scroll_down(ls_col, ls_row)), None);
        assert_eq!(app.selected_setting_idx, expected);
    }
    // Clamps at 5
    assert_eq!(app.handle_event(mouse_scroll_down(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 5);

    // Scrolling outside Core Preferences (e.g. over System Settings & Storage) does not move selection
    let (stor_col, stor_row) = find_text_coords(buf_off, "System Settings & Storage").unwrap();
    assert_eq!(app.handle_event(mouse_scroll_up(stor_col, stor_row)), None);
    assert_eq!(app.selected_setting_idx, 5);
}

#[test]
fn test_mouse_settings_controls_and_guards() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    app.active_config = Some(bpmute::config::Config::default());
    app.storage_stats = Some(bpmute::storage::StorageStats {
        db_size_bytes: 145_408,
        db_size_formatted: "142 KB".into(),
        sessions_count: 12,
        session_epochs_count: 1420,
        raw_samples_count: 8500,
        session_feedback_count: 4,
        calibration_boundaries_count: 1,
        wearables_count: 2,
        raw_samples_retention: "14 days".into(),
        session_epochs_retention: "indefinite".into(),
        calibration_maturity: CalibrationMaturity::Calibrated,
        active_calibration_boundary: Some(CalibrationBoundaryRecord {
            id: "bnd-001".into(),
            effective_ts_ms: 1_700_000_000_000,
            reason: BoundaryReason::ManualRebaseline,
            seed_sleep_hr: Some(56.0),
            reset_floors: false,
            created_at_ms: 1_700_000_000_000,
            reverted_at_ms: None,
        }),
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Online: [E] Edit in $EDITOR, [U] Undo Boundary, [B] Re-baseline, [P] Purge Raw, [W] Hard Wipe
    let (ed_col, ed_row) =
        find_text_coords(terminal.backend().buffer(), "[E] Edit in $EDITOR").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(ed_col + 2, ed_row)),
        Some(TuiCommand::EditorHandoff)
    );

    let (undo_col, undo_row) =
        find_text_coords(terminal.backend().buffer(), "[U] Undo Boundary").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(undo_col + 2, undo_row)),
        Some(TuiCommand::CalibrationUndoBoundary)
    );

    let (reb_col, reb_row) =
        find_text_coords(terminal.backend().buffer(), "[B] Re-baseline").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(reb_col + 2, reb_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (purge_col, purge_row) =
        find_text_coords(terminal.backend().buffer(), "[P] Purge Raw").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(purge_col + 2, purge_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::PurgeRawModal { .. }));
    app.modal = ModalState::None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (wipe_col, wipe_row) =
        find_text_coords(terminal.backend().buffer(), "[W] Hard Wipe").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(wipe_col + 2, wipe_row)),
        None
    );
    assert!(matches!(app.modal, ModalState::HardWipeModal { .. }));
    app.modal = ModalState::None;

    // 2. Active boundary guard: when active_calibration_boundary is None, [U] Undo Boundary is hidden
    app.storage_stats
        .as_mut()
        .unwrap()
        .active_calibration_boundary = None;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(find_text_coords(terminal.backend().buffer(), "[U] Undo Boundary").is_none());

    // 3. Offline guard: [E] Edit in $EDITOR still emits EditorHandoff, while [U]/[B]/[P]/[W] are no-ops
    app.storage_stats
        .as_mut()
        .unwrap()
        .active_calibration_boundary = Some(CalibrationBoundaryRecord {
        id: "bnd-002".into(),
        effective_ts_ms: 1_700_000_000_000,
        reason: BoundaryReason::ManualRebaseline,
        seed_sleep_hr: None,
        reset_floors: false,
        created_at_ms: 1_700_000_000_000,
        reverted_at_ms: None,
    });
    app.daemon_connected = false;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let off_buf = terminal.backend().buffer();

    let (ed_off_col, ed_off_row) = find_text_coords(off_buf, "[E] Edit in $EDITOR").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(ed_off_col + 2, ed_off_row)),
        Some(TuiCommand::EditorHandoff)
    );

    let (undo_off_col, undo_off_row) = find_text_coords(off_buf, "[U] Undo Boundary").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(undo_off_col + 2, undo_off_row)),
        None
    );

    let (reb_off_col, reb_off_row) = find_text_coords(off_buf, "[B] Re-baseline").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(reb_off_col + 2, reb_off_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    let (purge_off_col, purge_off_row) = find_text_coords(off_buf, "[P] Purge Raw").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(purge_off_col + 2, purge_off_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);

    let (wipe_off_col, wipe_off_row) = find_text_coords(off_buf, "[W] Hard Wipe").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(wipe_off_col + 2, wipe_off_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_settings_soft_rebaseline_modal_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    app.active_config = Some(bpmute::config::Config::default());

    // Open SoftRebaselineModal via [B]
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('B'))));
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Backdrop clicks outside modal box do not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));

    // 2. Clicking `[ ] Force cold-start floors` toggles reset_floors and focuses it (unfocusing text input)
    assert!(app.is_input_focused());
    let (chk_col, chk_row) =
        find_text_coords(terminal.backend().buffer(), "[ ] Force cold-start floors").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(chk_col + 1, chk_row)),
        None
    );
    assert!(!app.is_input_focused());
    assert!(matches!(
        app.modal,
        ModalState::SoftRebaselineModal {
            reset_floors: true,
            active_field: bpmute::tui::app::SoftRebaselineField::ResetFloors,
            ..
        }
    ));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("[x] Force cold-start floors"));

    // 3. Clicking `Seed Sleep HR` input focuses text entry, type invalid BPM "20"
    let (seed_col, seed_row) =
        find_text_coords(terminal.backend().buffer(), "[ e.g. 54.0  ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(seed_col + 2, seed_row)),
        None
    );
    assert!(app.is_input_focused());
    for c in "20".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // 4. Clicking [ Submit ] with out-of-range BPM displays validation error and keeps modal open
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (sub_col, sub_row) = find_text_coords(terminal.backend().buffer(), "[ Submit ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(sub_col + 2, sub_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::SoftRebaselineModal {
            error_message: Some(_),
            ..
        }
    ));

    // Re-focus Seed Sleep HR, fix to "54.5", and click [ Submit ] -> dispatches CalibrationSetBoundary
    let (seed_err_col, seed_err_row) =
        find_text_coords(terminal.backend().buffer(), "[ 20         ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(seed_err_col + 2, seed_err_row)),
        None
    );
    for _ in 0..2 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for c in "54.5".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (sub_col2, sub_row2) = find_text_coords(terminal.backend().buffer(), "[ Submit ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(sub_col2 + 2, sub_row2)),
        Some(TuiCommand::CalibrationSetBoundary {
            effective_ts_ms: None,
            seed_sleep_hr: Some(54.5),
            reset_floors: true,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 5. Reopen and click in-modal [ Cancel ] -> closes without command
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('B'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (canc_col, canc_row) = find_text_coords(terminal.backend().buffer(), "[ Cancel ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(canc_col + 2, canc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_settings_purge_raw_modal_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    app.active_config = Some(bpmute::config::Config::default());

    // Open PurgeRawModal via [P]
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('P'))));
    assert!(matches!(app.modal, ModalState::PurgeRawModal { .. }));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Backdrop clicks do not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::PurgeRawModal { .. }));

    // 2. Clicking in-modal [Enter] Confirm Purge dispatches DataPurgeRaw and closes modal
    let (confirm_col, confirm_row) =
        find_text_coords(terminal.backend().buffer(), "[Enter] Confirm Purge").unwrap();
    assert!(confirm_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(confirm_col + 2, confirm_row)),
        Some(TuiCommand::DataPurgeRaw)
    );
    assert_eq!(app.modal, ModalState::None);

    // 3. Reopen and click in-modal [Esc] Cancel -> closes without command
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('P'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (esc_col, esc_row) = find_text_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert!(esc_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(esc_col + 2, esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_mouse_settings_hard_wipe_modal_and_backdrop() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    app.active_config = Some(bpmute::config::Config::default());

    // Open HardWipeModal via [W]
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('W'))));
    assert!(matches!(app.modal, ModalState::HardWipeModal { .. }));

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 1. Backdrop clicks do not dismiss
    assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
    assert_eq!(app.handle_event(mouse_left_click(0, 12)), None);
    assert!(matches!(app.modal, ModalState::HardWipeModal { .. }));

    // 2. Clicking 'WIPE' confirmation input focuses it; typing wrong text + clicking [Enter] Confirm Wipe shows mismatch error
    let (in_col, in_row) =
        find_text_coords(terminal.backend().buffer(), "Type 'WIPE' to confirm:").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(in_col + 24, in_row)),
        None
    );
    assert!(app.is_input_focused());
    for c in "NOPE".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (confirm_col, confirm_row) =
        find_text_coords(terminal.backend().buffer(), "[Enter] Confirm Wipe").unwrap();
    assert!(confirm_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(confirm_col + 2, confirm_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::HardWipeModal {
            error_message: Some(_),
            ..
        }
    ));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert!(buffer_to_string(terminal.backend().buffer()).contains("Confirmation mismatch"));

    // 3. Replace with "WIPE" and click in-modal [Enter] Confirm Wipe -> dispatches DataHardWipe
    for _ in 0..4 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for c in "WIPE".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (confirm_col2, confirm_row2) =
        find_text_coords(terminal.backend().buffer(), "[Enter] Confirm Wipe").unwrap();
    assert!(confirm_row2 < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(confirm_col2 + 2, confirm_row2)),
        Some(TuiCommand::DataHardWipe {
            confirm: "WIPE".into(),
            wipe_all: false,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 4. Reopen and click in-modal [Esc] Cancel -> closes without command
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('W'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (esc_col, esc_row) = find_text_coords(terminal.backend().buffer(), "[Esc] Cancel").unwrap();
    assert!(esc_row < 23);
    assert_eq!(
        app.handle_event(mouse_left_click(esc_col + 2, esc_row)),
        None
    );
    assert_eq!(app.modal, ModalState::None);
}

struct RealSocketTuiHarness {
    _temp: TempDir,
    paths: bpmute::paths::ResolvedPaths,
    daemon: DaemonHandle,
    event_rx: mpsc::Receiver<TuiEvent>,
    cmd_tx: mpsc::Sender<TuiCommand>,
    shutdown_tx: watch::Sender<bool>,
    bridge_task: tokio::task::JoinHandle<()>,
    app: TuiApp,
    session_ids: Vec<String>,
}

impl RealSocketTuiHarness {
    async fn spawn(session_count: usize, with_raw_samples: bool) -> Self {
        let temp = TempDir::new().unwrap();
        let (paths, env) = make_test_paths(&temp);
        let base_ts = Utc::now().timestamp_millis() - 3_600_000;

        let mut session_ids = Vec::with_capacity(session_count);
        {
            let db_file = bpmute::storage::db_path_in_data_dir(&paths.data_dir);
            let db_lock = bpmute::storage::lock_path_in_data_dir(&paths.data_dir);
            let mut store =
                bpmute::storage::LedgerStore::open_read_write(&db_file, &db_lock).unwrap();
            let wid = store
                .upsert_wearable(
                    &bpmute::storage::WearableDescriptor {
                        mac: "AA:BB:CC:DD:EE:01".into(),
                        address_type: "public".into(),
                        alias: Some("Band".into()),
                        advertised_name: None,
                        supports_rr: true,
                        supports_contact: true,
                    },
                    base_ts - 100_000,
                )
                .unwrap();

            for i in 0..session_count {
                let start_ts = base_ts + (i as i64) * 600_000;
                let end_ts = start_ts + 400_000;
                let sid = store
                    .open_session(&bpmute::storage::OpenSessionRequest {
                        wearable_id: wid.clone(),
                        start_ts_ms: start_ts,
                        detection_sensitivity: bpmute::config::Sensitivity::Balanced,
                    })
                    .unwrap();
                if with_raw_samples {
                    store
                        .append_epoch_and_raw(
                            &sid,
                            None,
                            &[(
                                0,
                                &HrPacket {
                                    ts_ms: start_ts + 1_000,
                                    bpm: 64.0,
                                    rr_ms: vec![930.0],
                                    contact_status: ContactStatus::Contact,
                                },
                            )],
                        )
                        .unwrap();
                }
                store
                    .close_session_and_recompute(
                        &bpmute::storage::CloseSessionRequest {
                            session_id: sid.clone(),
                            summary: SessionSummary {
                                session_index: (i + 1) as u64,
                                start_ts_ms: start_ts,
                                end_ts_ms: end_ts,
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
                                had_rr: with_raw_samples,
                                grace_cancel_count: 0,
                                rapid_wake_fp_count: 0,
                                snooze_count: 0,
                                epochs_count: usize::from(with_raw_samples),
                            },
                            trigger_power_action: PowerAction::None,
                            rapid_resume_within_5m: false,
                        },
                        end_ts,
                    )
                    .unwrap();
                session_ids.push(sid);
            }
        }

        let loaded =
            LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();
        let clock = Arc::new(VirtualClock::starting_at(Utc::now()));
        let daemon = DaemonHandle::spawn(DaemonOptions::for_testing(
            paths.clone(),
            loaded,
            Arc::new(env),
            clock,
        ))
        .await
        .unwrap();

        let (event_tx, mut event_rx) = mpsc::channel(256);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
        let bridge_task = tokio::spawn(bridge.run());

        let mut app = TuiApp::new(false, false, false);
        wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
            a.daemon_connected
                && a.active_config.is_some()
                && a.history_sessions.len() == session_count
                && (session_count == 0 || a.selected_session_detail.is_some())
        })
        .await;

        // `session.list` orders newest-first, matching `app.history_sessions`.
        session_ids.reverse();

        Self {
            _temp: temp,
            paths,
            daemon,
            event_rx,
            cmd_tx,
            shutdown_tx,
            bridge_task,
            app,
            session_ids,
        }
    }

    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.bridge_task.await;
        self.daemon.shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_delete_session_e2e() {
    let mut h = RealSocketTuiHarness::spawn(1, false).await;
    let s1_id = h.session_ids[0].clone();

    // Switch to History tab, open Delete modal with 'D', confirm with Enter
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(h.app.active_tab, Tab::History);
    assert_eq!(
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('D')))),
        None
    );
    assert!(matches!(h.app.modal, ModalState::DeleteSessionModal { .. }));

    let del_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("confirming DeleteSessionModal should emit SessionDelete");
    assert_eq!(del_cmd, TuiCommand::SessionDelete { id: s1_id.clone() });
    h.cmd_tx.send(del_cmd).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_sessions.is_empty()
    })
    .await;

    let mut client = IpcClient::connect(&h.paths.socket_path).await.unwrap();
    let list_resp = client.request("session.list", json!({})).await.unwrap();
    let detail_cleared = h.app.selected_session_detail.is_none();

    h.shutdown().await;

    assert!(
        detail_cleared,
        "selected_session_detail should clear when last session is deleted"
    );
    assert_eq!(
        list_resp["sessions"].as_array().map(|a| a.len()),
        Some(0),
        "session still exists in daemon DB: {list_resp:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_delete_middle_session_updates_detail_pane() {
    let mut h = RealSocketTuiHarness::spawn(3, false).await;
    let expected_remaining_id = h.session_ids[2].clone();

    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    let select_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Char('j'))))
        .expect("selecting row 1 should request SessionGet");
    h.cmd_tx.send(select_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.selected_session_idx == 1
            && a.selected_session_detail
                .as_ref()
                .is_some_and(|d| d.session.id == h.session_ids[1])
    })
    .await;

    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('D'))));
    let del_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("confirming DeleteSessionModal should emit SessionDelete");
    h.cmd_tx.send(del_cmd).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_sessions.len() == 2
            && a.selected_session_idx == 1
            && a.selected_session_detail
                .as_ref()
                .is_some_and(|d| d.session.id == expected_remaining_id)
    })
    .await;

    h.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_export_trace_e2e() {
    let mut h = RealSocketTuiHarness::spawn(1, true).await;

    let export_path = h._temp.path().join("exported-trace.csv");
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('x')))),
        None
    );
    if let ModalState::ExportTraceModal {
        ref mut destination_path,
        ..
    } = h.app.modal
    {
        *destination_path = export_path.to_string_lossy().into_owned();
    } else {
        panic!("expected ExportTraceModal");
    }

    let export_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("confirming ExportTraceModal should emit SessionExportTrace");
    h.cmd_tx.send(export_cmd).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.op_feedback
            .as_ref()
            .is_some_and(|fb| !fb.is_error && fb.message == "Trace exported")
    })
    .await;

    let csv = fs::read_to_string(&export_path).expect("exported CSV file should exist");
    h.shutdown().await;

    assert!(csv.contains("ts_ms,bpm,rr_ms,contact_status,event"));
    assert!(csv.contains("64.0"));
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

#[tokio::test(start_paused = true)]
async fn test_real_socket_settings_config_rebaseline_undo_and_hard_wipe_e2e() {
    let mut h = RealSocketTuiHarness::spawn(1, false).await;

    // 1. Settings -> toggle Sensitivity via ConfigSnapshotPayload round-trip
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(h.app.active_tab, Tab::Settings);
    let initial_sens = h.app.active_config.as_ref().unwrap().detection.sensitivity;
    let cfg_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("toggling Sensitivity should emit ConfigUpdate");
    h.cmd_tx.send(cfg_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.active_config
            .as_ref()
            .is_some_and(|c| c.detection.sensitivity != initial_sens)
    })
    .await;

    // 2. Soft Rebaseline ('B') with seed 52
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('B'))));
    for c in "52".chars() {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let rebaseline_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("SoftRebaselineModal should emit CalibrationSetBoundary");
    h.cmd_tx.send(rebaseline_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_boundaries.len() == 1
    })
    .await;

    // 3. Undo Boundary ('U')
    let undo_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Char('U'))))
        .expect("Undo should emit CalibrationUndoBoundary");
    h.cmd_tx.send(undo_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_boundaries
            .first()
            .is_some_and(|b| b.reverted_at_ms.is_some())
    })
    .await;

    // 4. Hard Wipe ('W')
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('W'))));
    for c in "WIPE".chars() {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let wipe_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("HardWipeModal should emit DataHardWipe");
    h.cmd_tx.send(wipe_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_sessions.is_empty()
    })
    .await;

    assert!(h.app.selected_session_detail.is_none());
    h.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_feedback_and_unenroll_delete_sessions_e2e() {
    let mut h = RealSocketTuiHarness::spawn(1, false).await;

    // 1. Feedback modal ('f') -> step verdict to FalsePositive -> submit
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('f'))));
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    let fb_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("FeedbackModal should emit SessionSetFeedback");
    h.cmd_tx.send(fb_cmd).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_sessions
            .first()
            .and_then(|s| s.feedback.as_ref())
            .and_then(|f| f.verdict)
            .is_some()
    })
    .await;

    // 2. Wearables -> Unenroll with delete_sessions = true -> recomputes calibration and refreshes history
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('3'))));
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('u'))));
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    let unenroll_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("UnenrollModal should emit WearableUnenroll");
    h.cmd_tx.send(unenroll_cmd).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.enrolled_wearables.is_empty() && a.history_sessions.is_empty()
    })
    .await;

    let mut client = IpcClient::connect(&h.paths.socket_path).await.unwrap();
    let cal_resp = client
        .request("calibration.status", json!({}))
        .await
        .unwrap();
    assert_eq!(cal_resp["valid_sessions"], 0);
    assert_eq!(cal_resp["maturity"], "cold_start");
    let wearables_map = cal_resp["wearables"].as_object().unwrap();
    assert!(
        wearables_map.values().all(|w| w["sessions_count"] == 0),
        "all wearable session counts should reset to 0: {cal_resp:?}"
    );
    assert!(h.app.selected_session_detail.is_none());

    h.shutdown().await;
}
