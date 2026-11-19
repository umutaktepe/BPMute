use std::sync::Arc;
use std::time::Duration;

use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
use bpmute::ipc::protocol::error_codes;
use bpmute::ipc::{IpcClient, IpcClientError};
use bpmute::paths::{MapEnv, ResolvedPaths};
use chrono::Utc;
use serde_json::json;
use tempfile::TempDir;

async fn spawn_test_daemon(temp: &TempDir) -> (DaemonHandle, ResolvedPaths) {
    let paths = ResolvedPaths {
        config_file: temp.path().join("cfg/bpmute/config.toml"),
        data_dir: temp.path().join("data/bpmute"),
        socket_path: temp.path().join("run/bpmute/daemon.sock"),
    };
    let env = MapEnv::new();
    let cli = CliOverrides::default();
    let loaded = LoadedConfig::load(&paths.config_file, &env, &cli).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(Utc::now()));

    let handle = DaemonHandle::spawn(DaemonOptions {
        paths: paths.clone(),
        loaded_config: loaded,
        env: Arc::new(env),
        cli_overrides: cli,
        clock,
        wearable_source: None,
        install_signals: false,
    })
    .await
    .unwrap();

    (handle, paths)
}

#[tokio::test]
async fn wearable_ipc_list_rename_and_unenroll_e2e() {
    let temp = TempDir::new().unwrap();
    let (daemon, paths) = spawn_test_daemon(&temp).await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();

    // 1. Initial wearable.list is empty
    let list_res = client.request("wearable.list", json!({})).await.unwrap();
    assert_eq!(list_res["wearables"].as_array().unwrap().len(), 0);

    // 2. Direct upsert in storage to seed a wearable
    daemon
        .core()
        .persistence
        .upsert_wearable(
            bpmute::storage::WearableDescriptor {
                mac: "AA:BB:CC:DD:EE:77".to_string(),
                address_type: "public".to_string(),
                alias: Some("My Polar".to_string()),
                advertised_name: Some("Polar H10".to_string()),
                supports_rr: true,
                supports_contact: true,
            },
            1_700_000_000_000,
        )
        .await
        .unwrap();

    // 3. wearable.list now returns the wearable
    let list_res2 = client.request("wearable.list", json!({})).await.unwrap();
    let wearables = list_res2["wearables"].as_array().unwrap();
    assert_eq!(wearables.len(), 1);
    let w_id = wearables[0]["id"].as_str().unwrap().to_string();
    assert_eq!(wearables[0]["alias"].as_str().unwrap(), "My Polar");

    // 4. wearable.rename updates alias
    let rename_res = client
        .request(
            "wearable.rename",
            json!({ "id": w_id, "alias": "Sleep Polar" }),
        )
        .await
        .unwrap();
    assert_eq!(
        rename_res["wearable"]["alias"].as_str().unwrap(),
        "Sleep Polar"
    );

    // 5. wearable.unenroll removes from allowlist
    let unenroll_res = client
        .request("wearable.unenroll", json!({ "id": w_id }))
        .await
        .unwrap();
    assert!(!unenroll_res["wearable"]["active"].as_bool().unwrap());

    let list_res3 = client.request("wearable.list", json!({})).await.unwrap();
    assert_eq!(list_res3["wearables"].as_array().unwrap().len(), 0);

    daemon.shutdown().await;
}

#[tokio::test]
async fn enrollment_single_owner_lock_and_client_disconnect_autocancel() {
    let temp = TempDir::new().unwrap();
    let (daemon, paths) = spawn_test_daemon(&temp).await;

    // Client 1 connects and acquires enrollment lock
    let mut client1 = IpcClient::connect(&paths.socket_path).await.unwrap();
    let enroll_res = client1
        .request("wearable.enroll_start", json!({}))
        .await
        .unwrap();
    assert!(enroll_res.get("devices").is_some());

    // Client 2 connects and tries to enroll -> rejected with ENROLLMENT_IN_PROGRESS
    let mut client2 = IpcClient::connect(&paths.socket_path).await.unwrap();
    let err = client2
        .request("wearable.enroll_start", json!({}))
        .await
        .expect_err("concurrent enrollment must fail");

    match err {
        IpcClientError::Server { code, .. } => {
            assert_eq!(code, error_codes::ENROLLMENT_IN_PROGRESS);
        }
        other => panic!("expected Server error with ENROLLMENT_IN_PROGRESS, got: {other:?}"),
    }

    // Client 1 disconnects
    drop(client1);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Now Client 2 can acquire the enrollment lock (auto-cancelled on client 1 disconnect)
    let enroll2_res = client2
        .request("wearable.enroll_start", json!({}))
        .await
        .unwrap();
    assert!(enroll2_res.get("devices").is_some());

    // Client 2 cancels explicitly
    let cancel_res = client2
        .request("wearable.enroll_cancel", json!({}))
        .await
        .unwrap();
    assert!(cancel_res["ok"].as_bool().unwrap());

    daemon.shutdown().await;
}

#[tokio::test]
async fn enrollment_select_rpa_warning_and_not_found_unenroll() {
    let temp = TempDir::new().unwrap();
    let (daemon, paths) = spawn_test_daemon(&temp).await;

    let mut client = IpcClient::connect(&paths.socket_path).await.unwrap();

    // Start enrollment
    client
        .request("wearable.enroll_start", json!({}))
        .await
        .unwrap();

    // Select an RPA address: 0x4A is RPA (top bits 01)
    let select_res = client
        .request(
            "wearable.enroll_select",
            json!({
                "address": "4A:11:22:33:44:55",
                "alias": "RPA Watch",
            }),
        )
        .await
        .unwrap();

    let warnings = select_res["warnings"].as_array().unwrap();
    assert!(
        !warnings.is_empty(),
        "RPA wearable must produce diagnostic warning"
    );
    assert_eq!(
        select_res["wearable"]["mac"].as_str().unwrap(),
        "4A:11:22:33:44:55"
    );

    // Unenroll non-existent wearable -> NOT_FOUND
    let err = client
        .request(
            "wearable.unenroll",
            json!({ "id": "00000000-0000-0000-0000-000000000000" }),
        )
        .await
        .expect_err("unenroll of non-existent ID must fail");

    match err {
        IpcClientError::Server { code, .. } => {
            assert_eq!(code, error_codes::NOT_FOUND);
        }
        other => panic!("expected NOT_FOUND, got: {other:?}"),
    }

    daemon.shutdown().await;
}
