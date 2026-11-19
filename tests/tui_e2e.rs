use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use bpmute::config::{ActionsConfig, CliOverrides, LoadedConfig, PowerAction, ScheduleConfig};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::{
    AlertSeverity, DaemonAlert, EnrolledWearableStatus, HrSampleEvent, MonitoringState, PauseMode,
    PauseParams,
};
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{ContactStatus, StreamStatus};
use bpmute::storage::EnrolledWearable;
use bpmute::tui::{
    GraceState, ModalState, PairingPromptModalState, Tab, TuiApp, TuiCommand, TuiEvent,
    TuiIpcBridge,
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

#[test]
fn test_80x24_baseline_layout_and_navigation() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.monitoring_state = MonitoringState::Monitoring;
    app.latest_bpm = Some(68.0);
    app.latest_p_sleep = Some(0.12);

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Initial 80x24 Dashboard render
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    assert!(
        out.contains("1:Dashboard"),
        "Header must show Dashboard tab"
    );
    assert!(out.contains("2:History"), "Header must show History tab");
    assert!(
        out.contains("3:Wearables"),
        "Header must show Wearables tab"
    );
    assert!(out.contains("4:Settings"), "Header must show Settings tab");
    assert!(out.contains("68"), "Dashboard must show live BPM");
    assert!(
        out.contains("Sleep State"),
        "Dashboard must show Sleep State card"
    );
    assert!(out.contains("[1-4] Tabs"), "Footer must show tabs shortcut");
    assert!(out.contains("[?] Help"), "Footer must show help shortcut");
    assert!(out.contains("[q] Quit"), "Footer must show quit shortcut");

    // 2. Tab switching via numeric keys 1-4
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(app.active_tab, Tab::History);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Monitoring Sessions & Calibration History"));

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('3'))));
    assert_eq!(app.active_tab, Tab::Wearables);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Enrolled Wearables"));

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.active_tab, Tab::Settings);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("System Settings & Storage"));

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    assert_eq!(app.active_tab, Tab::Dashboard);

    // 3. Tab navigation via Tab / BackTab / Vim-hybrid (h/l) / Arrows
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

    // 4. Help modal
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('?'))));
    assert_eq!(app.modal, ModalState::Help);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("BPMute Keyboard & Mouse Shortcuts [?]"));

    // Close help modal with Esc
    app.handle_event(TuiEvent::Key(key(KeyCode::Esc)));
    assert_eq!(app.modal, ModalState::None);

    // 5. Quit key
    assert!(!app.should_quit);
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('q'))));
    assert!(app.should_quit);
}

#[test]
fn test_mouse_tab_switching_and_no_mouse_option() {
    // Normal mouse mode: clicking tab header switches tabs
    let mut app = TuiApp::new(false, false, false);
    assert_eq!(app.active_tab, Tab::Dashboard);

    // Click on row 0, col 20 (History tab area)
    app.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 20,
        row: 0,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(app.active_tab, Tab::History);

    // Click on row 0, col 35 (Wearables tab area)
    app.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 35,
        row: 0,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(app.active_tab, Tab::Wearables);

    // Click on row 0, col 50 (Settings tab area)
    app.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 50,
        row: 0,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(app.active_tab, Tab::Settings);

    // Now test with --no-mouse: mouse events must be ignored
    let mut app_no_mouse = TuiApp::new(true, false, false);
    assert_eq!(app_no_mouse.active_tab, Tab::Dashboard);

    app_no_mouse.handle_event(TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 35,
        row: 0,
        modifiers: KeyModifiers::empty(),
    }));
    assert_eq!(
        app_no_mouse.active_tab,
        Tab::Dashboard,
        "Mouse clicks must be ignored when --no-mouse is set"
    );
}

