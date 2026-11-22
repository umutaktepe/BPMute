mod common;
use common::tui::*;

use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use bpmute::calibration::{BoundaryReason, CalibrationBoundaryRecord};
use bpmute::config::{PowerAction, ScheduleConfig, Sensitivity};
use bpmute::sleep_engine::CalibrationMaturity;
use bpmute::tui::{IpcEvent, ModalState, Tab, TuiApp, TuiCommand, TuiEvent};

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
        wal_size_bytes: 0,
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

    // 5. Mouse-wheel ScrollUp / ScrollDown over Core Preferences moves selected_setting_idx between 0..=6
    assert_eq!(app.handle_event(mouse_scroll_up(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 1);
    assert_eq!(app.handle_event(mouse_scroll_up(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 0);
    // Clamps at 0
    assert_eq!(app.handle_event(mouse_scroll_up(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 0);

    for expected in 1..=6 {
        assert_eq!(app.handle_event(mouse_scroll_down(ls_col, ls_row)), None);
        assert_eq!(app.selected_setting_idx, expected);
    }
    // Clamps at 6
    assert_eq!(app.handle_event(mouse_scroll_down(ls_col, ls_row)), None);
    assert_eq!(app.selected_setting_idx, 6);

    // Scrolling outside Core Preferences (e.g. over System Settings & Storage) does not move selection
    let (stor_col, stor_row) = find_text_coords(buf_off, "System Settings & Storage").unwrap();
    assert_eq!(app.handle_event(mouse_scroll_up(stor_col, stor_row)), None);
    assert_eq!(app.selected_setting_idx, 6);
}

#[test]
fn test_mouse_settings_controls_and_guards() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    app.active_config = Some(bpmute::config::Config::default());
    app.storage_stats = Some(bpmute::storage::StorageStats {
        db_size_bytes: 145_408,
        wal_size_bytes: 0,
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
async fn test_real_socket_settings_purge_rebaseline_and_repeated_wipe_db_size() {
    let mut h = RealSocketTuiHarness::spawn(3, true).await;

    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(h.app.active_tab, Tab::Settings);
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.storage_stats.is_some()
    })
    .await;

    let initial_stats = h.app.storage_stats.as_ref().unwrap().clone();
    assert_eq!(initial_stats.sessions_count, 3);
    assert!(initial_stats.raw_samples_count > 0);
    let size_initial = initial_stats.db_size_bytes;

    // 1. Purge Raw ('P') -> clears raw samples and shrinks DB size
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('P'))));
    let purge_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("PurgeRawModal should emit DataPurgeRaw");
    h.app.storage_stats = None;
    h.cmd_tx.send(purge_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.storage_stats
            .as_ref()
            .is_some_and(|s| s.raw_samples_count == 0)
    })
    .await;
    let after_purge_stats = h.app.storage_stats.as_ref().unwrap().clone();
    assert_eq!(after_purge_stats.sessions_count, 3);
    assert!(
        after_purge_stats.db_size_bytes < size_initial,
        "Purge Raw should shrink DB size: initial={size_initial}, after_purge={}",
        after_purge_stats.db_size_bytes
    );

    // 2. Soft Rebaseline ('B') + Undo ('U')
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('B'))));
    let rebaseline_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("SoftRebaselineModal should emit CalibrationSetBoundary");
    h.app.storage_stats = None;
    h.cmd_tx.send(rebaseline_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_boundaries.len() == 1
            && a.storage_stats
                .as_ref()
                .is_some_and(|s| s.active_calibration_boundary.is_some())
    })
    .await;
    assert_eq!(h.app.calibration_maturity, CalibrationMaturity::ColdStart);

    let undo_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Char('U'))))
        .expect("Undo should emit CalibrationUndoBoundary");
    h.app.storage_stats = None;
    h.cmd_tx.send(undo_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_boundaries
            .first()
            .is_some_and(|b| b.reverted_at_ms.is_some())
            && a.storage_stats
                .as_ref()
                .is_some_and(|s| s.active_calibration_boundary.is_none())
    })
    .await;

    // 3. Repeated Hard Wipe ('W' + "WIPE") -> shrinks DB size on first wipe and stays constant
    let size_before_wipe = h.app.storage_stats.as_ref().unwrap().db_size_bytes;
    let mut wipe_sizes = Vec::with_capacity(3);
    for _ in 0..3 {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('W'))));
        for c in "WIPE".chars() {
            h.app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
        }
        let wipe_cmd = h
            .app
            .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
            .expect("HardWipeModal should emit DataHardWipe");
        h.app.storage_stats = None;
        h.cmd_tx.send(wipe_cmd).await.unwrap();
        wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
            a.history_sessions.is_empty() && a.storage_stats.is_some()
        })
        .await;
        wipe_sizes.push(h.app.storage_stats.as_ref().unwrap().db_size_bytes);
    }

    assert!(
        wipe_sizes[0] < size_before_wipe,
        "First WIPE should shrink DB size: before={size_before_wipe}, after_w1={}",
        wipe_sizes[0]
    );
    assert_eq!(
        wipe_sizes[1], wipe_sizes[0],
        "Second WIPE must not grow DB size: {wipe_sizes:?}"
    );
    assert_eq!(
        wipe_sizes[2], wipe_sizes[1],
        "Third WIPE must not grow DB size: {wipe_sizes:?}"
    );

    h.shutdown().await;
}

