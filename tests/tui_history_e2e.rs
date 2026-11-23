mod common;
use common::tui::*;

use std::fs;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use bpmute::calibration::{
    BoundaryReason, CalibrationBoundaryRecord, FeedbackVerdict, SessionFeedbackRecord,
    SessionLedgerRecord,
};
use bpmute::config::{CliOverrides, ClockTime, LoadedConfig, PowerAction, Sensitivity};
use bpmute::daemon::{
    DaemonHandle, DaemonOptions, VirtualClock, WearableDescriptor, WearableEvent,
};
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::{EpochUpdateEvent, HrSampleEvent, SessionStartedEvent};
use bpmute::sleep_engine::{
    ContactStatus, DetectorState, Epoch, EpochFeatures, HrPacket, SessionEndReason, SessionSummary,
    StreamState, StreamStatus,
};
use bpmute::storage::{SessionFullDetail, SessionStatus, WearableInfo};
use bpmute::tui::{IpcEvent, ModalState, Tab, TuiApp, TuiCommand, TuiEvent, TuiIpcBridge};

#[test]
fn test_history_tab_master_detail_chart_and_boundary_dividers() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let mut s1 = make_history_session(
        "sess-001",
        "Polar H10",
        1_700_100_000_000,
        1_700_102_700_000, // 45 min
        SessionStatus::Closed,
        30,
    );
    s1.end_reason = Some(SessionEndReason::HostSuspend);
    s1.warmup_mu_hr = Some(72.0);
    s1.sleep_mu_hr = Some(58.0);
    s1.onset_confirmed_ts_ms = Some(1_700_100_900_000); // 15m in
    s1.sleeping_ts_ms = Some(1_700_101_800_000); // 30m in
    s1.raw_samples_count = 2700;

    let boundary = CalibrationBoundaryRecord {
        id: "bnd-001".into(),
        effective_ts_ms: 1_700_050_000_000,
        reason: BoundaryReason::ManualRebaseline,
        seed_sleep_hr: Some(55.0),
        reset_floors: false,
        created_at_ms: 1_700_050_000_000,
        reverted_at_ms: None,
    };

    let mut s2 = make_history_session(
        "sess-002",
        "GT 5",
        1_700_000_000_000,
        1_700_003_600_000, // 60 min
        SessionStatus::Closed,
        40,
    );
    s2.wearable_id = "w-2".into();
    s2.wearable_mac_masked = "..:44:55".into();
    s2.had_rr = false;
    s2.warmup_mu_hr = Some(68.0);
    s2.auto_anomalous = true;
    s2.fp_count = 1;
    s2.raw_samples_count = 0;
    s2.feedback = Some(SessionFeedbackRecord {
        id: "fb-002".into(),
        session_id: "sess-002".into(),
        verdict: Some(FeedbackVerdict::FalsePositive),
        manual_onset_ts_ms: None,
        manual_avg_sleep_hr: None,
        notes: None,
        updated_at_ms: 1_700_004_000_000,
    });

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
            pre_candidate_mu_hr: None,
            pre_candidate_sigma_hr: None,
            pre_candidate_ln_rmssd: None,
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

    let mut active_session = make_history_session(
        "sess-active",
        "Polar H10",
        1_700_200_000_000,
        1_700_201_000_000,
        SessionStatus::Active,
        10,
    );
    active_session.raw_samples_count = 1000;

    let mut closed_session = make_history_session(
        "sess-closed",
        "Polar H10",
        1_700_100_000_000,
        1_700_103_600_000, // 1 hour
        SessionStatus::Closed,
        30,
    );
    closed_session.end_reason = Some(SessionEndReason::HostSuspend);
    closed_session.warmup_mu_hr = Some(72.0);
    closed_session.sleep_mu_hr = Some(58.0);
    closed_session.onset_confirmed_ts_ms = Some(1_700_101_800_000);
    closed_session.sleeping_ts_ms = Some(1_700_102_400_000);
    closed_session.raw_samples_count = 2700;

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
    let expected_clock: ClockTime = local_valid.parse().unwrap();
    assert!(matches!(
        submit_cmd,
        Some(TuiCommand::SessionSetFeedback {
            ref session_id,
            verdict: Some(FeedbackVerdict::FalsePositive),
            onset_hhmm: Some(ct),
            avg_sleep_hr: Some(58.5),
            clear: false,
        }) if session_id == "sess-closed" && ct == expected_clock
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
                        pre_candidate_mu_hr: None,
                        pre_candidate_sigma_hr: None,
                        pre_candidate_ln_rmssd: None,
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
                        pre_candidate_mu_hr: None,
                        pre_candidate_sigma_hr: None,
                        pre_candidate_ln_rmssd: None,
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
    let (hr_col, hr_row) = find_text_coords(terminal.backend().buffer(), "[ 30–140 ]").unwrap();
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
            onset_hhmm: Some(valid_hhmm.parse().unwrap()),
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

#[test]
fn test_history_braille_trace_overlay_gaps_and_markers() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let start_ts = 1_700_100_000_000i64;
    let end_ts = start_ts + 40 * 60_000; // 40 minutes
    let onset_ts = start_ts + 10 * 60_000; // 25% across
    let trigger_ts = start_ts + 30 * 60_000; // 75% across
    let manual_ts = start_ts + 20 * 60_000; // 50% across (inside the gap so marker stands clear)

    let mut entry = make_history_session(
        "sess-hist-trace",
        "Polar H10",
        start_ts,
        end_ts,
        SessionStatus::Closed,
        100,
    );
    entry.end_reason = Some(SessionEndReason::HostSuspend);
    entry.warmup_mu_hr = Some(64.0);
    entry.sleep_mu_hr = Some(56.0);
    entry.onset_confirmed_ts_ms = Some(onset_ts);
    entry.sleeping_ts_ms = Some(trigger_ts);
    entry.raw_samples_count = 0;

    // Build epochs at 15s cadence:
    // - 0..14m: HR drops 64 -> 56 (P(Sleep) rises 0.0 -> 0.8), crossing around midpoint
    // - 14..26m: missing (> 30s gap) AND one invalid hr_bpm_60s <= 20.0 epoch at 20m
    // - 26..40m: HR stays 56..58, P(Sleep) stays 0.9
    let mut epochs = Vec::new();
    let mut idx = 0u64;
    for sec in (0..=14 * 60).step_by(15) {
        let frac = sec as f64 / (14.0 * 60.0);
        let hr = 64.0 - frac * 8.0;
        let p_sleep = frac * 0.8;
        epochs.push(Epoch {
            session_index: idx,
            ts_ms: start_ts + sec * 1_000,
            features: EpochFeatures {
                hr_bpm_60s: hr,
                z_hr: -1.0,
                hr_slope_4m: -0.2,
                r_var_120s: 0.3,
                ln_rmssd_60s: Some(3.0),
                delta_ln_rmssd_60s: Some(0.1),
                rmssd_sdnn_120s: Some(0.8),
            },
            hrv_valid: true,
            rr_artifact_frac: 0.0,
            coverage: 1.0,
            flatline_stale: false,
            accumulators_frozen: false,
            baseline_mu_hr: 64.0,
            baseline_sigma_hr: 2.0,
            baseline_frozen: true,
            p_active_wake: 0.1,
            p_quiet_wake: 0.9 - p_sleep,
            p_sleep,
            state: DetectorState::Warmup,
        });
        idx += 1;
    }
    // Invalid HR epoch at 20m (hr <= 20.0) - must NOT plot HR or skew Y-axis
    epochs.push(Epoch {
        session_index: idx,
        ts_ms: manual_ts,
        features: EpochFeatures {
            hr_bpm_60s: 10.0,
            z_hr: 0.0,
            hr_slope_4m: 0.0,
            r_var_120s: 0.0,
            ln_rmssd_60s: None,
            delta_ln_rmssd_60s: None,
            rmssd_sdnn_120s: None,
        },
        hrv_valid: false,
        rr_artifact_frac: 1.0,
        coverage: 0.0,
        flatline_stale: true,
        accumulators_frozen: true,
        baseline_mu_hr: 64.0,
        baseline_sigma_hr: 2.0,
        baseline_frozen: true,
        p_active_wake: 0.0,
        p_quiet_wake: 0.5,
        p_sleep: 0.5,
        state: DetectorState::Candidate,
    });
    idx += 1;
    for sec in (26 * 60..=40 * 60).step_by(15) {
        epochs.push(Epoch {
            session_index: idx,
            ts_ms: start_ts + sec * 1_000,
            features: EpochFeatures {
                hr_bpm_60s: 57.0,
                z_hr: -2.0,
                hr_slope_4m: 0.0,
                r_var_120s: 0.2,
                ln_rmssd_60s: Some(3.4),
                delta_ln_rmssd_60s: Some(0.2),
                rmssd_sdnn_120s: Some(0.9),
            },
            hrv_valid: true,
            rr_artifact_frac: 0.0,
            coverage: 1.0,
            flatline_stale: false,
            accumulators_frozen: false,
            baseline_mu_hr: 64.0,
            baseline_sigma_hr: 2.0,
            baseline_frozen: true,
            p_active_wake: 0.02,
            p_quiet_wake: 0.08,
            p_sleep: 0.9,
            state: DetectorState::Confirmed,
        });
        idx += 1;
    }

    app.history_sessions = vec![entry.clone()];
    app.selected_session_idx = 0;
    app.selected_session_detail = Some(SessionFullDetail {
        session: SessionLedgerRecord {
            id: entry.id.clone(),
            wearable_id: entry.wearable_id.clone(),
            start_ts_ms: start_ts,
            end_ts_ms: end_ts,
            end_reason: SessionEndReason::HostSuspend,
            detection_sensitivity: Sensitivity::Balanced,
            had_rr: true,
            warmup_mu_hr: Some(64.0),
            warmup_sigma_hr: Some(2.0),
            warmup_ln_rmssd: Some(3.0),
            pre_candidate_mu_hr: None,
            pre_candidate_sigma_hr: None,
            pre_candidate_ln_rmssd: None,
            onset_candidate_ts_ms: Some(onset_ts - 60_000),
            onset_confirmed_ts_ms: Some(onset_ts),
            sleeping_ts_ms: Some(trigger_ts),
            sleep_mu_hr: Some(56.0),
            onset_hr_bpm: Some(59.0),
            delta_hr_onset: Some(5.0),
            sleep_ln_rmssd: Some(3.4),
            sleep_hr_var_ratio: Some(0.2),
            typical_sqi: Some(0.95),
            post_trigger_sleep_sec: 600,
            trigger_power_action: PowerAction::Suspend,
            rapid_resume_within_5m: false,
            grace_cancel_count: 0,
            rapid_wake_fp_count: 0,
            snooze_count: 0,
            auto_anomalous: false,
            epochs_count: epochs.len() as u32,
            raw_samples_count: 0,
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
            id: "fb-h".into(),
            session_id: entry.id.clone(),
            verdict: None,
            manual_onset_ts_ms: Some(manual_ts),
            manual_avg_sleep_hr: None,
            notes: None,
            updated_at_ms: end_ts + 10_000,
        }),
        epochs,
        has_raw_samples: false,
    });

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = terminal.backend().buffer();
    let out = buffer_to_string(buf);

    // 1. Dynamic Y-axis auto-scaling (56..64 -> padded 54..66) and right-side P(Sleep) 100%/0% scale
    let (top_bpm_x, top_row) =
        find_text_coords(buf, "66bpm").expect("History auto-scaled 66bpm label");
    let (_, bot_row) = find_text_coords(buf, "54bpm").expect("History auto-scaled 54bpm label");
    let (p100_x, p100_row) = find_text_coords(buf, "100%").expect("Right-side 100% label");
    let (_, p0_row) = find_text_coords(buf, "  0%").expect("Right-side 0% label");
    assert_eq!(p100_row, top_row);
    assert_eq!(p0_row, bot_row);
    assert!(p100_x + 4 < buf.area.width, "100% must fit inside border");

    let chart_left = top_bpm_x + 7;
    let chart_right = p100_x - 2;

    // 2. Both HR (Green) and P(Sleep) (Magenta) Braille curves render, and overlapping cells keep HR fg priority
    let mut hr_braille_cells = 0usize;
    let mut p_sleep_braille_cells = 0usize;
    for y in top_row..=bot_row {
        for x in chart_left..=chart_right {
            let cell = &buf[(x, y)];
            if cell
                .symbol()
                .chars()
                .next()
                .is_some_and(is_non_blank_braille)
            {
                if cell.fg == Color::Green {
                    hr_braille_cells += 1;
                } else if cell.fg == Color::Magenta {
                    p_sleep_braille_cells += 1;
                }
            }
        }
    }
    assert!(
        hr_braille_cells > 5 && p_sleep_braille_cells > 5,
        "Expected Braille cells for both HR ({hr_braille_cells}) and P(Sleep) ({p_sleep_braille_cells}):\n{out}"
    );

    // 3. Gap columns around 15..18m and 22..25m (between 0..14m and 26..40m) must have NO Braille dots
    let chart_span = (chart_right - chart_left) as f64;
    let gap_col_a = chart_left + (0.42 * chart_span).round() as u16;
    let gap_col_b = chart_left + (0.58 * chart_span).round() as u16;
    for gx in [gap_col_a, gap_col_b] {
        let any_braille = (top_row..=bot_row).any(|y| {
            buf[(gx, y)]
                .symbol()
                .chars()
                .next()
                .is_some_and(is_non_blank_braille)
        });
        assert!(
            !any_braille,
            "Gap column x={gx} must not bridge across >30s epoch outage:\n{out}"
        );
    }

    // 4. [O], [T], [M] vertical markers sit on top layer (all plot rows carry marker glyph)
    let (o_col, o_label_row) = find_text_coords(buf, "[O]").unwrap();
    let (t_col, _) = find_text_coords(buf, "[T]").unwrap();
    let (m_col, _) = find_text_coords(buf, "[M]").unwrap();
    assert_eq!(o_label_row, top_row - 1);
    assert_eq!(buf[(o_col + 1, top_row)].symbol(), "▼");
    assert_eq!(buf[(t_col + 1, top_row)].symbol(), "T");
    assert_eq!(buf[(m_col + 1, top_row)].symbol(), "M");
    for y in (top_row + 1)..=bot_row {
        assert_eq!(buf[(o_col + 1, y)].symbol(), "|");
        assert_eq!(buf[(t_col + 1, y)].symbol(), "|");
        assert_eq!(buf[(m_col + 1, y)].symbol(), "|");
    }

    // 5. Compact 60x18 layout renders History without panic or border overflow
    let mut compact_term = Terminal::new(TestBackend::new(60, 18)).unwrap();
    compact_term
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();

    // 6. Equal-timestamp History session & invalid-only HR falls back to 50..100 without panicking
    app.history_sessions[0].end_ts_ms = app.history_sessions[0].start_ts_ms;
    app.history_sessions[0].warmup_mu_hr = None;
    app.history_sessions[0].sleep_mu_hr = None;
    if let Some(detail) = app.selected_session_detail.as_mut() {
        for ep in &mut detail.epochs {
            ep.ts_ms = app.history_sessions[0].start_ts_ms;
            ep.features.hr_bpm_60s = 15.0; // <= 20.0 invalid
        }
    }
    terminal
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let eq_hist_out = buffer_to_string(terminal.backend().buffer());
    assert!(eq_hist_out.contains("100bpm"));
    assert!(eq_hist_out.contains("50bpm"));
}

