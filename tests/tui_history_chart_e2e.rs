mod common;
use common::tui::*;

use chrono::TimeZone;
use crossterm::event::KeyCode;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;

use bpmute::calibration::{SessionFeedbackRecord, SessionLedgerRecord};
use bpmute::config::{PowerAction, Sensitivity};
use bpmute::ipc::protocol::HrSampleEvent;
use bpmute::sleep_engine::{
    ContactStatus, DetectorState, Epoch, EpochFeatures, SessionEndReason, StreamStatus,
};
use bpmute::storage::{SessionFullDetail, SessionStatus, WearableInfo};
use bpmute::tui::{IpcEvent, ModalState, Tab, TuiApp, TuiEvent};

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
    let (o_col, o_label_row) = find_text_coords_in_row(buf, top_row - 1, "[O]").unwrap();
    let (t_col, _) = find_text_coords_in_row(buf, top_row - 1, "[T]").unwrap();
    let (m_col, _) = find_text_coords_in_row(buf, top_row - 1, "[M]").unwrap();
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
fn test_history_chart_wall_clock_x_axis_and_gridlines() {
    use chrono::Timelike;
    use ratatui::style::Modifier;

    let build_app = |ascii: bool, no_color: bool, start_ts: i64, duration_min: i64| {
        let mut app = TuiApp::new(false, ascii, no_color);
        app.daemon_connected = true;
        app.active_tab = Tab::History;
        let end_ts = start_ts + duration_min * 60_000;
        let mut entry = make_history_session(
            "sess-xaxis",
            "Polar H10",
            start_ts,
            end_ts,
            SessionStatus::Closed,
            30,
        );
        entry.warmup_mu_hr = Some(72.0);
        entry.sleep_mu_hr = Some(58.0);
        // Flat HR at 70 bpm and P(Sleep) at 0.2 so most middle plot rows remain free for gridline inspection
        let step_sec = if duration_min <= 60 { 60 } else { 300 };
        let epochs: Vec<Epoch> = (0..=duration_min * 60)
            .step_by(step_sec)
            .enumerate()
            .map(|(i, sec)| make_test_epoch(i as u64, start_ts + sec * 1_000, 70.0, 0.2, 72.0))
            .collect();
        entry.epochs_count = epochs.len() as u32;
        let detail = make_session_detail(&entry, epochs);
        app.history_sessions = vec![entry];
        app.selected_session_idx = 0;
        app.selected_session_detail = Some(detail);
        app
    };

    // 1. Short 45-minute session (23:15 -> 00:00 local): finer round sub-hour ticks & Unicode '┆' gridlines
    let short_start_dt = chrono::Local
        .with_ymd_and_hms(2026, 1, 10, 23, 15, 0)
        .single()
        .unwrap();
    let short_start_ts = short_start_dt.timestamp_millis();
    let mut short_app = build_app(false, false, short_start_ts, 45);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| short_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buf = term.backend().buffer();
    let out = buffer_to_string(buf);

    let (top_bpm_x, top_row) = find_text_coords(buf, "74bpm").expect("top HR label");
    let (_, bot_row) = find_text_coords(buf, "56bpm").expect("bottom HR label");
    let (p100_x, _) = find_text_coords(buf, "100%").expect("right 100% label");
    let chart_left = top_bpm_x + 7;
    let chart_right = p100_x - 2;
    let x_axis_row = bot_row + 1;

    // Extract row text of X-axis within plot bounds and verify round sub-hour HH:MM labels with non-zero spacing
    let axis_str: String = (chart_left..=chart_right)
        .map(|x| buf[(x, x_axis_row)].symbol())
        .collect();
    assert!(
        axis_str.contains("23:30") && axis_str.contains("00:00"),
        "45m chart X-axis row must show round sub-hour local HH:MM labels (e.g. 23:30, 00:00), got '{axis_str}':\n{out}"
    );
    // Verify subtle vertical '┆' gridline exists underneath curves on at least one tick column
    let grid_cols: Vec<u16> = (chart_left..=chart_right)
        .filter(|&x| (top_row..=bot_row).any(|y| buf[(x, y)].symbol() == "┆"))
        .collect();
    assert!(
        !grid_cols.is_empty(),
        "Expected subtle '┆' vertical time gridlines on plot area:\n{out}"
    );

    // 2. Multi-hour 8-hour overnight session (23:15 -> 07:15 local): scales to round hour ticks without overlap
    let long_start_ts = short_start_ts;
    let mut long_app = build_app(false, false, long_start_ts, 8 * 60);
    term.draw(|f| long_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let long_buf = term.backend().buffer();
    let long_out = buffer_to_string(long_buf);
    let (_, long_bot_row) = find_text_coords(long_buf, "56bpm").expect("long bottom HR label");
    let long_axis_row = long_bot_row + 1;
    let long_axis_str: String = (chart_left..=chart_right)
        .map(|x| long_buf[(x, long_axis_row)].symbol())
        .collect();

    // Parse all HH:MM tokens on the X-axis row and verify they are on :00 round hours and separated by >= 1 space
    let mut parsed_tokens: Vec<(usize, String)> = Vec::new();
    let chars: Vec<char> = long_axis_str.chars().collect();
    let mut i = 0;
    while i + 5 <= chars.len() {
        let slice: String = chars[i..i + 5].iter().collect();
        if slice.as_bytes()[2] == b':'
            && slice[0..2].chars().all(|c| c.is_ascii_digit())
            && slice[3..5].chars().all(|c| c.is_ascii_digit())
        {
            assert_eq!(
                &slice[3..5],
                "00",
                "8h session X-axis tick '{slice}' must align to round local hours:\n{long_out}"
            );
            if let Some((prev_start, _)) = parsed_tokens.last() {
                assert!(
                    i >= prev_start + 6,
                    "X-axis labels must not touch or overlap in '{long_axis_str}'"
                );
            }
            parsed_tokens.push((i, slice));
            i += 5;
        } else {
            i += 1;
        }
    }
    assert!(
        parsed_tokens.len() >= 2,
        "Expected >= 2 round-hour X-axis labels on 8h session, got '{long_axis_str}':\n{long_out}"
    );
    // Verify 00:00 local hour appears on an overnight 23:15..07:15 session
    assert!(
        parsed_tokens.iter().any(|(_, lbl)| lbl == "00:00"),
        "Overnight 23:15..07:15 chart must include 00:00 tick, got '{long_axis_str}'"
    );
    let _ = short_start_dt.hour();

    // 3. --ascii mode uses strictly 7-bit ASCII and '|' vertical time gridlines
    let mut ascii_app = build_app(true, false, short_start_ts, 45);
    term.draw(|f| ascii_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let ascii_buf = term.backend().buffer();
    let ascii_out = buffer_to_string(ascii_buf);
    assert!(
        ascii_out.is_ascii(),
        "History chart in --ascii mode must be strictly 7-bit ASCII:\n{ascii_out}"
    );
    let (_, ascii_bot_row) = find_text_coords(ascii_buf, "56bpm").unwrap();
    let ascii_grid_cols: Vec<u16> = (chart_left..=chart_right)
        .filter(|&x| (top_row..=ascii_bot_row).any(|y| ascii_buf[(x, y)].symbol() == "|"))
        .collect();
    assert!(
        !ascii_grid_cols.is_empty(),
        "Expected '|' vertical time gridlines in --ascii mode:\n{ascii_out}"
    );

    // 4. NO_COLOR mode preserves Reset colors and DIM modifier on gridlines and X-axis labels
    let mut nc_app = build_app(false, true, short_start_ts, 45);
    term.draw(|f| nc_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let nc_buf = term.backend().buffer();
    let mut saw_dim_grid = false;
    let mut saw_dim_axis_digit = false;
    let (_, nc_bot_row) = find_text_coords(nc_buf, "56bpm").unwrap();
    for y in 0..nc_buf.area.height {
        for x in 0..nc_buf.area.width {
            let cell = &nc_buf[(x, y)];
            assert_eq!(cell.fg, Color::Reset);
            assert_eq!(cell.bg, Color::Reset);
            if cell.symbol() == "┆" && cell.modifier.contains(Modifier::DIM) {
                saw_dim_grid = true;
            }
            if y == nc_bot_row + 1
                && x >= chart_left
                && x <= chart_right
                && cell.symbol().chars().all(|c| c.is_ascii_digit())
                && cell.modifier.contains(Modifier::DIM)
            {
                saw_dim_axis_digit = true;
            }
        }
    }
    assert!(saw_dim_grid, "NO_COLOR gridlines must use Modifier::DIM");
    assert!(
        saw_dim_axis_digit,
        "NO_COLOR X-axis labels must use Modifier::DIM"
    );

    // 5. Compact 60x18 viewport degrades gracefully with >= 2 plot rows and no overflow
    let mut compact_term = Terminal::new(TestBackend::new(60, 18)).unwrap();
    compact_term
        .draw(|f| short_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let compact_buf = compact_term.backend().buffer();
    let (_, c_top_row) = find_text_coords(compact_buf, "74bpm").unwrap();
    let (_, c_bot_row) = find_text_coords(compact_buf, "56bpm").unwrap();
    assert!(
        c_bot_row > c_top_row,
        "Compact 60x18 chart must retain >= 2 plot rows"
    );
}

#[test]
fn test_history_detail_header_events_timestamps_and_omission() {
    let start_dt = chrono::Local
        .with_ymd_and_hms(2026, 1, 10, 23, 0, 0)
        .single()
        .unwrap();
    let start_ts = start_dt.timestamp_millis();
    let end_ts = start_ts + 8 * 3_600_000; // 8-hour session

    // Closely spaced [O] and [T] (3 minutes apart) + [M] Manual onset
    let onset_ts = start_ts + 45 * 60_000; // 23:45
    let trigger_ts = start_ts + 48 * 60_000; // 23:48
    let manual_ts = start_ts + 46 * 60_000; // 23:46

    let mut s_with_events = make_history_session(
        "sess-ev",
        "Polar H10",
        start_ts,
        end_ts,
        SessionStatus::Closed,
        10,
    );
    s_with_events.onset_confirmed_ts_ms = Some(onset_ts);
    s_with_events.sleeping_ts_ms = Some(trigger_ts);
    s_with_events.feedback = Some(SessionFeedbackRecord {
        id: "fb-ev".into(),
        session_id: "sess-ev".into(),
        verdict: None,
        manual_onset_ts_ms: Some(manual_ts),
        manual_avg_sleep_hr: None,
        notes: None,
        updated_at_ms: end_ts + 1_000,
    });

    let epochs: Vec<Epoch> = (0..16)
        .map(|i| make_test_epoch(i, start_ts + (i as i64) * 30 * 60_000, 64.0, 0.3, 68.0))
        .collect();
    let detail_with_events = make_session_detail(&s_with_events, epochs.clone());

    let mut s_no_events = make_history_session(
        "sess-no-ev",
        "Polar H10",
        start_ts - 86_400_000,
        end_ts - 86_400_000,
        SessionStatus::Closed,
        10,
    );
    s_no_events.onset_confirmed_ts_ms = None;
    s_no_events.sleeping_ts_ms = None;
    let detail_no_events = make_session_detail(&s_no_events, epochs);

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;
    app.history_sessions = vec![s_with_events, s_no_events];
    app.selected_session_idx = 0;
    app.selected_session_detail = Some(detail_with_events);

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf = term.backend().buffer();
    let out = buffer_to_string(buf);

    let expected_o = chrono::Local
        .timestamp_millis_opt(onset_ts)
        .single()
        .unwrap()
        .format("%H:%M")
        .to_string();
    let expected_t = chrono::Local
        .timestamp_millis_opt(trigger_ts)
        .single()
        .unwrap()
        .format("%H:%M")
        .to_string();
    let expected_m = chrono::Local
        .timestamp_millis_opt(manual_ts)
        .single()
        .unwrap()
        .format("%H:%M")
        .to_string();

    let (_, ev_row) = find_text_coords(buf, "Events:").expect("Events: header line must appear");
    let ev_row_text: String = (35..79).map(|x| buf[(x, ev_row)].symbol()).collect();
    assert!(
        ev_row_text.contains(&format!("[O] {expected_o}"))
            && ev_row_text.contains(&format!("[T] {expected_t}"))
            && ev_row_text.contains(&format!("[M] {expected_m}")),
        "Events: header row on 80x24 must fit [O], [T], and [M] HH:MM timestamps inside 44-col inner pane, got '{ev_row_text}':\n{out}"
    );
    // Preserve [T] Trigger legend label on wider terminals (area.width >= 66)
    let mut wide_term = Terminal::new(TestBackend::new(110, 24)).unwrap();
    wide_term
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let wide_out = buffer_to_string(wide_term.backend().buffer());
    assert!(
        wide_out.contains("[T] Trigger"),
        "Existing [T] Trigger legend label must be preserved on wide terminals:\n{wide_out}"
    );

    // Switch to session with no onset/trigger/manual timestamps -> Events: line omitted
    app.selected_session_idx = 1;
    app.selected_session_detail = Some(detail_no_events);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let no_ev_out = buffer_to_string(term.backend().buffer());
    assert!(
        !no_ev_out.contains("Events:"),
        "Events: header line must be omitted when no event timestamps exist:\n{no_ev_out}"
    );
}

#[test]
fn test_history_chart_keyboard_inspect_mode_priority_scrub_and_reset() {
    let start_dt = chrono::Local
        .with_ymd_and_hms(2026, 1, 11, 23, 0, 0)
        .single()
        .unwrap();
    let start_ts = start_dt.timestamp_millis();
    let end_ts = start_ts + 40 * 60_000; // 40m
    let onset_ts = start_ts + 10 * 60_000; // 25%
    let trigger_ts = start_ts + 30 * 60_000; // 75%

    let build_40m_session = |id: &str, onset: Option<i64>, trigger: Option<i64>| {
        let mut entry = make_history_session(
            id,
            "Polar H10",
            start_ts,
            end_ts,
            SessionStatus::Closed,
            161,
        );
        entry.warmup_mu_hr = Some(70.0);
        entry.sleep_mu_hr = Some(56.0);
        entry.onset_confirmed_ts_ms = onset;
        entry.sleeping_ts_ms = trigger;
        let epochs: Vec<Epoch> = (0..=40 * 60)
            .step_by(15)
            .enumerate()
            .map(|(i, sec)| {
                let frac = sec as f64 / (40.0 * 60.0);
                let hr = 70.0 - frac * 14.0; // 70 -> 56
                let p = frac * 0.9; // 0.0 -> 0.9
                make_test_epoch(i as u64, start_ts + sec * 1_000, hr, p, 70.0)
            })
            .collect();
        let detail = make_session_detail(&entry, epochs);
        (entry, detail)
    };

    let (s_both, d_both) = build_40m_session("sess-both", Some(onset_ts), Some(trigger_ts));
    let (s_onset_only, d_onset_only) = build_40m_session("sess-onset", Some(onset_ts), None);
    let (s_neither, d_neither) = build_40m_session("sess-neither", None, None);
    let mut s_empty = make_history_session(
        "sess-empty",
        "Polar H10",
        start_ts,
        end_ts,
        SessionStatus::Closed,
        0,
    );
    s_empty.warmup_mu_hr = Some(70.0);
    let d_empty = make_session_detail(&s_empty, Vec::new());

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;
    app.history_sessions = vec![s_both, s_onset_only, s_neither, s_empty];
    app.selected_session_idx = 0;
    app.selected_session_detail = Some(d_both);

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();

    let buf0 = term.backend().buffer().clone();
    let (chart_left, chart_right, top_row, bot_row) =
        history_chart_plot_bounds(&buf0, "72bpm", "54bpm");
    let chart_w = chart_right - chart_left + 1;
    let (t_tag_x, _) = find_text_coords_in_row(&buf0, top_row - 1, "[T]").unwrap();
    let t_col_abs = t_tag_x + 1;
    let (o_tag_x, _) = find_text_coords_in_row(&buf0, top_row - 1, "[O]").unwrap();
    let o_col_abs = o_tag_x + 1;

    let reversed_cols = |buf: &ratatui::buffer::Buffer| -> Vec<u16> {
        reversed_chart_cols(buf, chart_left, chart_right, top_row, bot_row)
    };

    // 1. Press Enter -> enters inspect mode initialized on [T] Trigger column
    assert_eq!(app.handle_event(TuiEvent::Key(key(KeyCode::Enter))), None);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_enter = term.backend().buffer();
    let out_enter = buffer_to_string(buf_enter);
    assert_eq!(
        reversed_cols(buf_enter),
        vec![t_col_abs],
        "Enter must initialize cursor at [T] Trigger column:\n{out_enter}"
    );
    // Underlying [T] marker glyph 'T' at top_row must remain visible beneath REVERSED highlight
    assert_eq!(buf_enter[(t_col_abs, top_row)].symbol(), "T");
    assert_eq!(buf_enter[(t_col_abs, top_row)].fg, Color::Red);
    // Bottom legend replaced by cursor readout and footer updated
    assert!(
        out_enter.contains("@ ") && out_enter.contains("HR:") && out_enter.contains("P(Sleep):"),
        "Legend row must swap to cursor readout while inspecting:\n{out_enter}"
    );
    assert!(
        find_footer_coords(buf_enter, "Scrub").is_some()
            && find_footer_coords(buf_enter, "Exit").is_some(),
        "Footer must show inspect hints while inspecting:\n{out_enter}"
    );

    // 2. Left/Right/h/l scrub by 1 column without switching tabs
    app.handle_event(TuiEvent::Key(key(KeyCode::Left)));
    assert_eq!(app.active_tab, Tab::History);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(reversed_cols(term.backend().buffer()), vec![t_col_abs - 1]);

    app.handle_event(TuiEvent::Key(key(KeyCode::Char('h'))));
    assert_eq!(app.active_tab, Tab::History);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(reversed_cols(term.backend().buffer()), vec![t_col_abs - 2]);

    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('l'))));
    assert_eq!(app.active_tab, Tab::History);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(reversed_cols(term.backend().buffer()), vec![t_col_abs]);

    // 3. Home / End jump to edges and clamp cleanly
    app.handle_event(TuiEvent::Key(key(KeyCode::Home)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Left))); // clamp at 0
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_home = term.backend().buffer();
    assert_eq!(reversed_cols(buf_home), vec![chart_left]);
    let expected_start_hms = chrono::Local
        .timestamp_millis_opt(start_ts)
        .single()
        .unwrap()
        .format("%H:%M:%S")
        .to_string();
    assert!(
        buffer_to_string(buf_home).contains(&format!("@ {expected_start_hms}")),
        "Home column must read out session start HH:MM:SS ({expected_start_hms})"
    );

    app.handle_event(TuiEvent::Key(key(KeyCode::End)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Right))); // clamp at chart_w - 1
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_end = term.backend().buffer();
    assert_eq!(reversed_cols(buf_end), vec![chart_right]);
    let expected_end_hms = chrono::Local
        .timestamp_millis_opt(end_ts)
        .single()
        .unwrap()
        .format("%H:%M:%S")
        .to_string();
    assert!(
        buffer_to_string(buf_end).contains(&format!("@ {expected_end_hms}")),
        "End column must read out session end HH:MM:SS ({expected_end_hms})"
    );

    // 4. Esc exits inspect mode and restores legend + tab-switching keys
    app.handle_event(TuiEvent::Key(key(KeyCode::Esc)));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_exit = term.backend().buffer();
    assert!(reversed_cols(buf_exit).is_empty());
    assert!(buffer_to_string(buf_exit).contains("● HR"));
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert_eq!(
        app.active_tab,
        Tab::Wearables,
        "Right arrow after Esc must switch tabs again"
    );
    app.handle_event(TuiEvent::Key(key(KeyCode::Left)));
    assert_eq!(app.active_tab, Tab::History);

    // 5. 'i' enters inspect mode, Enter exits inspect mode
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('i'))));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(reversed_cols(term.backend().buffer()), vec![t_col_abs]);
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert!(reversed_cols(term.backend().buffer()).is_empty());

    // 6. Tab switch while inspecting clears inspect mode
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('i'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('1'))));
    assert_eq!(app.active_tab, Tab::Dashboard);
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('2'))));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert!(
        reversed_cols(term.backend().buffer()).is_empty(),
        "Tab switch must clear inspect mode"
    );

    // 7. Entry fallback: [O] Onset when [T] is None, and midpoint when neither is present; session switch resets inspect mode
    app.selected_session_idx = 1;
    app.selected_session_detail = Some(d_onset_only);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(
        reversed_cols(term.backend().buffer()),
        vec![o_col_abs],
        "When [T] is absent, inspect mode must start at [O] Onset"
    );
    // Switching session via 'j' clears inspect mode
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    assert_eq!(app.selected_session_idx, 2);
    app.selected_session_detail = Some(d_neither);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert!(
        reversed_cols(term.backend().buffer()).is_empty(),
        "Switching sessions must clear inspect mode"
    );
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(
        reversed_cols(term.backend().buffer()),
        vec![chart_left + chart_w / 2],
        "When neither [T] nor [O] is present, inspect mode must start at midpoint"
    );

    // 8. Empty epochs session cannot enter inspect mode
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('j'))));
    assert_eq!(app.selected_session_idx, 3);
    app.selected_session_detail = Some(d_empty);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('i'))));
    app.handle_event(TuiEvent::Key(key(KeyCode::Right)));
    assert_eq!(
        app.active_tab,
        Tab::Wearables,
        "Enter/i with empty epochs must not activate inspect mode"
    );

    // 9. --ascii + NO_COLOR inspect mode preserves Reset colors, REVERSED highlight, and 7-bit ASCII
    let (s_ascii, d_ascii) = build_40m_session("sess-nc-ascii", Some(onset_ts), Some(trigger_ts));
    let mut nc_ascii_app = TuiApp::new(false, true, true);
    nc_ascii_app.daemon_connected = true;
    nc_ascii_app.active_tab = Tab::History;
    nc_ascii_app.history_sessions = vec![s_ascii];
    nc_ascii_app.selected_session_detail = Some(d_ascii);
    term.draw(|f| nc_ascii_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    nc_ascii_app.handle_event(TuiEvent::Key(key(KeyCode::Enter)));
    term.draw(|f| nc_ascii_app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let nc_buf = term.backend().buffer();
    let nc_out = buffer_to_string(nc_buf);
    assert!(
        nc_out.is_ascii(),
        "Inspect mode in --ascii must be 7-bit ASCII:\n{nc_out}"
    );
    for y in 0..nc_buf.area.height {
        for x in 0..nc_buf.area.width {
            assert_eq!(nc_buf[(x, y)].fg, Color::Reset);
            assert_eq!(nc_buf[(x, y)].bg, Color::Reset);
        }
    }
    assert!(
        !reversed_cols(nc_buf).is_empty(),
        "NO_COLOR inspect mode must highlight cursor column via Modifier::REVERSED"
    );
}

#[test]
fn test_mouse_history_chart_click_drag_scrub_gap_and_compact_inspect() {
    let start_dt = chrono::Local
        .with_ymd_and_hms(2026, 1, 12, 1, 0, 0)
        .single()
        .unwrap();
    let start_ts = start_dt.timestamp_millis();
    let end_ts = start_ts + 40 * 60_000; // 40m

    let mut entry = make_history_session(
        "sess-mouse-gap",
        "Polar H10",
        start_ts,
        end_ts,
        SessionStatus::Closed,
        100,
    );
    entry.warmup_mu_hr = Some(68.0);
    entry.sleep_mu_hr = Some(56.0);

    let mut epochs = Vec::new();
    let mut idx = 0u64;
    // 0..14m valid (68 bpm, p=0.2)
    for sec in (0..=14 * 60).step_by(15) {
        epochs.push(make_test_epoch(
            idx,
            start_ts + sec * 1_000,
            68.0,
            0.2,
            68.0,
        ));
        idx += 1;
    }
    // 14..26m Signal Lost gap (no epochs at all!)
    // 26..40m valid (56 bpm, p=0.9)
    for sec in (26 * 60..=40 * 60).step_by(15) {
        epochs.push(make_test_epoch(
            idx,
            start_ts + sec * 1_000,
            56.0,
            0.9,
            68.0,
        ));
        idx += 1;
    }
    entry.epochs_count = epochs.len() as u32;
    let detail = make_session_detail(&entry, epochs);

    let mut app = TuiApp::new(false, false, false);
    app.daemon_connected = true;
    app.active_tab = Tab::History;
    app.history_sessions = vec![entry];
    app.selected_session_idx = 0;
    app.selected_session_detail = Some(detail);

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf0 = term.backend().buffer().clone();

    let (chart_left, chart_right, top_row, bot_row) =
        history_chart_plot_bounds(&buf0, "70bpm", "54bpm");
    let chart_w = chart_right - chart_left + 1;
    let mid_row = (top_row + bot_row) / 2;

    let reversed_cols = |buf: &ratatui::buffer::Buffer| -> Vec<u16> {
        reversed_chart_cols(buf, chart_left, chart_right, top_row, bot_row)
    };

    // 1. Left-click on column 3 (inside 0..14m valid segment) activates inspect mode at that column
    let col_a = chart_left + 3;
    assert_eq!(app.handle_event(mouse_left_click(col_a, mid_row)), None);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_a = term.backend().buffer();
    let out_a = buffer_to_string(buf_a);
    assert_eq!(
        reversed_cols(buf_a),
        vec![col_a],
        "Left-click on plot column must activate cursor at that column:\n{out_a}"
    );
    assert!(
        out_a.contains("HR: 68 bpm") && out_a.contains("P(Sleep): 20%"),
        "Click on valid segment must read out HR and P(Sleep):\n{out_a}"
    );

    // 2. Left-drag into the middle of the Signal Lost gap (~20m = midpoint)
    let gap_col = chart_left + chart_w / 2;
    assert_eq!(app.handle_event(mouse_left_drag(gap_col, mid_row)), None);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_gap = term.backend().buffer();
    let out_gap = buffer_to_string(buf_gap);
    assert_eq!(
        reversed_cols(buf_gap),
        vec![gap_col],
        "Left-drag across plot area must scrub cursor to gap column:\n{out_gap}"
    );
    assert!(
        out_gap.contains("@ ")
            && out_gap.contains("HR: -- bpm")
            && out_gap.contains("P(Sleep): --%")
            && out_gap.contains("gap"),
        "Scrubbing onto Signal Lost gap must show HH:MM:SS and '--' / 'gap' readout:\n{out_gap}"
    );

    // 3. Left-drag onto second valid segment (near right end)
    let col_b = chart_right - 2;
    assert_eq!(app.handle_event(mouse_left_drag(col_b, mid_row)), None);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    let buf_b = term.backend().buffer();
    let out_b = buffer_to_string(buf_b);
    assert_eq!(reversed_cols(buf_b), vec![col_b]);
    assert!(
        out_b.contains("HR: 56 bpm") && out_b.contains("P(Sleep): 90%"),
        "Drag onto second valid segment must show updated HR/P(Sleep):\n{out_b}"
    );

    // 4. Wheel scroll over plot area remains a no-op
    assert_eq!(app.handle_event(mouse_scroll_down(col_b, mid_row)), None);
    assert_eq!(app.handle_event(mouse_scroll_up(col_b, mid_row)), None);
    assert_eq!(app.selected_session_idx, 0);

    // 5. Modal HitMap shadowing: while FeedbackModal is open, clicking/dragging chart coords does not move cursor
    app.handle_event(TuiEvent::Key(key(KeyCode::Char('f'))));
    assert!(matches!(app.modal, ModalState::FeedbackModal { .. }));
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    app.handle_event(mouse_left_click(col_a, mid_row));
    app.handle_event(mouse_left_drag(col_a + 1, mid_row));
    app.handle_event(TuiEvent::Key(key(KeyCode::Esc)));
    assert_eq!(app.modal, ModalState::None);
    term.draw(|f| app.render(f.area(), f.buffer_mut())).unwrap();
    assert_eq!(
        reversed_cols(term.backend().buffer()),
        vec![col_b],
        "Modal HitMap shadowing must prevent chart cursor movement while modal is open"
    );

    // 6. Compact 60x18 viewport renders active inspect mode cleanly without panic or overflow
    let mut compact_term = Terminal::new(TestBackend::new(60, 18)).unwrap();
    compact_term
        .draw(|f| app.render(f.area(), f.buffer_mut()))
        .unwrap();
    let compact_out = buffer_to_string(compact_term.backend().buffer());
    assert!(
        compact_out.contains("@ "),
        "Compact 60x18 inspect mode must render '@ HH:MM:SS' readout:\n{compact_out}"
    );
}