#[test]
fn test_responsive_resizing_limits() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;

    // 1. Below 60x18: Render resize notice
    let backend_small = TestBackend::new(55, 15);
    let mut terminal_small = Terminal::new(backend_small).unwrap();
    terminal_small
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_small = buffer_to_string(terminal_small.backend().buffer());

    assert!(
        out_small.contains("Terminal too small"),
        "Must render resize notice when below 60x18"
    );
    assert!(
        out_small.contains("55×15"),
        "Notice must show current dimensions"
    );
    assert!(
        out_small.contains("60×18"),
        "Notice must show minimum required dimensions"
    );

    // 2. Compact mode (65x20): Must render cleanly without panic
    let backend_compact = TestBackend::new(65, 20);
    let mut terminal_compact = Terminal::new(backend_compact).unwrap();
    terminal_compact
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_compact = buffer_to_string(terminal_compact.backend().buffer());
    assert!(!out_compact.contains("Terminal too small"));
    assert!(out_compact.contains("Dashboard"));
}

#[test]
fn test_no_color_and_ascii_modes() {
    // 1. Test ASCII mode
    let mut app_ascii = TuiApp::new(false, true, false);
    app_ascii.daemon_connected = true;
    for bpm in [65.0, 70.0, 75.0, 80.0, 78.0, 72.0, 68.0] {
        app_ascii.sparkline_data.push_sample(HrSampleEvent {
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "TestStrap".into(),
            session_id: None,
            ts_ms: 1_700_000_000_000,
        });
    }

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| app_ascii.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    // Verify no Braille Unicode characters (U+2800..U+28FF) are present
    let has_braille = out.chars().any(|c| ('\u{2800}'..='\u{28ff}').contains(&c));
    assert!(
        !has_braille,
        "ASCII mode must not produce Braille characters"
    );

    // 2. Test NO_COLOR mode
    let mut app_no_color = TuiApp::new(false, false, true);
    app_no_color.daemon_connected = true;
    app_no_color.monitoring_state = MonitoringState::Confirmed;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| app_no_color.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();

    // In NO_COLOR mode, Theme uses Color::Reset for fg and bg
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            assert_eq!(
                cell.fg,
                ratatui::style::Color::Reset,
                "NO_COLOR mode cell fg must be Reset"
            );
            assert_eq!(
                cell.bg,
                ratatui::style::Color::Reset,
                "NO_COLOR mode cell bg must be Reset"
            );
        }
    }
}

#[test]
fn test_hotkey_suspension_in_text_inputs() {
    let mut app = TuiApp::new(false, false, false);
    app.active_tab = Tab::Wearables;

    // 1. Rename modal
    app.modal = ModalState::RenameModal {
        wearable_id: "w-01".into(),
        current_alias: "Old Strap".into(),
        input: "My ".into(),
    };

    assert!(
        app.is_input_focused(),
        "Rename modal must have text input focused"
    );

    // Type characters that would otherwise trigger global hotkeys (1, 2, q, ?, h, l, d, p)
    for c in ['1', '2', 'q', '?', 'h', 'l', 'd', 'p'] {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }

    // Verify global hotkeys were NOT triggered:
    assert_eq!(
        app.active_tab,
        Tab::Wearables,
        "Tab must not switch while input focused"
    );
    assert!(!app.should_quit, "q must not quit while input focused");
    assert_ne!(
        app.modal,
        ModalState::Help,
        "? must not open Help modal while input focused"
    );
    assert!(
        !app.is_diagnostics_open,
        "d must not toggle diagnostics while input focused"
    );

    // Verify input buffer received all typed characters
    if let ModalState::RenameModal { ref input, .. } = app.modal {
        assert_eq!(input, "My 12q?hldp");
    } else {
        panic!("Modal must still be RenameModal");
    }

    // Backspace
    app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    if let ModalState::RenameModal { ref input, .. } = app.modal {
        assert_eq!(input, "My 12q?hld");
    }

    // Enter submits rename
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::WearableRename {
            id: "w-01".into(),
            alias: Some("My 12q?hld".into()),
        })
    );
    assert_eq!(app.modal, ModalState::None);
    assert!(!app.is_input_focused());

    // 2. Enrollment modal with Agent1 Passkey prompt
    app.modal = ModalState::EnrollmentModal {
        scanning_timer_sec: 20,
        selected_device_idx: 0,
        alias_input: String::new(),
        alias_focused: false,
        pairing_prompt: Some(PairingPromptModalState::Passkey {
            device: "Polar H10".into(),
            input: String::new(),
        }),
    };

    assert!(
        app.is_input_focused(),
        "Passkey prompt must have input focused"
    );

    // Type passkey numbers
    for c in ['1', '2', '3', '4', '5', '6'] {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.active_tab, Tab::Wearables);
    assert!(!app.should_quit);

    // Submit passkey with Enter
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    if let Some(TuiCommand::EnrollAuthReply(params)) = cmd {
        assert_eq!(params.confirmed, Some(true));
        assert_eq!(params.passkey, Some(123456));
    } else {
        panic!("Expected EnrollAuthReply command");
    }
}