#[test]
fn test_trace_ascii_fallback_no_color_modifiers_and_braille_compositing() {
    use ratatui::style::Modifier;

    // 1. Braille bitwise-OR compositing + HR foreground priority in History
    let mut comp_app = TuiApp::new(false, false, false);
    comp_app.daemon_connected = true;
    comp_app.active_tab = Tab::History;
    let start_ts = 1_700_100_000_000i64;
    let end_ts = start_ts + 10 * 60_000;
    let mut comp_entry = make_history_session(
        "sess-comp",
        "Polar H10",
        start_ts,
        end_ts,
        SessionStatus::Closed,
        41,
    );
    comp_entry.end_reason = Some(SessionEndReason::HostSuspend);
    comp_entry.warmup_mu_hr = Some(64.0);
    comp_entry.sleep_mu_hr = Some(56.0);
    comp_entry.raw_samples_count = 0;
    let comp_epochs: Vec<Epoch> = (0..=40)
        .map(|i| Epoch {
            session_index: i as u64,
            ts_ms: start_ts + (i as i64) * 15_000,
            features: EpochFeatures {
                hr_bpm_60s: 62.0,
                z_hr: -0.5,
                hr_slope_4m: 0.0,
                r_var_120s: 0.3,
                ln_rmssd_60s: Some(3.0),
                delta_ln_rmssd_60s: Some(0.0),
                rmssd_sdnn_120s: Some(0.8),
            },
            hrv_valid: true,
            rr_artifact_frac: 0.0,
            coverage: 1.0,
            flatline_stale: false,
            accumulators_frozen: false,
            baseline_mu_hr: 64.0,
            baseline_sigma_hr: 2.0,
            baseline_frozen: true,
            p_active_wake: 0.1,
            p_quiet_wake: 0.25,
            p_sleep: 0.65,
            state: DetectorState::Warmup,
        })
        .collect();
    comp_app.history_sessions = vec![comp_entry.clone()];
    comp_app.selected_session_detail = Some(SessionFullDetail {
        session: SessionLedgerRecord {
            id: comp_entry.id.clone(),
            wearable_id: comp_entry.wearable_id.clone(),
            start_ts_ms: start_ts,
            end_ts_ms: end_ts,
            end_reason: SessionEndReason::HostSuspend,
            detection_sensitivity: Sensitivity::Balanced,
            had_rr: true,
            warmup_mu_hr: Some(64.0),
            warmup_sigma_hr: Some(2.0),
            warmup_ln_rmssd: Some(3.0),
            pre_candidate_mu_hr: None,
            pre_candidate_sigma_hr: None,
            pre_candidate_ln_rmssd: None,
            onset_candidate_ts_ms: None,
            onset_confirmed_ts_ms: None,
            sleeping_ts_ms: None,
            sleep_mu_hr: Some(56.0),
            onset_hr_bpm: None,
            delta_hr_onset: None,
            sleep_ln_rmssd: None,
            sleep_hr_var_ratio: None,
            typical_sqi: Some(0.95),
            post_trigger_sleep_sec: 0,
            trigger_power_action: PowerAction::None,
            rapid_resume_within_5m: false,
            grace_cancel_count: 0,
            rapid_wake_fp_count: 0,
            snooze_count: 0,
            auto_anomalous: false,
            epochs_count: comp_epochs.len() as u32,
            raw_samples_count: 0,
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
        feedback: None,
        epochs: comp_epochs,
        has_raw_samples: false,
    });

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| comp_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let comp_buf = term.backend().buffer();
    let (top_bpm_x, top_row) = find_text_coords(comp_buf, "66bpm").unwrap();
    let (_, bot_row) = find_text_coords(comp_buf, "54bpm").unwrap();
    let (p100_x, _) = find_text_coords(comp_buf, "100%").unwrap();
    let chart_left = top_bpm_x + 7;
    let chart_right = p100_x - 2;

    // Both HR (62 bpm -> norm 0.667) and P(Sleep) (0.70) sit on the same terminal row at different sub-pixel rows:
    // bitwise OR combines both horizontal dot pairs (4 dots = 0x36 '⠶') and gives HR (Color::Green) priority!
    let mut merged_cells = 0usize;
    for y in top_row..=bot_row {
        for x in chart_left..=chart_right {
            let cell = &comp_buf[(x, y)];
            if let Some(ch) = cell.symbol().chars().next()
                && is_non_blank_braille(ch)
            {
                let mask = u32::from(ch) - 0x2800;
                // A single flat horizontal curve sets at most 2 dots in a cell; >= 4 dots proves both sub-rows were ORed
                if mask.count_ones() >= 4 {
                    assert_eq!(
                        cell.fg,
                        Color::Green,
                        "Composited Braille cell must take HR foreground priority"
                    );
                    merged_cells += 1;
                }
            }
        }
    }
    assert!(
        merged_cells > 5,
        "Expected bitwise-OR composited Braille cells across overlapping HR and P(Sleep):\n{}",
        buffer_to_string(comp_buf)
    );

    // 2. --ascii mode on populated Dashboard & History: strictly 7-bit ASCII, continuous strokes, HR glyph priority
    let mut ascii_app = TuiApp::new(false, true, false);
    ascii_app.daemon_connected = true;
    ascii_app.enrolled_wearables = vec![make_wearable(
        "w-1",
        "AA:BB:CC:11:22:33",
        Some("Polar H10"),
        StreamStatus::Streaming,
        Some(90),
        Some("Chest"),
    )];
    ascii_app.sparkline_data.quiet_wake_baseline = Some(64.0);
    ascii_app.sparkline_data.sleeping_hr = Some(56.0);
    for sec in (0..=1800).step_by(30) {
        let bpm = 64.0 - (sec as f64 / 1800.0) * 8.0;
        ascii_app.handle_event(TuiEvent::ipc(IpcEvent::HrSample(HrSampleEvent {
            ts_ms: start_ts + sec * 1_000,
            bpm,
            rr_ms: vec![],
            contact_status: ContactStatus::Contact,
            wearable_id: "w-1".into(),
            session_id: Some("s-1".into()),
        })));
    }
    term.draw(|f| ascii_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let dash_ascii_out = buffer_to_string(term.backend().buffer());
    assert!(
        dash_ascii_out.is_ascii(),
        "Dashboard --ascii must emit strictly 7-bit ASCII:\n{dash_ascii_out}"
    );
    assert!(
        dash_ascii_out.contains('|'),
        "Dashboard --ascii must render '|' -15m guide:\n{dash_ascii_out}"
    );
    assert!(
        dash_ascii_out.contains('*') || dash_ascii_out.contains('\\'),
        "Dashboard --ascii must render continuous ASCII stroke glyphs:\n{dash_ascii_out}"
    );

    // History --ascii with overlapping HR and P(Sleep) -> strictly ASCII, HR ('*' / Green) wins over P(Sleep)
    ascii_app.active_tab = Tab::History;
    ascii_app.history_sessions = comp_app.history_sessions.clone();
    ascii_app.selected_session_detail = comp_app.selected_session_detail.clone();
    term.draw(|f| ascii_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let hist_ascii_buf = term.backend().buffer();
    let hist_ascii_out = buffer_to_string(hist_ascii_buf);
    assert!(
        hist_ascii_out.is_ascii(),
        "History --ascii must emit strictly 7-bit ASCII:\n{hist_ascii_out}"
    );
    let mut hr_ascii_cells = 0usize;
    for y in top_row..=bot_row {
        for x in chart_left..=chart_right {
            let cell = &hist_ascii_buf[(x, y)];
            if cell.symbol() == "*" && cell.fg == Color::Green {
                hr_ascii_cells += 1;
            }
        }
    }
    assert!(
        hr_ascii_cells > 5,
        "History --ascii must give HR glyph ('*') priority over P(Sleep) on overlap:\n{hist_ascii_out}"
    );

    // 3. NO_COLOR mode (no_color = true, ascii = false) preserves Braille with BOLD/DIM/UNDERLINED modifiers
    let mut nc_app = TuiApp::new(false, false, true);
    nc_app.daemon_connected = true;
    nc_app.enrolled_wearables = ascii_app.enrolled_wearables.clone();
    nc_app.sparkline_data = ascii_app.sparkline_data.clone();
    term.draw(|f| nc_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let nc_dash_buf = term.backend().buffer();
    let mut saw_bold_braille = false;
    let mut saw_dim_ref = false;
    let mut saw_underlined_ref = false;
    for y in 0..nc_dash_buf.area.height {
        for x in 0..nc_dash_buf.area.width {
            let cell = &nc_dash_buf[(x, y)];
            assert_eq!(cell.fg, Color::Reset);
            assert_eq!(cell.bg, Color::Reset);
            if cell
                .symbol()
                .chars()
                .next()
                .is_some_and(is_non_blank_braille)
                && cell.modifier.contains(Modifier::BOLD)
            {
                saw_bold_braille = true;
            }
            if cell.symbol() == "┄" && cell.modifier.contains(Modifier::DIM) {
                saw_dim_ref = true;
            }
            if cell.symbol() == "┄" && cell.modifier.contains(Modifier::UNDERLINED) {
                saw_underlined_ref = true;
            }
        }
    }
    assert!(
        saw_bold_braille,
        "NO_COLOR Dashboard must render BOLD Braille HR curve"
    );
    assert!(
        saw_dim_ref,
        "NO_COLOR Dashboard must render DIM QW reference line"
    );
    assert!(
        saw_underlined_ref,
        "NO_COLOR Dashboard must render UNDERLINED Sleep reference line"
    );

    nc_app.active_tab = Tab::History;
    nc_app.history_sessions = comp_app.history_sessions.clone();
    let mut nc_detail = comp_app.selected_session_detail.clone().unwrap();
    for ep in nc_detail.epochs.iter_mut().skip(20) {
        ep.p_sleep = 0.25;
    }
    nc_app.selected_session_detail = Some(nc_detail);
    term.draw(|f| nc_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let nc_hist_buf = term.backend().buffer();
    let mut saw_hist_bold_braille = false;
    let mut saw_hist_underlined_braille = false;
    let mut saw_hist_dim_ref = false;
    for y in 0..nc_hist_buf.area.height {
        for x in 0..nc_hist_buf.area.width {
            let cell = &nc_hist_buf[(x, y)];
            assert_eq!(cell.fg, Color::Reset);
            assert_eq!(cell.bg, Color::Reset);
            if cell
                .symbol()
                .chars()
                .next()
                .is_some_and(is_non_blank_braille)
            {
                if cell.modifier.contains(Modifier::BOLD) {
                    saw_hist_bold_braille = true;
                }
                if cell.modifier.contains(Modifier::UNDERLINED) {
                    saw_hist_underlined_braille = true;
                }
            }
            if cell.symbol() == "┄" && cell.modifier.contains(Modifier::DIM) {
                saw_hist_dim_ref = true;
            }
        }
    }
    assert!(
        saw_hist_bold_braille,
        "NO_COLOR History must preserve BOLD Braille HR curve"
    );
    assert!(
        saw_hist_underlined_braille,
        "NO_COLOR History must preserve UNDERLINED Braille P(Sleep) curve"
    );
    assert!(
        saw_hist_dim_ref,
        "NO_COLOR History must preserve DIM QW reference line"
    );
}

#[test]
fn test_tui_session_started_closed_lifecycle_and_selection_preservation() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let start_ts = 1_700_000_000_000i64;

    // 1. SessionStarted arrives for session "s-1"
    let started_ev = SessionStartedEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        start_ts_ms: start_ts,
    };
    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::SessionStarted(started_ev)));
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "SessionStarted must emit RefreshSessions"
    );

    // 2. Initial SessionsUpdated arrives with "s-1" as [ACTIVE]
    let s1_active = make_history_session(
        "s-1",
        "Polar H10",
        start_ts,
        start_ts,
        SessionStatus::Active,
        0,
    );
    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::SessionsUpdated {
        sessions: vec![s1_active.clone()],
        boundaries: vec![],
    }));
    // Since history was empty, index 0 is selected and SessionGet is requested
    assert_eq!(app.selected_session_idx, 0);
    assert_eq!(
        cmd,
        Some(TuiCommand::SessionGet {
            id: "s-1".to_string()
        })
    );

    // 3. SessionDetailLoaded arrives with 0 epochs (Active)
    let s1_active_detail = make_session_detail(&s1_active, vec![]);
    app.handle_event(TuiEvent::ipc(IpcEvent::SessionDetailLoaded(Box::new(
        s1_active_detail,
    ))));
    assert!(app.selected_session_detail.is_some());

    // Render check: History tab displays "[ACTIVE]" and "Waiting for first 15s epoch..."
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out = buffer_to_string(term.backend().buffer());
    assert!(out.contains("[ACTIVE]"), "Master list must show [ACTIVE]");
    assert!(
        out.contains("Waiting for first 15s epoch..."),
        "Active session with zero epochs must show waiting message"
    );
    assert!(
        !out.contains("Epochs were pruned by retention policy; trace unavailable."),
        "Active session must not show retention pruning message"
    );

    // 4. A second session "s-2" starts in the background
    let started_ev2 = SessionStartedEvent {
        session_id: "s-2".to_string(),
        wearable_id: "w-1".to_string(),
        start_ts_ms: start_ts + 60_000,
    };
    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::SessionStarted(started_ev2)));
    assert_eq!(cmd, Some(TuiCommand::RefreshSessions));

    // SessionsUpdated arrives with [s-2, s-1] (newest first)
    let s2_active = make_history_session(
        "s-2",
        "Polar H10",
        start_ts + 60_000,
        start_ts + 60_000,
        SessionStatus::Active,
        0,
    );
    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::SessionsUpdated {
        sessions: vec![s2_active.clone(), s1_active.clone()],
        boundaries: vec![],
    }));
    // Selection must be preserved by session_id: "s-1" is now at index 1!
    assert_eq!(
        app.selected_session_idx, 1,
        "Must preserve selected_session_idx for s-1 at index 1"
    );
    assert_eq!(
        cmd, None,
        "Detail is already loaded for s-1, no SessionGet needed"
    );
    assert_eq!(
        app.selected_session_detail.as_ref().unwrap().session.id,
        "s-1"
    );

    // 5. SessionClosed arrives for "s-1"
    let mut s1_closed = make_history_session(
        "s-1",
        "Polar H10",
        start_ts,
        start_ts + 300_000,
        SessionStatus::Closed,
        0,
    );
    s1_closed.end_reason = Some(SessionEndReason::StreamEnded);
    let s1_closed_detail = make_session_detail(&s1_closed, vec![]);
    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::SessionClosed(Box::new(
        s1_closed_detail,
    ))));
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "SessionClosed must emit RefreshSessions"
    );
    // selected_session_detail must immediately update to Closed
    assert_eq!(
        app.selected_session_detail.as_ref().unwrap().status,
        SessionStatus::Closed
    );

    // Render check: Closed session with 0 epochs now displays retention pruning message
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out = buffer_to_string(term.backend().buffer());
    assert!(
        out.contains("Epochs were pruned by retention policy"),
        "Closed session with 0 epochs must show retention pruning message"
    );
    assert!(
        !out.contains("Waiting for first 15s epoch..."),
        "Closed session must not show waiting message"
    );

    // Verify full string rendering in a wider terminal
    let mut wide_term = Terminal::new(TestBackend::new(120, 24)).unwrap();
    wide_term
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let wide_out = buffer_to_string(wide_term.backend().buffer());
    assert!(wide_out.contains("Epochs were pruned by retention policy; trace unavailable."));

    // 6. Background refresh where selected session disappears (e.g. pruned by retention policy):
    // Even when multiple sessions exist in the updated list, disappearing selection falls back to index 0
    let s3_closed = make_history_session(
        "s-3",
        "Polar H10",
        start_ts + 30_000,
        start_ts + 45_000,
        SessionStatus::Closed,
        1,
    );
    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::SessionsUpdated {
        sessions: vec![s2_active.clone(), s3_closed],
        boundaries: vec![],
    }));
    // Since "s-1" is gone, selection falls back to index 0 ("s-2") and requests detail
    assert_eq!(app.selected_session_idx, 0);
    assert_eq!(
        cmd,
        Some(TuiCommand::SessionGet {
            id: "s-2".to_string()
        })
    );
    assert!(app.selected_session_detail.is_none());
}