#[test]
fn test_settings_active_hours_row_and_modal_open() {
    use bpmute::config::ClockTime;

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;

    let cfg = bpmute::config::Config {
        schedule: ScheduleConfig {
            active_hours_enabled: false,
            start: ClockTime::new(22, 0).unwrap(),
            end: ClockTime::new(8, 0).unwrap(),
        },
        ..Default::default()
    };
    app.active_config = Some(cfg);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_off = buffer_to_string(terminal.backend().buffer());
    assert!(
        out_off.contains("Active Hours") && out_off.contains("[ Off 22:00-08:00 ]"),
        "Core Preferences must render disabled Active Hours within 15 chars:\n{out_off}"
    );
    assert!(
        out_off.contains("[Space/Enter] Edit/Toggle"),
        "Core Preferences must display updated Edit/Toggle hint:\n{out_off}"
    );
    assert!(
        out_off.contains("Advanced Settings (Read-Only)")
            && out_off.contains("[E] Edit in $EDITOR"),
        "Advanced Settings must remain intact below Core Preferences at 80x24:\n{out_off}"
    );

    // Enabled state displays HH:MM-HH:MM without Off prefix
    app.active_config
        .as_mut()
        .unwrap()
        .schedule
        .active_hours_enabled = true;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_on = buffer_to_string(terminal.backend().buffer());
    assert!(
        out_on.contains("[ 22:00-08:00 ]") && !out_on.contains("Off 22:00-08:00"),
        "Core Preferences must render enabled Active Hours as HH:MM-HH:MM:\n{out_on}"
    );

    // Navigate down 6 times to select Active Hours (index 6)
    for _ in 0..6 {
        assert_eq!(
            app.handle_event(TuiEvent::Key(key(KeyCode::Char('j')))),
            None
        );
    }
    assert_eq!(app.selected_setting_idx, 6);

    // Offline guard: Space, Enter, and second click on Active Hours do not open modal
    app.daemon_connected = false;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(' ')))),
        None
    );
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    assert_eq!(app.modal, ModalState::None);
    let (ah_col, ah_row) = find_text_coords(terminal.backend().buffer(), "Active Hours").unwrap();
    assert_eq!(app.handle_event(mouse_left_click(ah_col + 2, ah_row)), None);
    assert_eq!(app.modal, ModalState::None);

    // Online: Space opens ActiveHoursModal
    app.daemon_connected = true;
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(' ')))),
        None
    );
    assert!(matches!(app.modal, ModalState::ActiveHoursModal { .. }));
    app.modal = ModalState::None;

    // Online: Enter opens ActiveHoursModal
    assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    assert!(matches!(app.modal, ModalState::ActiveHoursModal { .. }));
    app.modal = ModalState::None;

    // Online: Two-click HitMap pattern (select another row, click Active Hours to select, click again to open)
    app.selected_setting_idx = 0;
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (ah_col2, ah_row2) = find_text_coords(terminal.backend().buffer(), "Active Hours").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(ah_col2 + 2, ah_row2)),
        None
    );
    assert_eq!(app.selected_setting_idx, 6);
    assert_eq!(app.modal, ModalState::None);
    assert_eq!(
        app.handle_event(mouse_left_click(ah_col2 + 2, ah_row2)),
        None
    );
    assert!(matches!(app.modal, ModalState::ActiveHoursModal { .. }));
}

