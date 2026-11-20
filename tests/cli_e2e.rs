use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bpmute::cli::{Cli, execute_cli};
use bpmute::config::{CliOverrides, LoadedConfig};
use bpmute::daemon::{DaemonHandle, DaemonOptions, VirtualClock};
use bpmute::exit_code::ExitCode;
use bpmute::ipc::IpcClient;
use bpmute::paths::{EnvSource, MapEnv, ProcessEnv, ResolvedPaths};
use chrono::{Local, TimeZone, Timelike, Utc};
use clap::Parser;
use serde_json::Value;
use tempfile::TempDir;

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("child present").id()
    }

    fn into_inner(mut self) -> Child {
        self.child.take().expect("child present")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(ref mut child) = self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn bin_path() -> &'static str {
    env!("CARGO_BIN_EXE_bpmute")
}

fn base_cli_cmd(temp: &TempDir) -> Command {
    let mut cmd = Command::new(bin_path());
    cmd.env_clear()
        .env("PATH", ProcessEnv.get("PATH").unwrap_or_default())
        .env("HOME", temp.path().join("home"))
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .env("XDG_DATA_HOME", temp.path().join("xdg-data"))
        .env("XDG_RUNTIME_DIR", temp.path().join("xdg-runtime"));
    cmd
}

async fn run_inproc_cli(args: &[&str], env: &MapEnv) -> (ExitCode, String, String) {
    let cli = Cli::try_parse_from(args).expect("CLI args should parse");
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = execute_cli(cli, env, &mut stdout, &mut stderr).await;
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

fn wait_for_socket(socket_path: &Path) {
    for _ in 0..100 {
        if socket_path.try_exists().unwrap_or(false)
            && std::os::unix::net::UnixStream::connect(socket_path).is_ok()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "daemon socket {} did not become ready in time",
        socket_path.display()
    );
}

#[tokio::test(start_paused = true)]
async fn virtual_time_cli_pause_resume_and_config_reload_e2e() {
    let temp = TempDir::new().unwrap();
    let env = MapEnv::new()
        .with(
            "HOME",
            temp.path().join("home").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_CONFIG_HOME",
            temp.path().join("cfg").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_DATA_HOME",
            temp.path().join("data").to_string_lossy().into_owned(),
        )
        .with(
            "XDG_RUNTIME_DIR",
            temp.path().join("run").to_string_lossy().into_owned(),
        );
    let paths = ResolvedPaths::resolve(None, None, None, &env).unwrap();
    let loaded = LoadedConfig::load(&paths.config_file, &env, &CliOverrides::default()).unwrap();

    let start_local = Local.with_ymd_and_hms(2026, 11, 19, 23, 0, 0).unwrap();
    let clock = Arc::new(VirtualClock::starting_at(start_local.with_timezone(&Utc)));
    let daemon = DaemonHandle::spawn(DaemonOptions::for_testing(
        paths.clone(),
        loaded,
        Arc::new(env.clone()),
        clock,
    ))
    .await
    .unwrap();

    // Status online via CLI.
    let (code, out, err) = run_inproc_cli(&["bpmute", "status", "--json"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let status: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(status["v"], 1);
    assert_eq!(status["daemon_running"], true);

    // Pause until-morning defaults to 08:00 local when active_hours is disabled.
    let (code, _, err) = run_inproc_cli(&["bpmute", "pause", "until-morning"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let (_, out, _) = run_inproc_cli(&["bpmute", "status", "--json"], &env).await;
    let st: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(st["paused"], true);
    assert_eq!(st["pause_mode"], "until_morning");
    let until_08: chrono::DateTime<Local> =
        chrono::DateTime::parse_from_rfc3339(st["paused_until"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Local);
    assert_eq!((until_08.hour(), until_08.minute()), (8, 0));

    // Advance virtual time 9 hours (23:00 -> 08:00) -> auto-resumed!
    tokio::time::advance(Duration::from_secs(9 * 3600 + 1)).await;
    tokio::task::yield_now().await;
    let (_, out, _) = run_inproc_cli(&["bpmute", "status", "--json"], &env).await;
    let st: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(st["paused"], false);
    assert_eq!(st["armed"], true);

    // Init config, enable active_hours with end = "06:15", reload via CLI, pause until-morning -> 06:15.
    let (code, _, _) = run_inproc_cli(&["bpmute", "config", "init"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let mut cfg_text = fs::read_to_string(&paths.config_file).unwrap();
    cfg_text = cfg_text.replace(
        "active_hours_enabled = false",
        "active_hours_enabled = true",
    );
    cfg_text = cfg_text.replace("end = \"08:00\"", "end = \"06:15\"");
    fs::write(&paths.config_file, cfg_text).unwrap();

    let (code, _, err) = run_inproc_cli(&["bpmute", "config", "reload"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, _, _) = run_inproc_cli(&["bpmute", "pause", "until-morning"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let (_, out, _) = run_inproc_cli(&["bpmute", "status", "--json"], &env).await;
    let st: Value = serde_json::from_str(out.trim()).unwrap();
    let until_0615: chrono::DateTime<Local> =
        chrono::DateTime::parse_from_rfc3339(st["paused_until"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Local);
    assert_eq!((until_0615.hour(), until_0615.minute()), (6, 15));

    // Pause 20m, advance 20m -> resumes.
    let (code, _, _) = run_inproc_cli(&["bpmute", "pause", "20m"], &env).await;
    assert_eq!(code, ExitCode::Success);
    tokio::time::advance(Duration::from_secs(20 * 60 + 1)).await;
    tokio::task::yield_now().await;
    let (_, out, _) = run_inproc_cli(&["bpmute", "status", "--json"], &env).await;
    let st: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(st["paused"], false);

    // Bare pause = indefinite, then manual resume.
    let (code, _, _) = run_inproc_cli(&["bpmute", "pause"], &env).await;
    assert_eq!(code, ExitCode::Success);
    let (_, out, _) = run_inproc_cli(&["bpmute", "status", "--json"], &env).await;
    let st: Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(st["paused"], true);
    assert_eq!(st["pause_mode"], "indefinite");
    assert!(st["paused_until"].is_null());

    let (code, _, _) = run_inproc_cli(&["bpmute", "resume"], &env).await;
    assert_eq!(code, ExitCode::Success);

    // Invalid pause durations fail with exit 2.
    let (code, _, _) = run_inproc_cli(&["bpmute", "pause", "0s"], &env).await;
    assert_eq!(code, ExitCode::UsageOrConfigError);
    let (code, _, _) = run_inproc_cli(&["bpmute", "pause", "not-a-duration"], &env).await;
    assert_eq!(code, ExitCode::UsageOrConfigError);

    daemon.shutdown().await;
}

#[test]
fn real_binary_offline_and_startup_validation_exit_codes() {
    let temp = TempDir::new().unwrap();

    // --help and --version exit 0.
    assert_eq!(
        base_cli_cmd(&temp)
            .arg("--help")
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        base_cli_cmd(&temp)
            .arg("--version")
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );

    // Unknown flag exits 2.
    assert_eq!(
        base_cli_cmd(&temp)
            .arg("--no-such-flag")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );

    // Offline status --json emits exact {"v":1,"daemon_running":false} and exits 3.
    let offline = base_cli_cmd(&temp)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_eq!(offline.status.code(), Some(3));
    let offline_json: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(
        offline_json,
        serde_json::json!({ "v": 1, "daemon_running": false })
    );

    // Invalid startup config.toml fails fast with exit 2.
    let bad_cfg = temp.path().join("bad-config.toml");
    fs::write(&bad_cfg, "schema_version = 1\nunknown_field = 123\n").unwrap();
    let bad_daemon = base_cli_cmd(&temp)
        .args(["--config", bad_cfg.to_str().unwrap(), "daemon"])
        .output()
        .unwrap();
    assert_eq!(bad_daemon.status.code(), Some(2));

    // Invalid BPMUTE_LOG filter fails fast with exit 2.
    let bad_log = base_cli_cmd(&temp)
        .env("BPMUTE_LOG", "invalid[directive")
        .arg("daemon")
        .output()
        .unwrap();
    assert_eq!(bad_log.status.code(), Some(2));
}

#[tokio::test]
async fn real_daemon_cli_config_edit_and_sighup_live_reload_without_dropping_subscribers() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("xdg-runtime/bpmute/daemon.sock");
    let config_path = temp.path().join("xdg-config/bpmute/config.toml");

    let child = base_cli_cmd(&temp)
        .args(["daemon", "--dry-run"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bpmute daemon");
    let daemon_guard = ChildGuard::new(child);
    let daemon_pid = daemon_guard.pid();
    wait_for_socket(&socket_path);

    // Config file must NOT exist on startup.
    assert!(!config_path.try_exists().unwrap());

    // `bpmute status --json` online check.
    let status_out = base_cli_cmd(&temp)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert_eq!(status_out.status.code(), Some(0));
    let status_json: Value = serde_json::from_slice(&status_out.stdout).unwrap();
    assert_eq!(status_json["daemon_running"], true);
    assert_eq!(status_json["dry_run"], true);

    // `bpmute config path`, `init`, duplicate `init` -> 4, `init --force` -> 0, `validate`, `show --json`.
    let path_out = base_cli_cmd(&temp)
        .args(["config", "path"])
        .output()
        .unwrap();
    assert_eq!(path_out.status.code(), Some(0));
    assert_eq!(
        PathBuf::from(String::from_utf8_lossy(&path_out.stdout).trim()),
        config_path
    );

    assert_eq!(
        base_cli_cmd(&temp)
            .args(["config", "init"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        base_cli_cmd(&temp)
            .args(["config", "init"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(4)
    );
    assert_eq!(
        base_cli_cmd(&temp)
            .args(["config", "init", "--force"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        base_cli_cmd(&temp)
            .args(["config", "validate"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );

    let show_out = base_cli_cmd(&temp)
        .args(["config", "show", "--json"])
        .output()
        .unwrap();
    assert_eq!(show_out.status.code(), Some(0));
    let show_json: Value = serde_json::from_slice(&show_out.stdout).unwrap();
    assert_eq!(show_json["config"]["schema_version"], 1);

    // `bpmute config edit` with mock EDITOR updating sensitivity to conservative.
    let ok_editor = temp.path().join("ok-editor.sh");
    fs::write(
        &ok_editor,
        "#!/bin/sh\nsed -i 's/sensitivity = \"balanced\"/sensitivity = \"conservative\"/' \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&ok_editor, fs::Permissions::from_mode(0o755)).unwrap();

    let edit_ok = base_cli_cmd(&temp)
        .env("EDITOR", &ok_editor)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_eq!(edit_ok.status.code(), Some(0));

    // Subscribe over real Unix socket BEFORE sending SIGHUP to verify connection persists.
    let mut sub_client = IpcClient::connect(&socket_path).await.unwrap();
    sub_client
        .request("daemon.subscribe", serde_json::json!({}))
        .await
        .unwrap();
    let snap = sub_client.next_event().await.unwrap();
    assert_eq!(snap.event, "Snapshot");

    // Send `bpmute config reload` -> picks up conservative + emits ConfigUpdated.
    assert_eq!(
        base_cli_cmd(&temp)
            .args(["config", "reload"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(0)
    );
    let cfg_ev = sub_client.next_event().await.unwrap();
    assert_eq!(cfg_ev.event, "ConfigUpdated");
    assert_eq!(
        cfg_ev.data["config"]["detection"]["sensitivity"],
        "conservative"
    );

    // Corrupt config via mock EDITOR -> `config edit` exits 2.
    let bad_editor = temp.path().join("bad-editor.sh");
    fs::write(
        &bad_editor,
        "#!/bin/sh\nprintf 'schema_version = 1\\nbad_key = true\\n' > \"$1\"\n",
    )
    .unwrap();
    fs::set_permissions(&bad_editor, fs::Permissions::from_mode(0o755)).unwrap();

    let edit_bad = base_cli_cmd(&temp)
        .env("EDITOR", &bad_editor)
        .args(["config", "edit"])
        .output()
        .unwrap();
    assert_eq!(edit_bad.status.code(), Some(2));

    // Send real SIGHUP with broken config on disk -> preserves conservative, emits AlertChanged,
    // and keeps subscriber alive!
    let kill_status = Command::new("kill")
        .args(["-HUP", &daemon_pid.to_string()])
        .status()
        .unwrap();
    assert!(kill_status.success());

    let alert_ev = sub_client.next_event().await.unwrap();
    assert_eq!(alert_ev.event, "AlertChanged");
    assert!(!alert_ev.data["alerts"].as_array().unwrap().is_empty());

    // Restore valid config with "responsive" and send real SIGHUP -> reloads live on same connection!
    fs::write(
        &config_path,
        "schema_version = 1\n[detection]\nsensitivity = \"responsive\"\n",
    )
    .unwrap();
    let kill_status2 = Command::new("kill")
        .args(["-HUP", &daemon_pid.to_string()])
        .status()
        .unwrap();
    assert!(kill_status2.success());

    let ev_a = sub_client.next_event().await.unwrap();
    let ev_b = sub_client.next_event().await.unwrap();
    let names = [&ev_a.event, &ev_b.event];
    assert!(names.contains(&&"AlertChanged".to_string()));
    assert!(names.contains(&&"ConfigUpdated".to_string()));
    let cfg_payload = if ev_a.event == "ConfigUpdated" {
        ev_a
    } else {
        ev_b
    };
    assert_eq!(
        cfg_payload.data["config"]["detection"]["sensitivity"],
        "responsive"
    );

    // Graceful SIGTERM cleans up socket.
    drop(sub_client);
    let _ = Command::new("kill")
        .args(["-TERM", &daemon_pid.to_string()])
        .status();
    let mut child = daemon_guard.into_inner();
    let status = child.wait().unwrap();
    assert!(status.success() || status.code() == Some(0));
    assert!(!socket_path.try_exists().unwrap());
}

#[test]
fn log_biometrics_flag_emits_prominent_startup_warn_banner() {
    let temp = TempDir::new().unwrap();
    let socket_path = temp.path().join("xdg-runtime/bpmute/daemon.sock");

    let child = base_cli_cmd(&temp)
        .args(["daemon", "--log-biometrics"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let guard = ChildGuard::new(child);

    wait_for_socket(&socket_path);
    let _ = Command::new("kill")
        .args(["-TERM", &guard.pid().to_string()])
        .status();
    let output = guard.into_inner().wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("WARN") && stderr.contains("BIOMETRIC LOGGING ENABLED"),
        "expected prominent startup WARN banner in stderr, got:\n{stderr}"
    );
}