#[test]
fn test_tui_epoch_update_unknown_session_triggers_fallback_refresh() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let start_ts = 1_700_000_000_000i64;
    let epoch = make_test_epoch(0, start_ts + 15_000, 68.0, 0.1, 70.0);
    let epoch_ev = EpochUpdateEvent {
        session_id: "s-unknown".to_string(),
        wearable_id: "w-1".to_string(),
        epoch,
    };

    let cmd = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(epoch_ev)));
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "EpochUpdate for unknown session must trigger fallback RefreshSessions"
    );
}

#[test]
fn test_tui_epoch_update_advances_known_session_row_without_ipc_commands_and_deduplicates() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let start_ts = 1_700_000_000_000i64;
    let mut s1 = make_history_session(
        "s-1",
        "Polar H10",
        start_ts,
        start_ts,
        SessionStatus::Active,
        0,
    );
    s1.warmup_mu_hr = None;
    app.history_sessions = vec![s1];

    // 1. First 15s epoch arrives
    let epoch1 = make_test_epoch(0, start_ts + 15_000, 65.0, 0.05, 63.5);
    let cmd1 = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        epoch: epoch1.clone(),
    })));
    assert_eq!(
        cmd1, None,
        "Known session epoch update must not emit any IPC command"
    );
    assert_eq!(
        app.history_sessions[0].end_ts_ms,
        start_ts + 15_000,
        "end_ts_ms must advance to epoch timestamp"
    );
    assert_eq!(
        app.history_sessions[0].epochs_count, 1,
        "epochs_count must increment on new epoch"
    );
    assert_eq!(
        app.history_sessions[0].warmup_mu_hr,
        Some(63.5),
        "warmup_mu_hr must reflect finite positive baseline"
    );

    // 2. Duplicate epoch with identical timestamp arrives
    let cmd_dup = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        epoch: epoch1,
    })));
    assert_eq!(cmd_dup, None);
    assert_eq!(
        app.history_sessions[0].end_ts_ms,
        start_ts + 15_000,
        "end_ts_ms must remain unchanged on duplicate"
    );
    assert_eq!(
        app.history_sessions[0].epochs_count, 1,
        "epochs_count must NOT double-increment on duplicate timestamp"
    );

    // 3. Second 15s epoch arrives
    let epoch2 = make_test_epoch(1, start_ts + 30_000, 64.0, 0.10, 63.5);
    let cmd2 = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        epoch: epoch2,
    })));
    assert_eq!(cmd2, None);
    assert_eq!(
        app.history_sessions[0].end_ts_ms,
        start_ts + 30_000,
        "end_ts_ms must advance to second epoch timestamp"
    );
    assert_eq!(
        app.history_sessions[0].epochs_count, 2,
        "epochs_count must increment to 2 on second distinct epoch"
    );
}