#[test]
fn test_settings_active_hours_modal_navigation_and_cancel() {
    use bpmute::config::ClockTime;
    use bpmute::tui::app::ActiveHoursField;

    for (ascii, no_color) in [(false, false), (true, true)] {
        let mut app = TuiApp::new(false, ascii, no_color);
        app.daemon_connected = true;
        app.active_tab = Tab::Settings;
        let cfg = bpmute::config::Config {
            schedule: ScheduleConfig {
                active_hours_enabled: false,
                start: ClockTime::new(22, 30).unwrap(),
                end: ClockTime::new(7, 15).unwrap(),
            },
            ..Default::default()
        };
        app.active_config = Some(cfg);
        app.selected_setting_idx = 6;

        // Open ActiveHoursModal -> focuses Enabled and seeds Start/End even when disabled
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                enabled: false,
                ref start_input,
                ref end_input,
                active_field: ActiveHoursField::Enabled,
                error_message: None,
                ..
            } if start_input == "22:30" && end_input == "07:15"
        ));
        assert!(!app.is_input_focused());

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let out = buffer_to_string(terminal.backend().buffer());
        assert!(
            out.contains("Active Hours")
                && out.contains("[ ]")
                && out.contains("22:30")
                && out.contains("07:15")
                && out.contains("[ Save ]")
                && out.contains("[ Cancel ]"),
            "Modal must render Enabled, Start, End, Save, Cancel:\n{out}"
        );
        // Footer hints reflect ActiveHoursModal
        let expected_step_hint = if ascii { "[<-/->]" } else { "[←/→]" };
        assert!(
            out.contains("[Tab] Field")
                && out.contains("[Space] Toggle")
                && out.contains(expected_step_hint)
                && out.contains("[Enter] Save")
                && out.contains("[Esc] Cancel"),
            "Footer must render ActiveHoursModal hints (ascii={ascii}):\n{out}"
        );

        // Backdrop click outside modal does not dismiss or trigger background Core Preferences
        assert_eq!(app.handle_event(mouse_left_click(0, 0)), None);
        assert!(matches!(app.modal, ModalState::ActiveHoursModal { .. }));

        // Space toggles Enabled when Enabled is focused
        assert_eq!(
            app.handle_event(TuiEvent::Key(key(KeyCode::Char(' ')))),
            None
        );
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal { enabled: true, .. }
        ));

        // Tab cycles Enabled -> Start -> End -> Save -> Cancel -> Enabled
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Tab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Start,
                ..
            }
        ));
        assert!(app.is_input_focused());

        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Tab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::End,
                ..
            }
        ));
        assert!(app.is_input_focused());

        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Tab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Save,
                ..
            }
        ));
        assert!(!app.is_input_focused());

        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Tab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Cancel,
                ..
            }
        ));

        // BackTab reverses Cancel -> Save -> End -> Start -> Enabled
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::BackTab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Save,
                ..
            }
        ));
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::BackTab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::End,
                ..
            }
        ));
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::BackTab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Start,
                ..
            }
        ));
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::BackTab))), None);
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Enabled,
                ..
            }
        ));

        // Mouse click on Enabled toggles it, mouse click on Start/End focuses input
        terminal
            .draw(|f| app.render(f.area(), f.buffer_mut()))
            .unwrap();
        let (chk_col, chk_row) = find_text_coords(terminal.backend().buffer(), "[x]").unwrap();
        assert_eq!(
            app.handle_event(mouse_left_click(chk_col + 1, chk_row)),
            None
        );
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                enabled: false,
                active_field: ActiveHoursField::Enabled,
                ..
            }
        ));

        let (start_col, start_row) =
            find_text_coords(terminal.backend().buffer(), "[ 22:30 ]").unwrap();
        assert_eq!(
            app.handle_event(mouse_left_click(start_col + 2, start_row)),
            None
        );
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Start,
                ..
            }
        ));

        let (end_col, end_row) =
            find_text_coords(terminal.backend().buffer(), "[ 07:15 ]").unwrap();
        assert_eq!(
            app.handle_event(mouse_left_click(end_col + 2, end_row)),
            None
        );
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::End,
                ..
            }
        ));

        // Clicking [ Cancel ] dismisses without emitting command
        let (canc_col, canc_row) =
            find_text_coords(terminal.backend().buffer(), "[ Cancel ]").unwrap();
        assert_eq!(
            app.handle_event(mouse_left_click(canc_col + 2, canc_row)),
            None
        );
        assert_eq!(app.modal, ModalState::None);

        // Reopen, Tab to Cancel, Enter on Cancel dismisses without command
        app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
        for _ in 0..4 {
            app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
        }
        assert!(matches!(
            app.modal,
            ModalState::ActiveHoursModal {
                active_field: ActiveHoursField::Cancel,
                ..
            }
        ));
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
        assert_eq!(app.modal, ModalState::None);

        // Reopen and press Esc -> dismisses without command
        app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
        assert!(matches!(app.modal, ModalState::ActiveHoursModal { .. }));
        assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Esc))), None);
        assert_eq!(app.modal, ModalState::None);
    }
}

