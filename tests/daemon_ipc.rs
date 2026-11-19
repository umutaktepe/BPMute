use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
use bpmute::ipc::IpcClient;
use bpmute::ipc::protocol::{MAX_FRAME_BYTES, PROTOCOL_VERSION};
use bpmute::paths::{MapEnv, ResolvedPaths};
use chrono::{Local, TimeZone, Timelike, Utc};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn test_paths(temp: &TempDir) -> ResolvedPaths {
    ResolvedPaths {
        config_file: temp.path().join("cfg/bpmute/config.toml"),
        data_dir: temp.path().join("data/bpmute"),
        socket_path: temp.path().join("run/bpmute/daemon.sock"),
    }
}

async fn start_virtual_daemon(
    temp: &TempDir,
    start_utc: chrono::DateTime<Utc>,
) -> (DaemonHandle, ResolvedPaths) {
    let paths = test_paths(temp);
    let env = MapEnv::new();
    let cli = CliOverrides::default();
    let loaded = LoadedConfig::load(&paths.config_file, &env, &cli).unwrap();
    let clock = std::sync::Arc::new(VirtualClock::starting_at(start_utc));

    let handle = DaemonHandle::spawn(DaemonOptions {
        paths: paths.clone(),
        loaded_config: loaded,
        env: std::sync::Arc::new(env),
        cli_overrides: cli,
        clock,
        install_signals: false,
    })
    .await
    .expect("daemon should start");

    (handle, paths)
}

#[tokio::test(start_paused = true)]
async fn daemon_socket_permissions_ping_status_and_framing_guards() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let (daemon, paths) = start_virtual_daemon(&temp, start_utc).await;

    let dir_mode = fs::metadata(paths.socket_path.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    let sock_mode = fs::metadata(&paths.socket_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(sock_mode, 0o600);
    assert!(
        !paths.config_file.exists(),
        "startup must not create config.toml"
    );

    // Stale/duplicate daemon guard: spawning on the same live socket must fail.
    let loaded =
        LoadedConfig::load(&paths.config_file, &MapEnv::new(), &CliOverrides::default()).unwrap();
    let dup = DaemonHandle::spawn(DaemonOptions {
        paths: paths.clone(),
        loaded_config: loaded,
        env: std::sync::Arc::new(MapEnv::new()),
        cli_overrides: CliOverrides::default(),
        clock: std::sync::Arc::new(VirtualClock::starting_at(start_utc)),
        install_signals: false,
    })
    .await;
    assert!(dup.is_err(), "cannot steal live daemon socket");

    // Raw NDJSON connection tests: UNSUPPORTED_VERSION, UNKNOWN_OP, INVALID_JSON, FRAME_TOO_LARGE.
    let stream = UnixStream::connect(&paths.socket_path).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Unsupported version -> UNSUPPORTED_VERSION and connection remains open.
    write_half
        .write_all(b"{\"v\":2,\"id\":7,\"op\":\"daemon.ping\",\"params\":{}}\n")
        .await
        .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["v"], PROTOCOL_VERSION);
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "UNSUPPORTED_VERSION");

    // Malformed JSON -> INVALID_JSON and connection remains open.
    line.clear();
    write_half.write_all(b"{not-json\n").await.unwrap();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "INVALID_JSON");

    // Unknown op -> UNKNOWN_OP.
    line.clear();
    write_half
        .write_all(b"{\"v\":1,\"id\":9,\"op\":\"no.such.op\",\"params\":{}}\n")
        .await
        .unwrap();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], 9);
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "UNKNOWN_OP");

    // Valid ping still works on the same connection.
    line.clear();
    write_half
        .write_all(b"{\"v\":1,\"id\":10,\"op\":\"daemon.ping\",\"params\":{}}\n")
        .await
        .unwrap();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["id"], 10);
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["data"]["pong"], true);

    // Oversized line (> 1 MiB) -> FRAME_TOO_LARGE + connection close.
    let oversized = format!(
        "{{\"v\":1,\"id\":11,\"op\":\"daemon.ping\",\"params\":{{\"pad\":\"{}\"}}}}\n",
        "x".repeat(MAX_FRAME_BYTES + 64)
    );
    write_half.write_all(oversized.as_bytes()).await.unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "FRAME_TOO_LARGE");

    line.clear();
    let eof = reader.read_line(&mut line).await.unwrap();
    assert_eq!(eof, 0, "oversized frame should close connection");

    daemon.shutdown().await;
    assert!(!paths.socket_path.exists(), "socket should be cleaned up");
}