#[test]
fn test_dashboard_diagnostics_and_pause_popover() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.idle_backend = Some("mutter-idle-monitor".into());
    app.playing_players_count = 2;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Toggle Engine Diagnostics with 'd'
    assert!(!app.is_diagnostics_open);
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('d'))));
    assert!(app.is_diagnostics_open);

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(
        out.contains("Engine Diagnostics [d]"),
        "Diagnostics pane must be visible"
    );
    assert!(out.contains("Slope:"), "Must show HR slope feature");
    assert!(out.contains("Var:"), "Must show rolling variance feature");
    assert!(out.contains("ln(RMSSD):"), "Must show ln(RMSSD) feature");
    assert!(
        out.contains("mutter-idle-monitor"),
        "Must show idle-probe backend"
    );
    assert!(
        out.contains("Playing MPRIS2 Players: 2"),
        "Must show active playing players"
    );

    // Toggle off
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('d'))));
    assert!(!app.is_diagnostics_open);

    // 2. Open Pause Popover with 'p'
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('p'))));
    assert_eq!(app.modal, ModalState::PausePopover { selected_idx: 0 });

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Pause / Resume Monitoring"));
    assert!(out.contains("1. Resume monitoring"));
    assert!(out.contains("2. Pause indefinitely"));
    assert!(out.contains("3. Pause 1 hour"));
    assert!(out.contains("4. Pause 2 hours"));
    assert!(out.contains("5. Pause until next morning"));

    // Navigate with 'j' to "Pause 1 hour" (index 2)
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j')))); // index 1
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j')))); // index 2

    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::Pause(PauseParams {
            mode: Some(PauseMode::Duration),
            duration_sec: Some(3600),
        }))
    );
    assert_eq!(app.modal, ModalState::None);
}

#[test]
fn test_alert_strip_actions() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;

    // Add persistent shift alert
    app.alerts.push(DaemonAlert {
        id: "persistent_shift".into(),
        kind: "calibration".into(),
        severity: AlertSeverity::Warn,
        message: "5-session shift of +4.2 BPM detected".into(),
        dismissible: true,
    });

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("ALERT [calibration]"));
    assert!(out.contains("5-session shift of +4.2 BPM detected"));
    assert!(out.contains("[a] Accept"));
    assert!(out.contains("[i] Ignore"));
    assert!(out.contains("[x] Dismiss"));

    // Press 'a' to accept shift
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('a'))));
    assert_eq!(cmd, Some(TuiCommand::AcceptShift));

    // Press 'i' to dismiss/ignore shift
    let cmd2 = app.handle_event(TuiEvent::Key(key(KeyCode::Char('i'))));
    assert_eq!(cmd2, Some(TuiCommand::DismissShift));

    // Press 'x' to dismiss alert
    let cmd3 = app.handle_event(TuiEvent::Key(key(KeyCode::Char('x'))));
    assert_eq!(
        cmd3,
        Some(TuiCommand::DismissAlert {
            id: "persistent_shift".into(),
        })
    );
    assert!(app.alerts.is_empty());
}