#[test]
fn test_tui_epoch_update_appends_to_open_session_detail_and_renders_live_chart() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;

    let start_ts = 1_700_000_000_000i64;
    let mut s1 = make_history_session(
        "s-1",
        "Polar H10",
        start_ts,
        start_ts,
        SessionStatus::Active,
        0,
    );
    s1.warmup_mu_hr = None;
    let s1_detail = make_session_detail(&s1, vec![]);
    app.history_sessions = vec![s1];
    app.selected_session_idx = 0;
    app.selected_session_detail = Some(s1_detail);

    // Initial render check: active session with zero epochs shows waiting message
    let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out0 = buffer_to_string(term.backend().buffer());
    assert!(
        out0.contains("Waiting for first 15s epoch..."),
        "Must show waiting message when epochs are empty"
    );

    // 1. First 15s epoch arrives
    let epoch1 = make_test_epoch(0, start_ts + 15_000, 65.0, 0.15, 64.0);
    let cmd1 = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        epoch: epoch1.clone(),
    })));
    assert_eq!(cmd1, None);

    let detail1 = app.selected_session_detail.as_ref().unwrap();
    assert_eq!(detail1.epochs.len(), 1, "Must append epoch to detail trace");
    assert_eq!(detail1.epochs[0].ts_ms, start_ts + 15_000);
    assert_eq!(detail1.session.epochs_count, 1);
    assert_eq!(detail1.session.end_ts_ms, start_ts + 15_000);
    assert_eq!(detail1.session.warmup_mu_hr, Some(64.0));

    // Render check: chart is now active, waiting message gone, P(Sleep) legend present
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out1 = buffer_to_string(term.backend().buffer());
    assert!(
        !out1.contains("Waiting for first 15s epoch..."),
        "Waiting message must disappear once epochs exist"
    );
    assert!(
        out1.contains("P(Sleep)"),
        "Chart legend must render P(Sleep)"
    );
    assert!(
        out1.contains("QW: 64.0 bpm"),
        "Live QW baseline display hint must render in header"
    );

    // 2. Duplicate epoch1 arrives (simulating race or retransmission)
    let cmd_dup = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        epoch: epoch1,
    })));
    assert_eq!(cmd_dup, None);
    let detail_dup = app.selected_session_detail.as_ref().unwrap();
    assert_eq!(
        detail_dup.epochs.len(),
        1,
        "Duplicate epoch must not append duplicate point to detail trace"
    );
    assert_eq!(detail_dup.session.epochs_count, 1);

    // 3. Second epoch arrives
    let epoch2 = make_test_epoch(1, start_ts + 30_000, 62.0, 0.40, 64.0);
    let cmd2 = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-1".to_string(),
        wearable_id: "w-1".to_string(),
        epoch: epoch2,
    })));
    assert_eq!(cmd2, None);
    let detail2 = app.selected_session_detail.as_ref().unwrap();
    assert_eq!(detail2.epochs.len(), 2, "Second epoch must append to trace");
    assert_eq!(detail2.session.epochs_count, 2);
    assert_eq!(detail2.session.end_ts_ms, start_ts + 30_000);

    // 4. Session for a DIFFERENT session ID arrives; must NOT affect open detail
    let epoch_other = make_test_epoch(0, start_ts + 15_000, 75.0, 0.05, 72.0);
    let s_other = make_history_session(
        "s-other",
        "Chest Strap",
        start_ts,
        start_ts,
        SessionStatus::Active,
        0,
    );
    app.history_sessions.push(s_other);
    let cmd_other = app.handle_event(TuiEvent::ipc(IpcEvent::EpochUpdate(EpochUpdateEvent {
        session_id: "s-other".to_string(),
        wearable_id: "w-2".to_string(),
        epoch: epoch_other,
    })));
    assert_eq!(cmd_other, None);
    let detail_after_other = app.selected_session_detail.as_ref().unwrap();
    assert_eq!(detail_after_other.session.id, "s-1");
    assert_eq!(
        detail_after_other.epochs.len(),
        2,
        "Detail trace for s-1 must remain untouched by s-other update"
    );
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_live_session_started_and_closed_e2e() {
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
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
        a.daemon_connected && !a.enrolled_wearables.is_empty()
    })
    .await;

    // Switch to History tab (initially empty)
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert!(app.history_sessions.is_empty());

    // 1. Send wearable stream packet -> starts a monitoring session in SQLite and emits SessionStarted
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

    // TUI must receive SessionStarted -> RefreshSessions -> SessionsUpdated -> SessionGet -> SessionDetailLoaded
    wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
        a.history_sessions.len() == 1
            && a.history_sessions[0].status == SessionStatus::Active
            && a.selected_session_detail.is_some()
    })
    .await;

    // Render check on live TUI: active session appears as [ACTIVE] and waiting for first epoch
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out = buffer_to_string(term.backend().buffer());
    assert!(
        out.contains("[ACTIVE]"),
        "Master list must show newly started session as [ACTIVE]"
    );
    assert!(
        out.contains("Waiting for first 15s epoch..."),
        "Active session with zero epochs must show waiting message"
    );

    // 2. Disconnect/stream end -> triggers SessionEnded -> close_session_and_recompute -> SessionClosed
    drop(w_tx);

    // TUI must receive SessionClosed -> RefreshSessions -> SessionsUpdated
    wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
        a.history_sessions
            .first()
            .is_some_and(|s| s.status == SessionStatus::Closed)
            && a.selected_session_detail
                .as_ref()
                .is_some_and(|d| d.status == SessionStatus::Closed)
    })
    .await;

    // Render check on finalized closed session: shows [Ended] and retention pruning message
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out = buffer_to_string(term.backend().buffer());
    assert!(
        out.contains("[Ended]"),
        "Master list must show closed session as [Ended]"
    );
    assert!(
        out.contains("Epochs were pruned by retention policy"),
        "Closed session with zero epochs must show retention pruning message"
    );

    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_live_15s_epoch_updates_advance_session_and_chart_e2e() {
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
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let bridge = TuiIpcBridge::new(paths.socket_path.clone(), event_tx, cmd_rx, shutdown_rx);
    let bridge_task = tokio::spawn(bridge.run());

    let mut app = TuiApp::new(false, false, false);
    wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
        a.daemon_connected && !a.enrolled_wearables.is_empty()
    })
    .await;

    // Switch to History tab (initially empty)
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert!(app.history_sessions.is_empty());

    // 1. Send wearable stream packet -> starts a monitoring session in SQLite and emits SessionStarted
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

    // TUI receives SessionStarted -> RefreshSessions -> SessionsUpdated -> SessionGet -> SessionDetailLoaded
    wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
        a.history_sessions.len() == 1
            && a.history_sessions[0].status == SessionStatus::Active
            && a.selected_session_detail.is_some()
    })
    .await;

    // Before first epoch: shows "Waiting for first 15s epoch..."
    let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let out = buffer_to_string(term.backend().buffer());
    assert!(out.contains("Waiting for first 15s epoch..."));

    // 2. Stream 15 seconds of HR packets -> triggers first 15s EpochUpdate
    for sec in 2..=16 {
        w_tx.send(WearableEvent::HrPacket {
            wearable: desc.clone(),
            packet: HrPacket {
                ts_ms: t0 + sec * 1_000,
                bpm: 64.0,
                rr_ms: vec![],
                contact_status: ContactStatus::Contact,
            },
        })
        .await
        .unwrap();
    }

    // TUI must receive EpochUpdate over real Unix socket -> in-memory advance without polling SQLite
    wait_for_app_state_with_cmds(&mut app, &mut event_rx, Some(&cmd_tx), |a| {
        a.history_sessions
            .first()
            .is_some_and(|s| s.epochs_count >= 1)
            && a.selected_session_detail
                .as_ref()
                .is_some_and(|d| !d.epochs.is_empty())
    })
    .await;

    // Verify in-memory state: 1 epoch appended, end_ts_ms advanced, duration updated
    let active_entry = &app.history_sessions[0];
    assert_eq!(active_entry.epochs_count, 1);
    assert!(active_entry.end_ts_ms >= t0 + 15_000);
    let active_detail = app.selected_session_detail.as_ref().unwrap();
    assert_eq!(active_detail.epochs.len(), 1);
    assert_eq!(active_detail.session.epochs_count, 1);

    // Render check on live TUI: waiting message gone, real-time heart-rate and P(Sleep) chart rendered
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let live_out = buffer_to_string(term.backend().buffer());
    assert!(
        !live_out.contains("Waiting for first 15s epoch..."),
        "Waiting message must disappear once first epoch arrives"
    );
    assert!(
        live_out.contains("P(Sleep)"),
        "Real-time chart must render P(Sleep) legend"
    );

    drop(w_tx);
    let _ = shutdown_tx.send(true);
    let _ = bridge_task.await;
    daemon.shutdown().await;
}