#[test]
fn test_settings_active_hours_hybrid_time_editing_and_stepping() {
    use bpmute::config::ClockTime;
    use bpmute::tui::app::ActiveHoursField;

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    let cfg = bpmute::config::Config {
        schedule: ScheduleConfig {
            active_hours_enabled: true,
            start: ClockTime::new(23, 30).unwrap(),
            end: ClockTime::new(0, 15).unwrap(),
        },
        ..Default::default()
    };
    app.active_config = Some(cfg);
    app.selected_setting_idx = 6;

    // Open modal and Tab to Start
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert!(app.is_input_focused());

    // Global shortcuts like 'q', '?', '1', 'j' must be suppressed while Start is focused
    for ch in ['q', '?', '1', 'j', 'x'] {
        assert_eq!(
            app.handle_event(TuiEvent::Key(key(KeyCode::Char(ch)))),
            None
        );
    }
    assert!(!app.should_quit);
    assert_eq!(app.active_tab, Tab::Settings);
    assert_eq!(app.selected_setting_idx, 6);
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal {
            ref start_input,
            active_field: ActiveHoursField::Start,
            ..
        } if start_input == "23:30"
    ));

    // Left/Right 15-min stepping with midnight wraparound: 23:30 -> 23:45 -> 00:00 -> 23:45
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "23:45"
    ));
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "00:00"
    ));
    app.handle_event(TuiEvent::Key(key(KeyCode::Left)));
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "23:45"
    ));

    // Direct typing + Backspace on Start: change 23:45 -> 21:05 (and verify 5-char cap)
    for _ in 0..5 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for ch in "21:059".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(ch))));
    }
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "21:05"
    ));

    // Typing a partial/invalid string then pressing Right falls back to last valid (21:05 -> 21:20)
    app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "21:"
    ));
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "21:20"
    ));

    // Render and test mouse-wheel stepping over End (00:15 -> ScrollDown -> 00:00 -> ScrollDown -> 23:45 -> ScrollUp -> 00:00)
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (end_col, end_row) = find_text_coords(terminal.backend().buffer(), "[ 00:15 ]").unwrap();

    // Scrolling outside Start/End (over background Core Preferences) must NOT change selected_setting_idx
    assert_eq!(app.handle_event(mouse_scroll_up(2, 3)), None);
    assert_eq!(app.selected_setting_idx, 6);

    assert_eq!(
        app.handle_event(mouse_scroll_down(end_col + 2, end_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal {
            ref end_input,
            active_field: ActiveHoursField::End,
            ..
        } if end_input == "00:00"
    ));
    assert_eq!(
        app.handle_event(mouse_scroll_down(end_col + 2, end_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref end_input, .. } if end_input == "23:45"
    ));
    assert_eq!(
        app.handle_event(mouse_scroll_up(end_col + 2, end_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref end_input, .. } if end_input == "00:00"
    ));

    // Also test mouse-wheel stepping over Start with invalid text ("99:99") recovers from last_valid_start (21:20)
    let (start_col, start_row) =
        find_text_coords(terminal.backend().buffer(), "[ 21:20 ]").unwrap();
    app.handle_event(mouse_left_click(start_col + 2, start_row));
    for _ in 0..5 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for ch in "99:99".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(ch))));
    }
    assert_eq!(
        app.handle_event(mouse_scroll_up(start_col + 2, start_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal { ref start_input, .. } if start_input == "21:35"
    ));
}

