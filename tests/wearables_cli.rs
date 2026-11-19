use std::io::Cursor;
use std::sync::Arc;

use bpmute::cli::args::{Cli, Command, WearablesCommand};
use bpmute::cli::execute_cli;
use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
use bpmute::exit_code::ExitCode;
use bpmute::paths::{MapEnv, ResolvedPaths};
use bpmute::storage::{
    LedgerStore, WearableDescriptor, db_path_in_data_dir, lock_path_in_data_dir,
};
use chrono::Utc;
use serde_json::Value;
use tempfile::TempDir;

fn make_test_paths(temp: &TempDir) -> (ResolvedPaths, MapEnv) {
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

#[tokio::test]
async fn wearables_cli_offline_flock_parity_list_rename_unenroll() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);

    // Seed wearable directly in SQLite while daemon is offline
    let db_path = db_path_in_data_dir(&paths.data_dir);
    let lock_path = lock_path_in_data_dir(&paths.data_dir);
    let w_id = {
        let mut store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
        store
            .upsert_wearable(
                &WearableDescriptor {
                    mac: "AA:BB:CC:DD:EE:01".to_string(),
                    address_type: "public".to_string(),
                    alias: Some("Offline Strap".to_string()),
                    advertised_name: Some("Polar H10".to_string()),
                    supports_rr: true,
                    supports_contact: true,
                },
                1_700_000_000_000,
            )
            .unwrap()
    };

    // 1. `bpmute wearables list --json` offline
    let cli = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::List { json: true },
        },
    };
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let code = execute_cli(cli, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::Success);

    let output_str = String::from_utf8(stdout.into_inner()).unwrap();
    let val: Value = serde_json::from_str(&output_str).expect("should parse JSON");
    let wearables = val["wearables"].as_array().unwrap();
    assert_eq!(wearables.len(), 1);
    assert_eq!(wearables[0]["id"].as_str().unwrap(), w_id);
    assert_eq!(wearables[0]["alias"].as_str().unwrap(), "Offline Strap");

    // 2. `bpmute wearables rename <prefix> "Renamed Strap"` offline
    let prefix = &w_id[..8];
    let cli_rename = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::Rename {
                id: prefix.to_string(),
                alias: "Renamed Strap".to_string(),
            },
        },
    };
    let mut stdout2 = Cursor::new(Vec::new());
    let mut stderr2 = Cursor::new(Vec::new());
    let code2 = execute_cli(cli_rename, &env, &mut stdout2, &mut stderr2).await;
    assert_eq!(code2, ExitCode::Success);

    // Verify in store
    {
        let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
        let w = store.get_wearable(prefix).unwrap();
        assert_eq!(w.alias.as_deref(), Some("Renamed Strap"));
    }

    // 3. `bpmute wearables connect <id>` offline -> ExitCode::DaemonOffline (3)
    let cli_connect = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::Connect {
                id: prefix.to_string(),
            },
        },
    };
    let mut stdout3 = Cursor::new(Vec::new());
    let mut stderr3 = Cursor::new(Vec::new());
    let code3 = execute_cli(cli_connect, &env, &mut stdout3, &mut stderr3).await;
    assert_eq!(code3, ExitCode::DaemonOffline);

    // 4. `bpmute wearables unenroll <id>` offline
    let cli_unenroll = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::Unenroll {
                id: prefix.to_string(),
                forget_bluez: false,
                delete_sessions: false,
            },
        },
    };
    let mut stdout4 = Cursor::new(Vec::new());
    let mut stderr4 = Cursor::new(Vec::new());
    let code4 = execute_cli(cli_unenroll, &env, &mut stdout4, &mut stderr4).await;
    assert_eq!(code4, ExitCode::Success);

    // Allowlist must now be empty
    {
        let store = LedgerStore::open_read_write(&db_path, &lock_path).unwrap();
        assert!(store.list_enrolled_wearables().unwrap().is_empty());
    }
}