#[test]
fn test_tui_tab_switch_history_refresh_sessions() {
    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();

    // 1. Direct tab key '2' from Dashboard emits RefreshSessions
    assert_eq!(app.active_tab, Tab::Dashboard);
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Switching from Dashboard to History via '2' must emit RefreshSessions"
    );

    // 2. Pressing '2' while already on History must NOT emit RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd, None,
        "Pressing '2' while already on History must not emit redundant RefreshSessions"
    );

    // 3. Switching to other tabs must NOT emit RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    assert_eq!(app.active_tab, Tab::Dashboard);
    assert_eq!(cmd, None);

    // 4. Tab (next) navigation: Dashboard -> History emits RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Navigating to History via Tab must emit RefreshSessions"
    );

    // 5. Tab (next) navigation: History -> Wearables must NOT emit RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert_eq!(app.active_tab, Tab::Wearables);
    assert_eq!(cmd, None);

    // 6. BackTab (prev) navigation: Wearables -> History emits RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::BackTab)));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Navigating to History via BackTab must emit RefreshSessions"
    );

    // 7. 'h' (prev) navigation: History -> Dashboard must NOT emit RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('h'))));
    assert_eq!(app.active_tab, Tab::Dashboard);
    assert_eq!(cmd, None);

    // 8. 'l' (next) navigation: Dashboard -> History emits RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Navigating to History via 'l' must emit RefreshSessions"
    );

    // 9. 'h' (prev) back to Dashboard
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('h'))));
    assert_eq!(app.active_tab, Tab::Dashboard);
    assert_eq!(cmd, None);

    // 10. Right arrow navigation: Dashboard -> History emits RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Navigating to History via Right arrow must emit RefreshSessions"
    );

    // 11. Right arrow navigation: History -> Wearables must NOT emit RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert_eq!(app.active_tab, Tab::Wearables);
    assert_eq!(cmd, None);

    // 12. Left arrow navigation: Wearables -> History emits RefreshSessions
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Left)));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Navigating to History via Left arrow must emit RefreshSessions"
    );

    // 13. Mouse tab click: switch to Settings, re-render, then click [2:History]
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Char('4'))));
    assert_eq!(app.active_tab, Tab::Settings);
    assert_eq!(cmd, None);

    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let (hist_col, hist_row) = find_text_coords(term.backend().buffer(), "[2:History]")
        .expect("History tab label in header");

    let cmd = app.handle_event(mouse_left_click(hist_col + 1, hist_row));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Clicking History tab from Settings must emit RefreshSessions"
    );

    // 14. Mouse click on [2:History] while already on History must NOT emit RefreshSessions
    let cmd = app.handle_event(mouse_left_click(hist_col + 1, hist_row));
    assert_eq!(app.active_tab, Tab::History);
    assert_eq!(
        cmd, None,
        "Clicking History tab while already on History must not emit redundant RefreshSessions"
    );

    // 15. Offline behavior: when disconnected from daemon, tab switching must NOT emit RefreshSessions
    let mut offline_app = TuiApp::new(false, false, false);
    assert!(!offline_app.daemon_connected);
    assert_eq!(offline_app.active_tab, Tab::Dashboard);

    let cmd = offline_app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(offline_app.active_tab, Tab::History);
    assert_eq!(
        cmd, None,
        "Switching to History while offline must not emit RefreshSessions"
    );

    offline_app.active_tab = Tab::Dashboard;
    let cmd = offline_app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    assert_eq!(offline_app.active_tab, Tab::History);
    assert_eq!(
        cmd, None,
        "Tab navigation to History while offline must not emit RefreshSessions"
    );

    term.draw(|f| offline_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let (hist_col, hist_row) = find_text_coords(term.backend().buffer(), "[2:History]")
        .expect("History tab label in header");
    offline_app.active_tab = Tab::Dashboard;
    let cmd = offline_app.handle_event(mouse_left_click(hist_col + 1, hist_row));
    assert_eq!(offline_app.active_tab, Tab::History);
    assert_eq!(
        cmd, None,
        "Mouse clicking History while offline must not emit RefreshSessions"
    );
}