#[test]
fn test_settings_active_hours_validation_submission_and_roundtrip() {
    use bpmute::config::{ClockTime, ConfigPatch, SchedulePatch};

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::Settings;
    let cfg = bpmute::config::Config {
        schedule: ScheduleConfig {
            active_hours_enabled: false,
            start: ClockTime::new(22, 0).unwrap(),
            end: ClockTime::new(8, 0).unwrap(),
        },
        ..Default::default()
    };
    app.active_config = Some(cfg.clone());
    app.selected_setting_idx = 6;

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    // 1. Two-keystroke flow: Enter to open, Space to toggle Enabled -> true, Enter to save
    assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    assert_eq!(
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(' ')))),
        None
    );
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::ConfigUpdate(ConfigPatch {
            schedule: Some(SchedulePatch {
                active_hours_enabled: Some(true),
                start: Some(ClockTime::new(22, 0).unwrap()),
                end: Some(ClockTime::new(8, 0).unwrap()),
            }),
            ..Default::default()
        }))
    );
    assert_eq!(app.modal, ModalState::None);

    // Subsequent ConfigLoaded (from ConfigUpdated IPC event) updates rendered row
    let mut updated_cfg = cfg.clone();
    updated_cfg.schedule.active_hours_enabled = true;
    app.handle_ipc(IpcEvent::ConfigLoaded(updated_cfg));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_enabled = buffer_to_string(terminal.backend().buffer());
    assert!(
        out_enabled.contains("[ 22:00-08:00 ]"),
        "Updated config must render enabled Active Hours:\n{out_enabled}"
    );

    // 2. Invalid HH:MM keeps modal open with inline ⚠ error and emits no command
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab))); // Focus Start
    for _ in 0..5 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for ch in "25:00".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(ch))));
    }
    assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal {
            error_message: Some(_),
            ..
        }
    ));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_err1 = buffer_to_string(terminal.backend().buffer());
    assert!(
        out_err1.contains("⚠"),
        "Invalid HH:MM must show inline ⚠ error:\n{out_err1}"
    );

    // Typing clears stale error; fix Start to "08:00" (matching End "08:00") while Enabled == true
    for _ in 0..5 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal {
            error_message: None,
            ..
        }
    ));
    for ch in "08:00".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(ch))));
    }

    // 3. enabled == true && start == end rejected on [ Save ] click with inline ⚠ error
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (save_col, save_row) = find_text_coords(terminal.backend().buffer(), "[ Save ]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(save_col + 2, save_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal {
            error_message: Some(ref msg),
            ..
        } if msg.contains("Start and End must differ when Active Hours are enabled")
    ));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_err2 = buffer_to_string(terminal.backend().buffer());
    assert!(
        out_err2.contains("⚠ Start and End must differ when Active Hours are enabled"),
        "Equal Start and End when enabled must show inline ⚠ error:\n{out_err2}"
    );

    // 4. Unchecking Enabled allows start == end and submits via footer [Enter] Save
    let (chk_col, chk_row) = find_text_coords(terminal.backend().buffer(), "[x]").unwrap();
    assert_eq!(
        app.handle_event(mouse_left_click(chk_col + 1, chk_row)),
        None
    );
    assert!(matches!(
        app.modal,
        ModalState::ActiveHoursModal {
            enabled: false,
            error_message: None,
            ..
        }
    ));

    // Also verify 60x18 minimum terminal renders cleanly with modal open
    let mut term_min = Terminal::new(TestBackend::new(60, 18)).unwrap();
    term_min
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_min = buffer_to_string(term_min.backend().buffer());
    assert!(out_min.contains("Active Hours") && out_min.contains("[ Save ]"));

    // Verify --ascii error rendering uses ASCII `!` instead of Unicode `⚠`
    let mut app_ascii = TuiApp::new(false, true, true);
    app_ascii.daemon_connected = true;
    app_ascii.active_tab = Tab::Settings;
    app_ascii.active_config = Some(cfg.clone());
    app_ascii.selected_setting_idx = 6;
    app_ascii.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    app_ascii.handle_event(TuiEvent::Key(key(KeyCode::Char(' '))));
    app_ascii.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    for _ in 0..5 {
        app_ascii.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for ch in "08:00".chars() {
        app_ascii.handle_event(TuiEvent::Key(key(KeyCode::Char(ch))));
    }
    app_ascii.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    let mut term_ascii = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term_ascii
        .draw(|f| app_ascii.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_ascii_err = buffer_to_string(term_ascii.backend().buffer());
    assert!(
        out_ascii_err.contains(" ! Start and End must differ when Active Hours are enabled"),
        "ASCII mode must render `!` error prefix:\n{out_ascii_err}"
    );
    assert!(
        out_ascii_err.is_ascii(),
        "ASCII mode ActiveHoursModal error output must be strictly ASCII:\n{out_ascii_err}"
    );

    // Re-render at 80x24 and click footer [Enter] Save
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (f_save_col, f_save_row) =
        find_footer_coords(terminal.backend().buffer(), "[Enter] Save").unwrap();
    let cmd_disabled = app.handle_event(mouse_left_click(f_save_col + 1, f_save_row));
    assert_eq!(
        cmd_disabled,
        Some(TuiCommand::ConfigUpdate(ConfigPatch {
            schedule: Some(SchedulePatch {
                active_hours_enabled: Some(false),
                start: Some(ClockTime::new(8, 0).unwrap()),
                end: Some(ClockTime::new(8, 0).unwrap()),
            }),
            ..Default::default()
        }))
    );
    assert_eq!(app.modal, ModalState::None);

    // ConfigLoaded updates rendered row to Off 08:00-08:00
    let mut final_cfg = cfg;
    final_cfg.schedule = ScheduleConfig {
        active_hours_enabled: false,
        start: ClockTime::new(8, 0).unwrap(),
        end: ClockTime::new(8, 0).unwrap(),
    };
    app.handle_ipc(IpcEvent::ConfigLoaded(final_cfg));
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let out_final = buffer_to_string(terminal.backend().buffer());
    assert!(
        out_final.contains("[ Off 08:00-08:00 ]"),
        "Updated disabled config must render Off 08:00-08:00:\n{out_final}"
    );
}

#[test]
fn test_soft_rebaseline_modal_accepts_up_to_140_bpm() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.active_tab, Tab::Settings);

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('B'))));
    assert!(matches!(app.modal, ModalState::SoftRebaselineModal { .. }));
    for c in "135.0".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::CalibrationSetBoundary {
            effective_ts_ms: None,
            seed_sleep_hr: Some(135.0),
            reset_floors: false,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn test_day1_seed_modal_validation_and_real_socket_roundtrip_e2e() {
    let mut h = RealSocketTuiHarness::spawn(1, false).await;
    h.cmd_tx.send(TuiCommand::DataStats).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.storage_stats.is_some()
    })
    .await;
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(h.app.active_tab, Tab::Settings);

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| h.app.render(f.area(), f.buffer_mut()))
        .unwrap();

    let (seed_col, seed_row) =
        find_text_coords(terminal.backend().buffer(), "[S] Set Seed").unwrap();
    assert_eq!(
        h.app.handle_event(mouse_left_click(seed_col + 2, seed_row)),
        None
    );
    assert!(matches!(h.app.modal, ModalState::Day1SeedModal { .. }));

    // Out-of-range BPM (> 140) stays open with validation error
    for c in "150".chars() {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(h.app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    assert!(matches!(
        h.app.modal,
        ModalState::Day1SeedModal {
            error_message: Some(_),
            ..
        }
    ));

    // Fix to "135.0" and submit over real socket -> sets seed without inserting boundary
    for _ in 0..3 {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for c in "135.0".chars() {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    let set_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("Day1SeedModal should emit CalibrationSetSeed(Some(135.0))");
    assert_eq!(set_cmd, TuiCommand::CalibrationSetSeed(Some(135.0)));
    h.cmd_tx.send(set_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.day1_seed_sleep_hr
            .is_some_and(|hr| (hr - 135.0).abs() < 1e-6)
    })
    .await;
    assert!(
        h.app.history_boundaries.is_empty(),
        "CalibrationSetSeed must not insert a CalibrationBoundary"
    );

    // Re-open via 'S' -> pre-populated with "135.0"; clear and submit CalibrationSetSeed(None)
    h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('S'))));
    assert!(matches!(
        h.app.modal,
        ModalState::Day1SeedModal {
            ref seed_sleep_hr_input,
            ..
        } if seed_sleep_hr_input == "135.0"
    ));
    for _ in 0..5 {
        h.app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    let clear_cmd = h
        .app
        .handle_event(TuiEvent::Key(key(KeyCode::Enter)))
        .expect("Day1SeedModal should emit CalibrationSetSeed(None)");
    assert_eq!(clear_cmd, TuiCommand::CalibrationSetSeed(None));
    h.cmd_tx.send(clear_cmd).await.unwrap();
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.day1_seed_sleep_hr.is_none()
    })
    .await;

    h.shutdown().await;
}