#[tokio::test(start_paused = true)]
async fn subscribe_snapshot_pause_until_morning_and_virtual_time_auto_resume() {
    let temp = TempDir::new().unwrap();
    let local_start = Local.with_ymd_and_hms(2026, 11, 19, 23, 30, 0).unwrap();
    let (daemon, paths) = start_virtual_daemon(&temp, local_start.with_timezone(&Utc)).await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();

    // Subscribe and expect immediate Snapshot push event.
    let sub_resp = client.request("daemon.subscribe", json!({})).await.unwrap();
    assert_eq!(sub_resp["subscribed"], true);

    let snapshot = client.next_event().await.unwrap();
    assert_eq!(snapshot.v, 1);
    assert_eq!(snapshot.event, "Snapshot");
    assert_eq!(snapshot.data["status"]["v"], 1);
    assert_eq!(snapshot.data["status"]["daemon_running"], true);
    assert_eq!(snapshot.data["status"]["armed"], true);
    assert_eq!(snapshot.data["status"]["paused"], false);

    // Pause for 15 minutes and verify virtual-time auto-resume emits StateChanged.
    let pause_resp = client
        .request(
            "monitor.pause",
            json!({ "mode": "duration", "duration_sec": 900 }),
        )
        .await
        .unwrap();
    assert_eq!(pause_resp["paused"], true);
    assert_eq!(pause_resp["armed"], false);

    let paused_ev = client.next_event().await.unwrap();
    assert_eq!(paused_ev.event, "StateChanged");
    assert_eq!(paused_ev.data["paused"], true);
    assert_eq!(paused_ev.data["armed"], false);

    // Advance virtual time 14m59s -> still paused.
    tokio::time::advance(Duration::from_secs(899)).await;
    tokio::task::yield_now().await;
    let status_mid = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(status_mid["paused"], true);

    // Advance past 15m -> auto-resumes!
    tokio::time::advance(Duration::from_secs(2)).await;
    let resumed_ev = client.next_event().await.unwrap();
    assert_eq!(resumed_ev.event, "StateChanged");
    assert_eq!(resumed_ev.data["paused"], false);
    assert_eq!(resumed_ev.data["armed"], true);

    // Pause until-morning without active-hours enabled anchors to 08:00 local.
    let morning_resp = client
        .request("monitor.pause", json!({ "mode": "until_morning" }))
        .await
        .unwrap();
    assert_eq!(morning_resp["paused"], true);
    let paused_until = morning_resp["paused_until"].as_str().unwrap();
    let parsed_until: chrono::DateTime<Local> = chrono::DateTime::parse_from_rfc3339(paused_until)
        .unwrap()
        .with_timezone(&Local);
    assert_eq!(parsed_until.hour(), 8);
    assert_eq!(parsed_until.minute(), 0);
    let _ = client.next_event().await.unwrap(); // StateChanged

    // Enable active_hours with end = "06:30" via config.update, then pause until-morning -> 06:30.
    client
        .request(
            "config.update",
            json!({
                "schedule": {
                    "active_hours_enabled": true,
                    "start": "22:00",
                    "end": "06:30"
                }
            }),
        )
        .await
        .unwrap();
    let cfg_ev = client.next_event().await.unwrap();
    assert_eq!(cfg_ev.event, "ConfigUpdated");

    let custom_morning = client
        .request("monitor.pause", json!({ "mode": "until_morning" }))
        .await
        .unwrap();
    let custom_until: chrono::DateTime<Local> =
        chrono::DateTime::parse_from_rfc3339(custom_morning["paused_until"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Local);
    assert_eq!(custom_until.hour(), 6);
    assert_eq!(custom_until.minute(), 30);

    // Drain StateChanged, then manual resume & dry_run toggle.
    let _ = client.next_event().await.unwrap();
    client.request("monitor.resume", json!({})).await.unwrap();
    let res_ev = client.next_event().await.unwrap();
    assert_eq!(res_ev.event, "StateChanged");
    assert_eq!(res_ev.data["paused"], false);

    client
        .request("monitor.set_dry_run", json!({ "dry_run": true }))
        .await
        .unwrap();
    let dry_ev = client.next_event().await.unwrap();
    assert_eq!(dry_ev.event, "StateChanged");
    assert_eq!(dry_ev.data["dry_run"], true);

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn config_reload_preserves_last_valid_config_and_keeps_connections_alive() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap();
    let (daemon, paths) = start_virtual_daemon(&temp, start_utc).await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let _snapshot = client.next_event().await.unwrap();

    // Patch config via config.update (lazy creation).
    client
        .request(
            "config.update",
            json!({ "detection": { "sensitivity": "conservative" } }),
        )
        .await
        .unwrap();
    let ev1 = client.next_event().await.unwrap();
    assert_eq!(ev1.event, "ConfigUpdated");
    assert_eq!(
        ev1.data["config"]["detection"]["sensitivity"],
        "conservative"
    );

    // Corrupt config.toml on disk, then trigger config.reload -> error, preserves conservative, emits AlertChanged.
    fs::write(&paths.config_file, "schema_version = 999\nbad_key = true\n").unwrap();
    let reload_err = client
        .request("config.reload", json!({}))
        .await
        .expect_err("invalid config reload must fail");
    assert_eq!(reload_err.code(), Some("INVALID_CONFIG"));

    let alert_ev = client.next_event().await.unwrap();
    assert_eq!(alert_ev.event, "AlertChanged");
    assert!(
        !alert_ev.data["alerts"].as_array().unwrap().is_empty(),
        "reload failure alert should be active"
    );

    // Last valid config must still be active on the same connection.
    let current = client.request("config.get", json!({})).await.unwrap();
    assert_eq!(
        current["config"]["detection"]["sensitivity"],
        "conservative"
    );

    // Fix config.toml on disk and reload -> clears alert, updates config, connection still intact.
    fs::write(
        &paths.config_file,
        "schema_version = 1\n[detection]\nsensitivity = \"responsive\"\n",
    )
    .unwrap();
    let ok_reload = client.request("config.reload", json!({})).await.unwrap();
    assert_eq!(
        ok_reload["config"]["detection"]["sensitivity"],
        "responsive"
    );

    let ev_alert_cleared = client.next_event().await.unwrap();
    let ev_cfg = client.next_event().await.unwrap();
    let events = [&ev_alert_cleared.event, &ev_cfg.event];
    assert!(events.contains(&&"AlertChanged".to_string()));
    assert!(events.contains(&&"ConfigUpdated".to_string()));

    let status = client.request("daemon.status", json!({})).await.unwrap();
    assert!(status["alerts"].as_array().unwrap().is_empty());

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn drop_oldest_backpressure_drops_stale_hr_samples_without_dropping_control_events() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap();
    let (daemon, paths) = start_virtual_daemon(&temp, start_utc).await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client.request("daemon.subscribe", json!({})).await.unwrap();
    let snapshot = client.next_event().await.unwrap();
    assert_eq!(snapshot.event, "Snapshot");

    // Flood 100 HrSample events synchronously before client drains, exceeding the 64-slot stream queue,
    // plus a control event (`StateChanged` via `monitor.set_dry_run` or direct control push).
    for i in 0..100_u64 {
        daemon.publish_hr_sample(json!({
            "tick": i,
            "bpm": 60 + (i % 5)
        }));
    }
    client
        .request("monitor.set_dry_run", json!({ "dry_run": true }))
        .await
        .unwrap();

    let mut hr_ticks = Vec::new();
    let mut saw_state_changed = false;
    for _ in 0..65 {
        let ev = client.next_event().await.unwrap();
        match ev.event.as_str() {
            "HrSample" => hr_ticks.push(ev.data["tick"].as_u64().unwrap()),
            "StateChanged" => saw_state_changed = true,
            other => panic!("unexpected event {other}"),
        }
    }

    assert!(saw_state_changed, "control event must never be dropped");
    assert_eq!(
        hr_ticks.len(),
        64,
        "bounded queue should retain newest 64 samples"
    );
    assert_eq!(hr_ticks.first().copied(), Some(36));
    assert_eq!(hr_ticks.last().copied(), Some(99));

    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn runtime_dry_run_survives_unrelated_config_updates_and_reloads() {
    let temp = TempDir::new().unwrap();
    let start_utc = Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap();
    let (daemon, paths) = start_virtual_daemon(&temp, start_utc).await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();
    client
        .request("monitor.set_dry_run", json!({ "dry_run": true }))
        .await
        .unwrap();

    // Unrelated config.update (sensitivity) must NOT clobber runtime dry_run.
    client
        .request(
            "config.update",
            json!({ "detection": { "sensitivity": "conservative" } }),
        )
        .await
        .unwrap();
    let st1 = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(st1["dry_run"], true);

    // Unrelated config.reload must also preserve runtime dry_run.
    client.request("config.reload", json!({})).await.unwrap();
    let st2 = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(st2["dry_run"], true);

    // Explicit config.update touching actions.dry_run updates runtime dry_run.
    client
        .request("config.update", json!({ "actions": { "dry_run": false } }))
        .await
        .unwrap();
    let st3 = client.request("daemon.status", json!({})).await.unwrap();
    assert_eq!(st3["dry_run"], false);

    daemon.shutdown().await;
}
