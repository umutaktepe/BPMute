mod common;
use common::tui::*;

use std::fs;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use tempfile::TempDir;
use tokio::sync::{mpsc, oneshot, watch};

use bpmute::actions::traits::BoxFuture;
use bpmute::bluetooth::agent::AgentAuthRequest;
use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{
    DaemonCore, DaemonHandle, DaemonOptions, EnrollmentPairingHook, VirtualClock,
    WearableDescriptor, WearableEvent,
};
use bpmute::ipc::protocol::{
    AlertSeverity, DaemonAlert, DiscoveredWearable, EnrollAuthResponseParams, alert_ids,
};
use bpmute::paths::MapEnv;
use bpmute::sleep_engine::{ContactStatus, HrPacket, StreamState, StreamStatus};
use bpmute::tui::{
    ModalState, PairingPromptModalState, Tab, TuiApp, TuiCommand, TuiEvent, TuiIpcBridge,
    UnenrollButton, detect_systemd_service_installed,
};

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
fn test_mouse_wearables_footer_buttons_and_offline_guards() {
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

    let (e_col, bar_row) = find_text_coords(buf, "[e] Enroll").unwrap();
    assert_eq!(
        bar_row, 23,
        "Wearables actions should sit in the global footer"
    );
    let (n_col, _) = find_text_coords_in_row(buf, bar_row, "[n] Rename").unwrap();
    let (c_col, _) = find_text_coords_in_row(buf, bar_row, "[c] Reconnect").unwrap();
    let (u_col, _) = find_text_coords_in_row(buf, bar_row, "[u] Unenroll").unwrap();

    // [c] Reconnect dispatches WearableConnect for selected wearable
    assert_eq!(
        app.handle_event(mouse_left_click(c_col + 2, bar_row)),
        Some(TuiCommand::WearableConnect { id: "w-1".into() })
    );

    // [n] Rename opens RenameModal
    assert_eq!(app.handle_event(mouse_left_click(n_col + 2, bar_row)), None);
    assert!(matches!(
        app.modal,
        ModalState::RenameModal {
            ref wearable_id,
            ..
        } if wearable_id == "w-1"
    ));
    app.modal = ModalState::None;

    // [u] Unenroll opens UnenrollModal
    assert_eq!(app.handle_event(mouse_left_click(u_col + 2, bar_row)), None);
    assert!(matches!(app.modal, ModalState::UnenrollModal { .. }));
    app.modal = ModalState::None;

    // [e] Enroll opens EnrollmentModal and dispatches EnrollStart
    assert_eq!(
        app.handle_event(mouse_left_click(e_col + 2, bar_row)),
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

    // Offline guard: all footer action buttons ignore clicks
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
    for col in [e_col + 1, n_col + 1, c_col + 1, u_col + 1] {
        assert_eq!(app.handle_event(mouse_left_click(col, bar_row)), None);
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

    // 2. Click [e] Enroll in the footer
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (enroll_btn_col, enroll_btn_row) =
        find_text_coords(terminal.backend().buffer(), "[e] Enroll").unwrap();
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