#[test]
fn test_tui_history_footer_uncluttered_no_manual_refresh() {
    let mut app = TuiApp::new(false, false, false);
    app.active_tab = Tab::History;

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf = term.backend().buffer();
    let out = buffer_to_string(buf);

    // Standard footer actions must be present
    assert!(
        out.contains("Feedback"),
        "Footer must contain Feedback action"
    );
    assert!(out.contains("Export"), "Footer must contain Export action");
    assert!(out.contains("Delete"), "Footer must contain Delete action");
    assert!(
        out.contains("Navigate"),
        "Footer must contain Navigate hint"
    );

    // History footer must NOT add a manual refresh keybinding or footer item
    assert!(
        !out.contains("[r]"),
        "History footer must not have an [r] keybinding"
    );
    assert!(
        !out.contains("Refresh"),
        "History footer must not contain a Refresh item"
    );
}

#[tokio::test(start_paused = true)]
async fn test_real_socket_history_tab_switch_reconciles_sessions_e2e() {
    let mut h = RealSocketTuiHarness::spawn(1, false).await;
    let s1_id = h.session_ids[0].clone();
    assert_eq!(h.app.history_sessions.len(), 1);
    assert_eq!(h.app.history_sessions[0].id, s1_id);

    // Switch to Dashboard
    h.app.active_tab = Tab::Dashboard;

    // Directly insert a second session into SQLite
    let base_ts = Utc::now().timestamp_millis() - 100_000;
    let s2_id = h.insert_closed_session(2, base_ts, base_ts + 50_000);

    // Before tab switch, TuiApp still has 1 session cached
    assert_eq!(h.app.history_sessions.len(), 1);

    // Switch from Dashboard to History via '2': must emit RefreshSessions
    let cmd = h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(
        cmd,
        Some(TuiCommand::RefreshSessions),
        "Switching to History tab must emit RefreshSessions command"
    );

    // Send the emitted command through the bridge
    h.cmd_tx.send(cmd.unwrap()).await.unwrap();

    // Wait for the reconciled session list to arrive in TuiApp
    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_sessions.len() == 2
    })
    .await;

    // Both sessions present (s2_id is newest so index 0, s1_id is index 1)
    assert_eq!(h.app.history_sessions[0].id, s2_id);
    assert_eq!(h.app.history_sessions[1].id, s1_id);

    // Select the older session (s1_id at index 1) via 'j'
    let sel_cmd = h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    if let Some(c) = sel_cmd {
        h.cmd_tx.send(c).await.unwrap();
        wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
            a.selected_session_detail.as_ref().map(|d| &d.session.id) == Some(&s1_id)
        })
        .await;
    }
    assert_eq!(h.app.selected_session_idx, 1);
    let selected_id = h.app.history_sessions[h.app.selected_session_idx]
        .id
        .clone();
    assert_eq!(selected_id, s1_id);

    // Switch to Dashboard, then back to History: must emit RefreshSessions and preserve selected session_id
    let cmd_dash = h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    assert_eq!(cmd_dash, None);
    assert_eq!(h.app.active_tab, Tab::Dashboard);

    let cmd_hist = h.app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    assert_eq!(cmd_hist, Some(TuiCommand::RefreshSessions));
    h.cmd_tx.send(cmd_hist.unwrap()).await.unwrap();

    wait_for_app_state_with_cmds(&mut h.app, &mut h.event_rx, Some(&h.cmd_tx), |a| {
        a.history_sessions.len() == 2
    })
    .await;

    // Preserved selection by session_id
    assert_eq!(h.app.selected_session_idx, 1);
    assert_eq!(h.app.history_sessions[h.app.selected_session_idx].id, s1_id);

    h.shutdown().await;
}

