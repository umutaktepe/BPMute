mod common;

use std::sync::Arc;

use bpmute::bluetooth::packet::parse_hr_measurement;
use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{
    DaemonHandle, DaemonOptions, VirtualClock, WearableDescriptor, WearableEvent, WearableSource,
};
use bpmute::ipc::IpcClient;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{SessionEndReason, StreamDisconnectReason, StreamState, StreamStatus};
use bpmute::storage::{LedgerStore, db_path_in_data_dir, lock_path_in_data_dir};
use chrono::{TimeZone, Utc};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

struct ChannelWearableSource {
    event_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<WearableEvent>>>,
}

impl ChannelWearableSource {
    fn new(rx: mpsc::Receiver<WearableEvent>) -> Self {
        Self {
            event_rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }
}

impl WearableSource for ChannelWearableSource {
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
                    res = shutdown_rx.changed() => {
                        if res.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });
        out_rx
    }
}

struct TestHarness {
    _temp: TempDir,
    paths: ResolvedPaths,
    _clock: Arc<VirtualClock>,
    t0: i64,
    feed_tx: mpsc::Sender<WearableEvent>,
    daemon: DaemonHandle,
    wearable: WearableDescriptor,
}

impl TestHarness {
    async fn new(start_utc: chrono::DateTime<Utc>) -> Self {
        let temp = TempDir::new().unwrap();
        let paths = ResolvedPaths {
            config_file: temp.path().join("cfg/bpmute/config.toml"),
            data_dir: temp.path().join("data/bpmute"),
            socket_path: temp.path().join("run/bpmute/daemon.sock"),
        };
        let env = MapEnv::new();
        let cli = CliOverrides::default();
        let loaded = LoadedConfig::load(&paths.config_file, &env, &cli).unwrap();

        let clock = Arc::new(VirtualClock::starting_at(start_utc));
        let t0 = start_utc.timestamp_millis();

        let (feed_tx, feed_rx) = mpsc::channel(64);
        let wearable_source = Arc::new(ChannelWearableSource::new(feed_rx));

        let daemon = DaemonHandle::spawn(
            DaemonOptions::for_testing(paths.clone(), loaded, Arc::new(env), clock.clone())
                .with_wearable_source(wearable_source),
        )
        .await
        .unwrap();

        let wearable = WearableDescriptor {
            mac: "AA:BB:CC:DD:EE:99".to_string(),
            address_type: "random".to_string(),
            alias: Some("Test Strap".to_string()),
            advertised_name: Some("Polar H10".to_string()),
            supports_rr: true,
            supports_contact: true,
        };

        Self {
            _temp: temp,
            paths,
            _clock: clock,
            t0,
            feed_tx,
            daemon,
            wearable,
        }
    }
}

async fn complete_warmup_stream(
    feed_tx: &mpsc::Sender<WearableEvent>,
    wearable: &WearableDescriptor,
    sub_client: &mut IpcClient,
    start_ts_ms: i64,
) -> i64 {
    common::daemon::send_synthetic_wave_stream(
        feed_tx,
        wearable,
        common::daemon::SyntheticStreamParams {
            start_ts_ms,
            count_sec: 500,
            base_bpm: 68.0,
            bpm_amplitude: 2.4,
            contact: bpmute::sleep_engine::ContactStatus::Contact,
        },
    )
    .await;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data["monitoring_state"] == "Monitoring" {
            break;
        }
    }
    start_ts_ms + 500_000
}