#[test]
fn test_global_grace_period_banner_across_tabs() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.grace_state = Some(GraceState {
        session_id: "sess-test".into(),
        remaining_sec: 23,
        total_sec: 30,
        planned_actions: vec!["Pause Media".into(), "Suspend".into()],
        dry_run: false,
    });

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // Verify banner renders across all 4 tabs
    for tab in [Tab::Dashboard, Tab::History, Tab::Wearables, Tab::Settings] {
        app.active_tab = tab;
        terminal
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let out = buffer_to_string(terminal.backend().buffer());

        assert!(
            out.contains("GRACE:"),
            "Grace banner must be pinned on tab {:?}",
            tab
        );
        assert!(
            out.contains("23s left"),
            "Grace banner must show countdown on tab {:?}",
            tab
        );
        assert!(
            out.contains("[c] Cancel"),
            "Grace banner must show Cancel button on tab {:?}",
            tab
        );
        assert!(
            out.contains("[s] Snooze"),
            "Grace banner must show Snooze button on tab {:?}",
            tab
        );
    }

    // Cancel key 'c'
    let cmd_cancel = app.handle_event(TuiEvent::Key(key(KeyCode::Char('c'))));
    assert_eq!(cmd_cancel, Some(TuiCommand::CancelGrace));

    // Snooze key 's'
    let cmd_snooze = app.handle_event(TuiEvent::Key(key(KeyCode::Char('s'))));
    assert_eq!(cmd_snooze, Some(TuiCommand::SnoozeGrace(None)));
}

#[test]
fn test_wearables_tab_modals() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Wearables;

    let w1 = EnrolledWearableStatus {
        wearable: EnrolledWearable {
            id: "polar-01".into(),
            mac: "AA:BB:CC:11:22:33".into(),
            mac_masked: "AA:BB:***".into(),
            address_type: "public".into(),
            alias: Some("Chest Strap".into()),
            advertised_name: Some("Polar H10".into()),
            supports_rr: true,
            supports_contact: true,
            hr_var_floor: 5.0,
            typical_sqi: Some(0.95),
            enrolled_at_ms: 1_700_000_000_000,
            last_seen_at_ms: 1_700_000_000_000,
            active: true,
        },
        status: StreamStatus::Streaming,
        battery: Some(85),
    };
    let w2 = EnrolledWearableStatus {
        wearable: EnrolledWearable {
            id: "whoop-02".into(),
            mac: "AA:BB:CC:44:55:66".into(),
            mac_masked: "AA:BB:***".into(),
            address_type: "public".into(),
            alias: Some("Wrist Band".into()),
            advertised_name: Some("Whoop 4.0".into()),
            supports_rr: false,
            supports_contact: false,
            hr_var_floor: 5.0,
            typical_sqi: None,
            enrolled_at_ms: 1_700_000_000_000,
            last_seen_at_ms: 1_700_000_000_000,
            active: false,
        },
        status: StreamStatus::Disconnected,
        battery: None,
    };
    app.enrolled_wearables = vec![w1.clone(), w2];

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Wearables list table render
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Chest Strap"));
    assert!(out.contains("Streaming"));
    assert!(out.contains("85%"));
    assert!(out.contains("BPM+RR"));
    assert!(out.contains("Wrist Band"));
    assert!(out.contains("Disconnected"));
    assert!(out.contains("BPM-only"));

    // 2. Unenroll modal with checkboxes
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('u'))));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Unenroll Wearable"));
    assert!(out.contains("Remove 'Chest Strap' from BPMute allowlist?"));
    assert!(out.contains("[ ] [1] Also forget from BlueZ"));
    assert!(out.contains("[ ] [2] Also delete historical Monitoring Sessions"));

    // Toggle checkboxes with '1' and '2'
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("[X] [1] Also forget from BlueZ"));
    assert!(out.contains("[X] [2] Also delete historical Monitoring Sessions"));

    // Confirm unenroll
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::WearableUnenroll {
            id: "polar-01".into(),
            forget_bluez: true,
            delete_sessions: true,
        })
    );
    assert_eq!(app.modal, ModalState::None);

    // 3. Agent1 Pairing prompt in Enrollment modal
    app.modal = ModalState::EnrollmentModal {
        scanning_timer_sec: 28,
        selected_device_idx: 0,
        alias_input: String::new(),
        alias_focused: false,
        pairing_prompt: Some(PairingPromptModalState::Confirmation {
            passkey: 876543,
            device: "Polar H10".into(),
        }),
    };

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Bluetooth LE Pairing Authorization (Agent1)"));
    assert!(out.contains("876543"));
    assert!(out.contains("[y] to Confirm"));
    assert!(out.contains("[n]/[Esc] to Reject"));

    // Confirm with 'y'
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('y'))));
    if let Some(TuiCommand::EnrollAuthReply(params)) = cmd {
        assert_eq!(params.confirmed, Some(true));
    } else {
        panic!("Expected EnrollAuthReply");
    }
}

