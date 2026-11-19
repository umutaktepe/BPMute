use std::sync::Arc;
use std::time::Duration;

use bpmute::bluetooth::packet::parse_hr_measurement;
use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{
    DaemonHandle, DaemonOptions, VirtualClock, WearableDescriptor, WearableEvent, WearableSource,
};
use bpmute::ipc::IpcClient;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::sleep_engine::{SessionEndReason, StreamState, StreamStatus};
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

#[tokio::test]
async fn virtual_time_wearable_source_stream_monitoring_and_session_recording() {
    let temp = TempDir::new().unwrap();
    let paths = ResolvedPaths {
        config_file: temp.path().join("cfg/bpmute/config.toml"),
        data_dir: temp.path().join("data/bpmute"),
        socket_path: temp.path().join("run/bpmute/daemon.sock"),
    };
    let env = MapEnv::new();
    let cli = CliOverrides::default();
    let loaded = LoadedConfig::load(&paths.config_file, &env, &cli).unwrap();

    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_utc));
    let t0 = start_utc.timestamp_millis();

    let (feed_tx, feed_rx) = mpsc::channel(64);
    let wearable_source = Arc::new(ChannelWearableSource::new(feed_rx));

    let daemon = DaemonHandle::spawn(DaemonOptions {
        paths: paths.clone(),
        loaded_config: loaded,
        env: Arc::new(env),
        cli_overrides: cli,
        clock: clock.clone(),
        wearable_source: Some(wearable_source),
        install_signals: false,
        action_executor: None,
        notifier: None,
    })
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

    // 1. Initial status -> running
    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    let status_res = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(status_res["daemon_running"], true);

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

    // Give daemon monitoring loop a moment to ingest packets
    tokio::time::sleep(Duration::from_millis(50)).await;

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

    // 5. Feed StreamState Disconnected to end session
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

    tokio::time::sleep(Duration::from_millis(50)).await;

    // 6. Verify session stored in SQLite ledger directly with end_reason StreamEnded
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
    let detail = store.get_session(&session_id).unwrap();
    assert_eq!(detail.session.end_reason, SessionEndReason::StreamEnded);
    assert_eq!(detail.wearable.alias.as_deref(), Some("Test Strap"));

    daemon.shutdown().await;
}
