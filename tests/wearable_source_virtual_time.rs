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

    // 5. Feed StreamState Disconnected and close the stream source to flush/close the session
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Disconnected,
                ts_ms: t0 + 6000,
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

    // Disconnect at t0 + 2000 ms. Do NOT drop feed_tx!
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Disconnected,
                ts_ms: t0 + 2000,
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

    // 2. Explicit Remote disconnect at t0 + 2_000 -> deadline = t0 + 122_001 via StateChanged, daemon.status, and Snapshot.
    tokio::time::advance(Duration::from_secs(1)).await;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: t0 + 2_000,
            reason: StreamDisconnectReason::Remote,
        })
        .await
        .unwrap();

    let expected_remote_deadline = t0 + 2_000 + 120_001;
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

    // 3. Reconnect within 2 minutes (at t0 + 62_000) -> clears deadline and continues session 1.
    tokio::time::advance(Duration::from_secs(60)).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::Streaming,
                ts_ms: t0 + 62_000,
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

    // 4. SignalLost at t0 + 70_000 (+15 min), then Remote at t0 + 100_000 (tightens to +2 min from t0 + 100_000).
    tokio::time::advance(Duration::from_secs(8)).await;
    feed_tx
        .send(WearableEvent::StreamState {
            wearable: wearable.clone(),
            state: StreamState {
                status: StreamStatus::SignalLost,
                ts_ms: t0 + 70_000,
            },
        })
        .await
        .unwrap();

    let expected_sig_deadline = t0 + 70_000 + 900_001;
    loop {
        let ev = sub_client.next_event().await.unwrap();
        if ev.event == "StateChanged"
            && ev.data["session_close_deadline_ms"].as_i64() == Some(expected_sig_deadline)
        {
            break;
        }
    }

    tokio::time::advance(Duration::from_secs(30)).await;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: t0 + 100_000,
            reason: StreamDisconnectReason::Remote,
        })
        .await
        .unwrap();

    let expected_tight_deadline = t0 + 100_000 + 120_001;
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
    assert_eq!(status_pre_close["monitoring_state"], "Warmup");
    assert_eq!(
        status_pre_close["session_close_deadline_ms"].as_i64(),
        Some(expected_tight_deadline)
    );

    // Advance past tightened 2-min deadline (+6s -> t0 + 221_000) -> closes with end_ts_ms = t0 + 70_000 (original SignalLost).
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
                Some(t0 + 70_000)
            );
            saw_closed = true;
        }
    }

    // 5. Timeout disconnect preserves 15-minute tolerance (> 2 min stays open, > 15 min closes).
    let t_s2 = t0 + 300_000;
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

    tokio::time::advance(Duration::from_secs(1)).await;
    feed_tx
        .send(WearableEvent::StreamDisconnected {
            wearable: wearable.clone(),
            ts_ms: t_s2 + 1_000,
            reason: StreamDisconnectReason::Timeout,
        })
        .await
        .unwrap();

    let expected_timeout_deadline = t_s2 + 1_000 + 900_001;
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
    assert_eq!(status_mid_timeout["monitoring_state"], "Warmup");
    assert_eq!(
        status_mid_timeout["session_close_deadline_ms"].as_i64(),
        Some(expected_timeout_deadline)
    );

    // Advance remaining 771s (total 901s > 900s) -> closes session 2 with GapExceeded at t_s2 + 1_000.
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
                Some(t_s2 + 1_000)
            );
            break;
        }
    }

    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let d1 = store.get_session(&session_1_id).unwrap();
    assert_eq!(d1.session.end_reason, SessionEndReason::GapExceeded);
    assert_eq!(d1.session.end_ts_ms, t0 + 70_000);
    let d2 = store.get_session(&session_2_id).unwrap();
    assert_eq!(d2.session.end_reason, SessionEndReason::GapExceeded);
    assert_eq!(d2.session.end_ts_ms, t_s2 + 1_000);

    daemon.shutdown().await;
}