#[test]
fn test_zero_wearables_inline_cta() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.enrolled_wearables.clear();
    app.systemd_service_installed = false;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());

    assert!(
        out.contains("No BLE Heart Rate wearables enrolled yet."),
        "Must display zero-wearables CTA when enrolled list is empty"
    );
    assert!(out.contains("Press [e] on the Dashboard"));
    assert!(out.contains("bpmute service install"));

    // Pressing 'e' on Dashboard activates Enrollment modal on Wearables tab
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('e'))));
    assert_eq!(app.active_tab, Tab::Wearables);
    assert!(matches!(app.modal, ModalState::EnrollmentModal { .. }));
    assert_eq!(cmd, Some(TuiCommand::EnrollStart(Some(30))));
}

#[tokio::test]
async fn test_daemon_offline_banner_and_resilience_e2e() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);

    let mut app = TuiApp::new(false, false, false);
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    // 1. Initial state: daemon is offline
    assert!(!app.daemon_connected);
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(
        out.contains("Daemon offline — retrying…"),
        "Must show offline banner when daemon is disconnected"
    );
    assert!(out.contains("systemctl --user start bpmute"));

    // Live controls disabled while offline
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('p'))));
    assert_eq!(
        app.modal,
        ModalState::None,
        "Pause popover must be disabled while daemon offline"
    );

    // 2. Start real daemon with real NDJSON Unix socket
    let cli_overrides = CliOverrides::default();
    let mut loaded = LoadedConfig::load(&paths.config_file, &env, &cli_overrides).unwrap();
    loaded.config.actions = ActionsConfig {
        power_action: PowerAction::Suspend,
        grace_period_sec: Some(30),
        ..ActionsConfig::default()
    };
    loaded.config.schedule = ScheduleConfig::default();

    let clock = Arc::new(VirtualClock::starting_at(Utc::now()));
    let daemon = DaemonHandle::spawn(DaemonOptions::for_testing(
        paths.clone(),
        loaded.clone(),
        Arc::new(env.clone()),
        clock,
    ))
    .await
    .expect("daemon should start");

    // 3. Spawn TuiIpcBridge
    let (event_tx, mut event_rx) = mpsc::channel(128);
    let (cmd_tx, cmd_rx) = mpsc::channel(128);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    // Wait for connection handshake
    let mut connected = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(ev) = event_rx.recv() => {
                let _ = app.handle_event(ev);
                if app.daemon_connected {
                    connected = true;
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    assert!(connected, "TuiIpcBridge must connect to daemon socket");

    // Render: offline banner must be gone
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(!out.contains("Daemon offline — retrying…"));

    // 4. Trigger Grace Period over daemon socket and verify TUI reception
    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let _ = client
        .request("monitor.test_trigger", json!({ "grace_sec": 20 }))
        .await
        .unwrap();

    let mut received_grace = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(ev) = event_rx.recv() => {
                let _ = app.handle_event(ev);
                if app.grace_state.is_some() {
                    received_grace = true;
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    assert!(received_grace, "TUI must receive GraceTick event");

    // Verify Grace Banner rendering
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("GRACE:"));

    // Cancel Grace Period via TUI key 'c'
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('c'))));
    assert_eq!(cmd, Some(TuiCommand::CancelGrace));
    cmd_tx.send(cmd.unwrap()).await.unwrap();

    // Check daemon state returned to not Confirmed
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(ev) = event_rx.recv() => {
                let _ = app.handle_event(ev);
                if app.grace_state.is_none() {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    assert!(
        app.grace_state.is_none(),
        "Grace state must be cleared after cancellation"
    );

    // 5. Mid-session daemon shutdown: TUI stays open and transitions to offline banner
    daemon.shutdown().await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
            Some(ev) = event_rx.recv() => {
                let _ = app.handle_event(ev);
                if !app.daemon_connected {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    assert!(
        !app.daemon_connected,
        "Daemon disconnection must be detected"
    );

    // Render: offline banner must reappear seamlessly without crashing
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out = buffer_to_string(terminal.backend().buffer());
    assert!(out.contains("Daemon offline — retrying…"));

    // Clean up bridge task
    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
}