#[test]
fn test_feedback_modal_canonicalizes_short_hour_rejects_short_minute_and_accepts_135_bpm() {
    let start_dt = chrono::Local
        .with_ymd_and_hms(2026, 11, 19, 8, 0, 0)
        .single()
        .unwrap();
    let start_ts = start_dt.timestamp_millis();
    let end_ts = start_ts + 4 * 3_600_000;

    let session = make_history_session(
        "s-morning",
        "Polar H10",
        start_ts,
        end_ts,
        SessionStatus::Closed,
        10,
    );
    let detail = make_session_detail(&session, Vec::new());

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    app.handle_ipc(IpcEvent::SessionsUpdated {
        sessions: vec![session],
        boundaries: Vec::new(),
    });
    app.handle_ipc(IpcEvent::SessionDetailLoaded(Box::new(detail)));

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('f'))));
    assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    for c in "9:5".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    assert!(
        matches!(
            app.modal,
            ModalState::FeedbackModal {
                error_message: Some(_),
                ..
            }
        ),
        "single-digit minute like 9:5 must be rejected"
    );

    for _ in 0..3 {
        app.handle_event(TuiEvent::Key(key(KeyCode::Backspace)));
    }
    for c in "9:30".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    for c in "135.0".chars() {
        app.handle_event(TuiEvent::Key(key(KeyCode::Char(c))));
    }
    app.handle_event(TuiEvent::Key(key(KeyCode::Tab)));
    let cmd = app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    assert_eq!(
        cmd,
        Some(TuiCommand::SessionSetFeedback {
            session_id: "s-morning".to_string(),
            verdict: None,
            onset_hhmm: Some(ClockTime::new(9, 30).unwrap()),
            avg_sleep_hr: Some(135.0),
            clear: false,
        })
    );
}