#[tokio::test]
async fn wearables_cli_online_end_to_end() {
    let temp = TempDir::new().unwrap();
    let (paths, env) = make_test_paths(&temp);

    // Spawn test daemon
    let cli_overrides = CliOverrides::default();
    let loaded = LoadedConfig::load(&paths.config_file, &env, &cli_overrides).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(Utc::now()));

    let daemon = DaemonHandle::spawn(DaemonOptions {
        paths: paths.clone(),
        loaded_config: loaded,
        env: Arc::new(env.clone()),
        cli_overrides,
        clock,
        wearable_source: None,
        install_signals: false,
    })
    .await
    .unwrap();

    // 1. Initial list -- empty human readable
    let cli_list = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::List { json: false },
        },
    };
    let mut stdout = Cursor::new(Vec::new());
    let mut stderr = Cursor::new(Vec::new());
    let code = execute_cli(cli_list, &env, &mut stdout, &mut stderr).await;
    assert_eq!(code, ExitCode::Success);
    let out = String::from_utf8(stdout.into_inner()).unwrap();
    assert!(out.contains("No enrolled Wearables found"));

    // 2. Enroll via CLI
    let cli_enroll = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Enroll {
            address: Some("C0:00:11:22:33:44".to_string()),
            alias: Some("Live Strap".to_string()),
        },
    };
    let mut stdout2 = Cursor::new(Vec::new());
    let mut stderr2 = Cursor::new(Vec::new());
    let code2 = execute_cli(cli_enroll, &env, &mut stdout2, &mut stderr2).await;
    assert_eq!(code2, ExitCode::Success);
    let out2 = String::from_utf8(stdout2.into_inner()).unwrap();
    assert!(out2.contains("Successfully enrolled wearable"));

    // 3. List --json -> 1 wearable
    let cli_list_json = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::List { json: true },
        },
    };
    let mut stdout3 = Cursor::new(Vec::new());
    let mut stderr3 = Cursor::new(Vec::new());
    let code3 = execute_cli(cli_list_json, &env, &mut stdout3, &mut stderr3).await;
    assert_eq!(code3, ExitCode::Success);
    let out3 = String::from_utf8(stdout3.into_inner()).unwrap();
    let val: Value = serde_json::from_str(&out3).unwrap();
    let arr = val["wearables"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let w_id = arr[0]["id"].as_str().unwrap().to_string();
    assert_eq!(arr[0]["alias"].as_str().unwrap(), "Live Strap");

    // 4. Connect
    let cli_connect = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::Connect { id: w_id.clone() },
        },
    };
    let mut stdout4 = Cursor::new(Vec::new());
    let mut stderr4 = Cursor::new(Vec::new());
    let code4 = execute_cli(cli_connect, &env, &mut stdout4, &mut stderr4).await;
    assert_eq!(code4, ExitCode::Success);

    // 5. Rename
    let cli_rename = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::Rename {
                id: w_id.clone(),
                alias: "Updated Live Strap".to_string(),
            },
        },
    };
    let mut stdout5 = Cursor::new(Vec::new());
    let mut stderr5 = Cursor::new(Vec::new());
    let code5 = execute_cli(cli_rename, &env, &mut stdout5, &mut stderr5).await;
    assert_eq!(code5, ExitCode::Success);

    // 6. Unenroll
    let cli_unenroll = Cli {
        config: Some(paths.config_file.clone()),
        data_dir: Some(paths.data_dir.clone()),
        socket: Some(paths.socket_path.clone()),
        log_level: None,
        log_format: None,
        log_biometrics: false,
        command: Command::Wearables {
            action: WearablesCommand::Unenroll {
                id: w_id.clone(),
                forget_bluez: false,
                delete_sessions: false,
            },
        },
    };
    let mut stdout6 = Cursor::new(Vec::new());
    let mut stderr6 = Cursor::new(Vec::new());
    let code6 = execute_cli(cli_unenroll, &env, &mut stdout6, &mut stderr6).await;
    assert_eq!(code6, ExitCode::Success);

    // Shutdown daemon
    daemon.shutdown().await;
}
