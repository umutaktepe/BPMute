#![allow(
    dead_code,
    reason = "shared test harness items are used selectively by separate integration test binaries"
)]

use std::sync::Arc;

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

use bpmute::calibration::SessionLedgerRecord;
use bpmute::config::{CliOverrides, LoadedConfig, PowerAction};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock, WearableEvent, WearableSource};
use bpmute::ipc::protocol::EnrolledWearableStatus;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{
    ContactStatus, DetectorState, Epoch, EpochFeatures, HrPacket, SessionEndReason, SessionSummary,
    StreamStatus,
};
use bpmute::storage::{
    EnrolledWearable, SessionFullDetail, SessionListEntry, SessionStatus, WearableInfo,
};
use bpmute::tui::{TuiApp, TuiCommand, TuiEvent, TuiIpcBridge};

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

pub fn make_test_epoch(
    session_index: u64,
    ts_ms: i64,
    hr: f64,
    p_sleep: f64,
    baseline_mu_hr: f64,
) -> Epoch {
    Epoch {
        session_index,
        ts_ms,
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
        baseline_mu_hr,
        baseline_sigma_hr: 2.0,
        baseline_frozen: false,
        p_active_wake: 0.1,
        p_quiet_wake: (1.0 - p_sleep - 0.1).max(0.0),
        p_sleep,
        state: DetectorState::Monitoring,
    }
}

pub fn make_session_summary(
    session_index: u64,
    start_ts_ms: i64,
    end_ts_ms: i64,
    with_raw_samples: bool,
) -> SessionSummary {
    SessionSummary {
        session_index,
        start_ts_ms,
        end_ts_ms,
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
    }
}

pub fn is_non_blank_braille(ch: char) -> bool {
    ('\u{2801}'..='\u{28FF}').contains(&ch)
}

pub async fn wait_for_app_state_with_cmds<F>(
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
    panic!(
        "TUI app did not reach expected state within event budget (256 events). Final state: tab={:?}, modal={:?}, sel_session_idx={}, sessions={:?}, sel_session_detail={:?}",
        app.active_tab,
        app.modal,
        app.selected_session_idx,
        app.history_sessions
            .iter()
            .map(|s| &s.id)
            .collect::<Vec<_>>(),
        app.selected_session_detail.as_ref().map(|d| &d.session.id),
    );
}

pub async fn wait_for_app_state<F>(
    app: &mut TuiApp,
    event_rx: &mut mpsc::Receiver<TuiEvent>,
    pred: F,
) where
    F: FnMut(&TuiApp) -> bool,
{
    wait_for_app_state_with_cmds(app, event_rx, None, pred).await;
}

pub struct RealSocketTuiHarness {
    pub _temp: TempDir,
    pub paths: ResolvedPaths,
    pub daemon: DaemonHandle,
    pub event_rx: mpsc::Receiver<TuiEvent>,
    pub cmd_tx: mpsc::Sender<TuiCommand>,
    pub shutdown_tx: watch::Sender<bool>,
    pub bridge_task: tokio::task::JoinHandle<()>,
    pub app: TuiApp,
    pub session_ids: Vec<String>,
}

impl RealSocketTuiHarness {
    pub async fn spawn(session_count: usize, with_raw_samples: bool) -> Self {
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
                            summary: make_session_summary(
                                (i + 1) as u64,
                                start_ts,
                                end_ts,
                                with_raw_samples,
                            ),
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

    pub fn insert_closed_session(&self, session_index: u64, start_ts: i64, end_ts: i64) -> String {
        let db_file = bpmute::storage::db_path_in_data_dir(&self.paths.data_dir);
        let db_lock = bpmute::storage::lock_path_in_data_dir(&self.paths.data_dir);
        let mut store = bpmute::storage::LedgerStore::open_read_write(&db_file, &db_lock).unwrap();
        let wid = store
            .list_enrolled_wearables()
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .id;
        let sid = store
            .open_session(&bpmute::storage::OpenSessionRequest {
                wearable_id: wid,
                start_ts_ms: start_ts,
                detection_sensitivity: bpmute::config::Sensitivity::Balanced,
            })
            .unwrap();
        store
            .close_session_and_recompute(
                &bpmute::storage::CloseSessionRequest {
                    session_id: sid.clone(),
                    summary: make_session_summary(session_index, start_ts, end_ts, false),
                    trigger_power_action: PowerAction::None,
                    rapid_resume_within_5m: false,
                },
                end_ts,
            )
            .unwrap();
        sid
    }

    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.bridge_task.await;
        self.daemon.shutdown().await;
    }
}

pub struct LiveStreamTestSource {
    pub event_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<WearableEvent>>>,
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
