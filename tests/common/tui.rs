#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use tempfile::TempDir;

use bpmute::calibration::SessionLedgerRecord;
use bpmute::config::PowerAction;
use bpmute::ipc::protocol::EnrolledWearableStatus;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{Epoch, SessionEndReason, StreamStatus};
use bpmute::storage::{
    EnrolledWearable, SessionFullDetail, SessionListEntry, SessionStatus, WearableInfo,
};
use bpmute::tui::TuiEvent;

pub fn buffer_to_string(buf: &Buffer) -> String {
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

pub fn find_text_coords_in_row(buf: &Buffer, row: u16, needle: &str) -> Option<(u16, u16)> {
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

pub fn find_text_coords(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    for y in 0..buf.area.height {
        if let Some(coords) = find_text_coords_in_row(buf, y, needle) {
            return Some(coords);
        }
    }
    None
}

pub fn find_footer_coords(buf: &Buffer, needle: &str) -> Option<(u16, u16)> {
    find_text_coords_in_row(buf, buf.area.height.saturating_sub(1), needle)
}

pub fn mouse_left_click(col: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

pub fn mouse_scroll_down(col: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

pub fn mouse_scroll_up(col: u16, row: u16) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: col,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

pub fn make_wearable(
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

pub fn make_test_paths(temp: &TempDir) -> (ResolvedPaths, MapEnv) {
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

pub fn make_history_session(
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

pub fn make_session_detail(entry: &SessionListEntry, epochs: Vec<Epoch>) -> SessionFullDetail {
    SessionFullDetail {
        session: SessionLedgerRecord {
            id: entry.id.clone(),
            wearable_id: entry.wearable_id.clone(),
            start_ts_ms: entry.start_ts_ms,
            end_ts_ms: entry.end_ts_ms,
            end_reason: entry.end_reason.unwrap_or(SessionEndReason::StreamEnded),
            detection_sensitivity: entry.detection_sensitivity,
            had_rr: entry.had_rr,
            warmup_mu_hr: entry.warmup_mu_hr,
            warmup_sigma_hr: Some(2.5),
            warmup_ln_rmssd: Some(3.5),
            onset_candidate_ts_ms: None,
            onset_confirmed_ts_ms: entry.onset_confirmed_ts_ms,
            sleeping_ts_ms: entry.sleeping_ts_ms,
            sleep_mu_hr: entry.sleep_mu_hr,
            onset_hr_bpm: entry.sleep_mu_hr,
            delta_hr_onset: None,
            sleep_ln_rmssd: Some(3.8),
            sleep_hr_var_ratio: Some(0.25),
            typical_sqi: Some(0.95),
            post_trigger_sleep_sec: 0,
            trigger_power_action: PowerAction::None,
            rapid_resume_within_5m: false,
            grace_cancel_count: 0,
            rapid_wake_fp_count: 0,
            snooze_count: 0,
            auto_anomalous: entry.auto_anomalous,
            epochs_count: epochs.len() as u32,
            raw_samples_count: 0,
        },
        status: entry.status,
        wearable: WearableInfo {
            id: entry.wearable_id.clone(),
            mac_masked: entry.wearable_mac_masked.clone(),
            alias: entry.wearable_alias.clone(),
            advertised_name: entry.wearable_alias.clone(),
            supports_rr: entry.had_rr,
            supports_contact: true,
            hr_var_floor: 0.15,
            typical_sqi: Some(0.95),
        },
        feedback: entry.feedback.clone(),
        epochs,
        has_raw_samples: false,
    }
}