#[tokio::test]
async fn virtual_time_wearable_source_stream_monitoring_and_session_recording() {
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let h = TestHarness::new(start_utc).await;
    let paths = h.paths;
    let t0 = h.t0;
    let feed_tx = h.feed_tx;
    let daemon = h.daemon;
    let wearable = h.wearable;

    // 1. Initial status -> running
    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let status_res = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(status_res["daemon_running"], true);

    let mut sub_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub_client
        .request("daemon.subscribe", json!({}))
        .await
        .unwrap();

    // 2. Feed stream state: Connecting -> Acquiring -> Streaming
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Connecting,
                ts_ms: t0,
            },
        })
        .await
        .unwrap();

    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Acquiring,
                ts_ms: t0 + 1000,
            },
        })
        .await
        .unwrap();

    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t0 + 2000,
            },
        })
        .await
        .unwrap();

    // 3. Feed packets:
    // a. Standard 8-bit HR packet
    let p1 = parse_hr_measurement(&[0x00, 68], t0 + 2500).unwrap();
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: p1,
        })
        .await
        .unwrap();

    // b. 16-bit HR packet (75 bpm)
    let p2 = parse_hr_measurement(&[0x01, 75, 0x00], t0 + 3500).unwrap();
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: p2,
        })
        .await
        .unwrap();

    // c. Huawei Watch GT5 2-byte payload with contact bit set (0x16: 8-bit BPM, contact detected, energy expended omitted)
    let p3 = parse_hr_measurement(&[0x16, 62], t0 + 4500).unwrap();
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: p3,
        })
        .await
        .unwrap();

    // d. Packet with RR-intervals (1024/1024 s = 1.0 s = 1000 ms)
    let p4 = parse_hr_measurement(&[0x10, 60, 0x00, 0x04], t0 + 5500).unwrap();
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: p4,
        })
        .await
        .unwrap();

    // Wait until the daemon has opened the session and broadcast the final ingested HR packet
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "HrSample" && ev.data["ts_ms"].as_i64() == Some(t0 + 5500) {
            break;
        }
    }

    // 4. Verify session list now has 1 open monitoring session
    let list_res = client
        .request("session.list", json!({ "limit": 10 }))
        .await
        .unwrap();
    let sessions = list_res["sessions"].as_array().unwrap();
    assert_eq!(
        sessions.len(),
        1,
        "Must open a monitoring session on first valid HR packet"
    );
    let session_id = sessions[0]["id"].as_str().unwrap().to_string();
    assert_eq!(sessions[0]["wearable_alias"].as_str(), Some("Test Strap"));

    // 5. Complete Warmup into Monitoring, then feed Disconnected and close the stream source
    let t_after_warmup =
        complete_warmup_stream(&feed_tx, &wearable, &mut sub_client, t0 + 6000).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Disconnected,
                ts_ms: t_after_warmup + 1000,
            },
        })
        .await
        .unwrap();
    drop(feed_tx);

    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            break;
        }
    }

    // 6. Verify session stored in SQLite ledger directly with end_reason StreamEnded
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let detail = store.get_session(&session_id).unwrap();
    assert_eq!(detail.session.end_reason, SessionEndReason::StreamEnded);
    assert_eq!(detail.wearable.alias.as_deref(), Some("Test Strap"));

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn virtual_time_sustained_disconnection_gap_timeout_closes_session() {
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let h = TestHarness::new(start_utc).await;
    let paths = h.paths;
    let t0 = h.t0;
    let feed_tx = h.feed_tx;
    let daemon = h.daemon;
    let wearable = h.wearable;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let mut sub_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub_client
        .request("daemon.subscribe", json!({}))
        .await
        .unwrap();

    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t0,
            },
        })
        .await
        .unwrap();

    let p1 = parse_hr_measurement(&[0x00, 68], t0 + 1000).unwrap();
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: p1,
        })
        .await
        .unwrap();

    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionStarted" {
            break;
        }
    }

    let list_res = client
        .request("session.list", json!({ "limit": 10 }))
        .await
        .unwrap();
    let sessions = list_res["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    let session_id = sessions[0]["id"].as_str().unwrap().to_string();
    assert_eq!(sessions[0]["status"].as_str(), Some("active"));

    // Complete Warmup into Monitoring before disconnecting
    let t_mon = complete_warmup_stream(&feed_tx, &wearable, &mut sub_client, t0 + 2000).await;
    tokio::time::advance(std::time::Duration::from_secs(502)).await;

    // Disconnect at t_mon + 1000 ms. Do NOT drop feed_tx!
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Disconnected,
                ts_ms: t_mon + 1000,
            },
        })
        .await
        .unwrap();

    // Advance 16 minutes (960s > 900s MEDIUM_GAP_MAX_MS) to trigger sustained gap timeout
    tokio::time::advance(std::time::Duration::from_secs(960)).await;

    // Await SessionClosed event emitted on gap timeout
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            assert_eq!(
                ev.data["session"]["session"]["id"].as_str(),
                Some(session_id.as_str())
            );
            assert_eq!(
                ev.data["session"]["session"]["end_reason"].as_str(),
                Some("gap_exceeded")
            );
            break;
        }
    }

    // Verify daemon status is now Idle
    let final_status = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(final_status["monitoring_state"].as_str(), Some("Idle"));

    // Verify session in SQLite is closed with end_reason 'gap_exceeded'
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let detail = store.get_session(&session_id).unwrap();
    assert_eq!(detail.session.end_reason, SessionEndReason::GapExceeded);

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn virtual_time_tiered_disconnect_deadlines_tightening_and_status_hydration() {
    use std::time::Duration;

    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let h = TestHarness::new(start_utc).await;
    let paths = h.paths;
    let t0 = h.t0;
    let feed_tx = h.feed_tx;
    let daemon = h.daemon;
    let wearable = h.wearable;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let mut sub_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub_client
        .request("daemon.subscribe", json!({}))
        .await
        .unwrap();
    let initial_snap = sub_client.next_event().await.unwrap();
    assert_eq!(initial_snap.event, "Snapshot");
    assert!(
        initial_snap.data["status"]
            .get("session_close_deadline_ms")
            .is_none(),
        "Idle Snapshot must omit session_close_deadline_ms"
    );

    // 1. Start Streaming + valid packet at t0 + 1_000 -> session 1 opens.
    tokio::time::advance(Duration::from_secs(1)).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t0 + 1_000,
            },
        })
        .await
        .unwrap();
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: parse_hr_measurement(&[0x00, 68], t0 + 1_500).unwrap(),
        })
        .await
        .unwrap();

    let session_1_id = loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionStarted" {
            break ev.data["session_id"].as_str().unwrap().to_string();
        }
    };

    // Complete Warmup into Monitoring so session 1 persists on GapExceeded.
    let w1_end = complete_warmup_stream(&feed_tx, &wearable, &mut sub_client, t0 + 2_000).await;
    tokio::time::advance(Duration::from_secs(501)).await;

    // 2. Explicit Remote disconnect at w1_end -> deadline = w1_end + 120_001 via StateChanged, daemon.status, and Snapshot.
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: w1_end,
            reason: StreamDisconnectReason::Remote,
        })
        .await
        .unwrap();

    let expected_remote_deadline = w1_end + 120_001;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data.get("session_close_deadline_ms").is_some() {
            assert_eq!(
                ev.data["session_close_deadline_ms"].as_i64(),
                Some(expected_remote_deadline)
            );
            break;
        }
    }

    let status_dc = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(
        status_dc["session_close_deadline_ms"].as_i64(),
        Some(expected_remote_deadline)
    );

    let mut late_sub = IpcClient::connect(&paths.socket_path).await.unwrap();
    late_sub
        .request("daemon.subscribe", json!({}))
        .await
        .unwrap();
    let late_snap = late_sub.next_event().await.unwrap();
    assert_eq!(late_snap.event, "Snapshot");
    assert_eq!(
        late_snap.data["status"]["session_close_deadline_ms"].as_i64(),
        Some(expected_remote_deadline)
    );
    drop(late_sub);

    // 3. Reconnect within 2 minutes (at w1_end + 60_000) -> clears deadline and continues session 1.
    tokio::time::advance(Duration::from_secs(60)).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: w1_end + 60_000,
            },
        })
        .await
        .unwrap();

    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data.get("session_close_deadline_ms").is_none() {
            break;
        }
    }
    let status_resumed = client.request("daemon.status", json!({})).await.unwrap();
    assert!(status_resumed.get("session_close_deadline_ms").is_none());

    // 4. SignalLost at w1_end + 68_000 (+15 min), then Remote at w1_end + 98_000 (tightens to +2 min from w1_end + 98_000).
    let t_sig_lost = w1_end + 68_000;
    tokio::time::advance(Duration::from_secs(8)).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::SignalLost,
                ts_ms: t_sig_lost,
            },
        })
        .await
        .unwrap();

    let expected_sig_deadline = t_sig_lost + 900_001;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged"
            && ev.data["session_close_deadline_ms"].as_i64() == Some(expected_sig_deadline)
        {
            break;
        }
    }

    tokio::time::advance(Duration::from_secs(30)).await;
    let t_remote_tighten = t_sig_lost + 30_000;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: t_remote_tighten,
            reason: StreamDisconnectReason::Remote,
        })
        .await
        .unwrap();

    let expected_tight_deadline = t_remote_tighten + 120_001;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged"
            && ev.data["session_close_deadline_ms"].as_i64() == Some(expected_tight_deadline)
        {
            break;
        }
    }

    // Still active at +115s after Remote disconnect.
    tokio::time::advance(Duration::from_secs(115)).await;
    tokio::task::yield_now().await;
    let status_pre_close = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(status_pre_close["monitoring_state"], "Monitoring");
    assert_eq!(
        status_pre_close["session_close_deadline_ms"].as_i64(),
        Some(expected_tight_deadline)
    );

    // Advance past tightened 2-min deadline (+6s) -> closes with end_ts_ms = t_sig_lost (original SignalLost).
    tokio::time::advance(Duration::from_secs(6)).await;
    let mut saw_idle_clear = false;
    let mut saw_closed = false;
    while !(saw_idle_clear && saw_closed) {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data["monitoring_state"] == "Idle" {
            assert!(ev.data.get("session_close_deadline_ms").is_none());
            saw_idle_clear = true;
        } else if ev.event == "SessionClosed" {
            assert_eq!(
                ev.data["session"]["session"]["id"].as_str(),
                Some(session_1_id.as_str())
            );
            assert_eq!(
                ev.data["session"]["session"]["end_reason"].as_str(),
                Some("gap_exceeded")
            );
            assert_eq!(
                ev.data["session"]["session"]["end_ts_ms"].as_i64(),
                Some(t_sig_lost)
            );
            saw_closed = true;
        }
    }

    // 5. Timeout disconnect preserves 15-minute tolerance (> 2 min stays open, > 15 min closes).
    let t_s2 = t_remote_tighten + 200_000;
    tokio::time::advance(Duration::from_secs(79)).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t_s2,
            },
        })
        .await
        .unwrap();
    let session_2_id = loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionStarted" {
            break ev.data["session_id"].as_str().unwrap().to_string();
        }
    };
    assert_ne!(session_2_id, session_1_id);

    let w2_end = complete_warmup_stream(&feed_tx, &wearable, &mut sub_client, t_s2 + 1_000).await;
    tokio::time::advance(Duration::from_secs(501)).await;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: w2_end,
            reason: StreamDisconnectReason::Timeout,
        })
        .await
        .unwrap();

    let expected_timeout_deadline = w2_end + 900_001;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged"
            && ev.data["session_close_deadline_ms"].as_i64() == Some(expected_timeout_deadline)
        {
            break;
        }
    }

    // Advance 130s (past 2 min) -> must remain open!
    tokio::time::advance(Duration::from_secs(130)).await;
    tokio::task::yield_now().await;
    let status_mid_timeout = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(status_mid_timeout["monitoring_state"], "Monitoring");
    assert_eq!(
        status_mid_timeout["session_close_deadline_ms"].as_i64(),
        Some(expected_timeout_deadline)
    );

    // Advance remaining 771s (total 901s > 900s) -> closes session 2 with GapExceeded at w2_end.
    tokio::time::advance(Duration::from_secs(771)).await;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            assert_eq!(
                ev.data["session"]["session"]["id"].as_str(),
                Some(session_2_id.as_str())
            );
            assert_eq!(
                ev.data["session"]["session"]["end_reason"].as_str(),
                Some("gap_exceeded")
            );
            assert_eq!(
                ev.data["session"]["session"]["end_ts_ms"].as_i64(),
                Some(w2_end)
            );
            break;
        }
    }

    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let d1 = store.get_session(&session_1_id).unwrap();
    assert_eq!(d1.session.end_reason, SessionEndReason::GapExceeded);
    assert_eq!(d1.session.end_ts_ms, t_sig_lost);
    let d2 = store.get_session(&session_2_id).unwrap();
    assert_eq!(d2.session.end_reason, SessionEndReason::GapExceeded);
    assert_eq!(d2.session.end_ts_ms, w2_end);

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn virtual_time_gap_gated_manual_session_closure_and_rejections() {
    use bpmute::ipc::IpcClientError;
    use bpmute::sleep_engine::{ContactStatus, HrPacket};
    use bpmute::storage::SessionFullDetail;

    let start_utc = chrono::DateTime::parse_from_rfc3339("2026-11-22T22:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let harness = TestHarness::new(start_utc).await;
    let paths = harness.paths.clone();
    let feed_tx = harness.feed_tx.clone();
    let daemon = harness.daemon;
    let wearable = harness.wearable.clone();
    let t0 = harness.t0;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let mut sub_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub_client
        .request("daemon.subscribe", json!({}))
        .await
        .unwrap();

    // 1. Rejection on Idle
    let idle_err = client.request("session.end", json!({})).await.unwrap_err();
    assert_eq!(idle_err.code(), Some("PRECONDITION_FAILED"));

    // 2. Start streaming and open session
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t0,
            },
        })
        .await
        .unwrap();

    for i in 0..20 {
        feed_tx
            .send(WearableEvent::HrPacket {
                wearable: wearable.clone(),
                packet: HrPacket {
                    ts_ms: t0 + (i * 1000),
                    bpm: 65.0,
                    rr_ms: vec![923.0],
                    contact_status: ContactStatus::Contact,
                },
            })
            .await
            .unwrap();
    }

    // Wait for SessionStarted
    let session_id = loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionStarted" {
            break ev.data["session_id"].as_str().unwrap().to_string();
        }
    };

    // 3. Rejection while Streaming
    let stream_err = client.request("session.end", json!({})).await.unwrap_err();
    assert_eq!(stream_err.code(), Some("PRECONDITION_FAILED"));
    if let IpcClientError::Server { message, .. } = stream_err {
        assert!(message.contains("streaming"));
    } else {
        panic!("expected Server error");
    }

    // Complete Warmup into Monitoring before disconnecting
    let t_after_warmup =
        complete_warmup_stream(&feed_tx, &wearable, &mut sub_client, t0 + 20_000).await;

    // Feed a few trailing raw packets that do not form a complete 15-s epoch
    let t_trailing = t_after_warmup;
    feed_tx
        .send(WearableEvent::HrPacket {
            wearable: wearable.clone(),
            packet: HrPacket {
                ts_ms: t_trailing,
                bpm: 64.0,
                rr_ms: vec![937.0],
                contact_status: ContactStatus::Contact,
            },
        })
        .await
        .unwrap();

    // 4. Disconnect wearable (start gap at t_dc)
    let t_dc = t_after_warmup + 5_000;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: t_dc,
            reason: StreamDisconnectReason::Remote,
        })
        .await
        .unwrap();

    // Await StateChanged with session_close_deadline_ms
    let expected_deadline = t_dc + 120_001;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data.get("session_close_deadline_ms").is_some() {
            assert_eq!(
                ev.data["session_close_deadline_ms"].as_i64(),
                Some(expected_deadline)
            );
            break;
        }
    }

    // Verify session_close_deadline_ms is now exposed
    let status_in_gap = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(
        status_in_gap["session_close_deadline_ms"].as_i64(),
        Some(expected_deadline)
    );

    // 5. Explicit manual closure during gap
    let close_resp = client.request("session.end", json!({})).await.unwrap();
    let detail: SessionFullDetail = serde_json::from_value(close_resp).unwrap();
    assert_eq!(detail.session.id, session_id);
    assert_eq!(detail.session.end_reason, SessionEndReason::ManualClose);
    assert_eq!(detail.session.end_ts_ms, t_dc);

    // Verify SessionClosed event emitted
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionClosed" {
            assert_eq!(
                ev.data["session"]["session"]["id"].as_str(),
                Some(session_id.as_str())
            );
            assert_eq!(
                ev.data["session"]["session"]["end_reason"].as_str(),
                Some("manual_close")
            );
            assert_eq!(
                ev.data["session"]["session"]["end_ts_ms"].as_i64(),
                Some(t_dc)
            );
            break;
        }
    }

    // Verify daemon status is Idle and deadline is cleared
    let final_status = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(final_status["monitoring_state"].as_str(), Some("Idle"));
    assert!(final_status["session_close_deadline_ms"].is_null());
    assert!(final_status["active_session_id"].is_null());

    // Verify SQLite persistence
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let stored_detail = store.get_session(&session_id).unwrap();
    assert_eq!(
        stored_detail.session.end_reason,
        SessionEndReason::ManualClose
    );
    assert_eq!(stored_detail.session.end_ts_ms, t_dc);
    // Trailing raw sample was flushed
    assert!(stored_detail.session.raw_samples_count >= 1);

    // 6. Subsequent call rejected on Idle
    let subsequent_err = client.request("session.end", json!({})).await.unwrap_err();
    assert_eq!(subsequent_err.code(), Some("PRECONDITION_FAILED"));

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn virtual_time_manual_session_closure_aborts_active_grace_without_penalty() {
    use bpmute::sleep_engine::{ContactStatus, HrPacket};
    use bpmute::storage::SessionFullDetail;

    let start_utc = chrono::DateTime::parse_from_rfc3339("2026-11-22T23:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let harness = TestHarness::new(start_utc).await;
    let paths = harness.paths.clone();
    let feed_tx = harness.feed_tx.clone();
    let daemon = harness.daemon;
    let wearable = harness.wearable.clone();
    let t0 = harness.t0;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let mut sub_client = IpcClient::connect(&paths.socket_path).await.unwrap();
    sub_client
        .request("daemon.subscribe", json!({}))
        .await
        .unwrap();

    // 1. Start streaming and open session
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t0,
            },
        })
        .await
        .unwrap();

    for i in 0..10 {
        feed_tx
            .send(WearableEvent::HrPacket {
                wearable: wearable.clone(),
                packet: HrPacket {
                    ts_ms: t0 + (i * 1000),
                    bpm: 62.0,
                    rr_ms: vec![968.0],
                    contact_status: ContactStatus::Contact,
                },
            })
            .await
            .unwrap();
    }

    let session_id = loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "SessionStarted" {
            break ev.data["session_id"].as_str().unwrap().to_string();
        }
    };

    // Complete Warmup into Monitoring before triggering grace / manual closure
    let t_after_warmup =
        complete_warmup_stream(&feed_tx, &wearable, &mut sub_client, t0 + 10_000).await;

    // 2. Start Grace Period via test trigger
    let trig_res = client
        .request("monitor.test_trigger", json!({ "grace_sec": 60 }))
        .await
        .unwrap();
    assert_eq!(trig_res["test_trigger_started"], true);
    assert!(daemon.core().is_in_grace_period());

    // 3. Disconnect wearable (start gap at t_dc while Grace Period is active)
    let t_dc = t_after_warmup + 10_000;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: t_dc,
            reason: StreamDisconnectReason::Remote,
        })
        .await
        .unwrap();

    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged" && ev.data.get("session_close_deadline_ms").is_some() {
            break;
        }
    }

    // 4. Manually close session during disconnect gap
    let close_resp = client.request("session.end", json!({})).await.unwrap();
    let detail: SessionFullDetail = serde_json::from_value(close_resp).unwrap();
    assert_eq!(detail.session.id, session_id);
    assert_eq!(detail.session.end_reason, SessionEndReason::ManualClose);
    assert_eq!(detail.session.end_ts_ms, t_dc);
    assert_eq!(
        detail.session.grace_cancel_count, 0,
        "must not record false-positive penalty"
    );

    // 5. Verify Grace Period was aborted and daemon is Idle
    assert!(!daemon.core().is_in_grace_period());
    let final_status = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(final_status["monitoring_state"].as_str(), Some("Idle"));
    assert!(final_status["session_close_deadline_ms"].is_null());

    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let stored = store.get_session(&session_id).unwrap();
    assert_eq!(stored.session.grace_cancel_count, 0);
    assert_eq!(stored.session.end_reason, SessionEndReason::ManualClose);

    daemon.shutdown().await;
}
