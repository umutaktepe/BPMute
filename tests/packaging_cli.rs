use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bpmute::cli::commands::service::{
    ResolvedServiceTarget, render_systemd_unit, resolve_service_unit_path, run_service_with_runner,
};
use bpmute::cli::commands::system_exec::{CommandOutput, SystemRunner};
use bpmute::cli::commands::update::{find_updater_binary, run_update_with_runner};
use bpmute::cli::{Cli, ServiceCommand, ServiceTargetArg, execute_cli};
use bpmute::exit_code::ExitCode;
use bpmute::paths::{EnvSource, MapEnv, ResolvedPaths};
use clap::Parser;
use tempfile::TempDir;

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

fn make_test_env(temp: &TempDir, bin_dir: Option<&Path>) -> (MapEnv, ResolvedPaths) {
    let mut env = MapEnv::new()
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
    if let Some(bin) = bin_dir {
        env = env.with("PATH", bin.to_string_lossy().into_owned());
    }
    let paths = ResolvedPaths::resolve(None, None, None, &env).unwrap();
    (env, paths)
}

fn write_executable_script(dir: &Path, name: &str, body: &str) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

type RecordedCalls = Arc<Mutex<Vec<(String, Vec<String>)>>>;
type CaptureHandler = Box<dyn Fn(&str, &[&str]) -> io::Result<CommandOutput> + Send + Sync>;
type PassthroughHandler = Box<dyn Fn(&str, &[&str]) -> io::Result<Option<i32>> + Send + Sync>;

#[derive(Default)]
struct RecordingRunner {
    calls: RecordedCalls,
    capture_handler: Option<CaptureHandler>,
    passthrough_handler: Option<PassthroughHandler>,
}

impl SystemRunner for RecordingRunner {
    fn run_capture(
        &self,
        program: &str,
        args: &[&str],
        _env: &dyn EnvSource,
    ) -> io::Result<CommandOutput> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|s| (*s).to_string()).collect(),
        ));
        if let Some(ref handler) = self.capture_handler {
            handler(program, args)
        } else {
            Ok(CommandOutput {
                status_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn run_passthrough(
        &self,
        program: &str,
        args: &[&str],
        _env: &dyn EnvSource,
    ) -> io::Result<Option<i32>> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|s| (*s).to_string()).collect(),
        ));
        if let Some(ref handler) = self.passthrough_handler {
            handler(program, args)
        } else {
            Ok(Some(0))
        }
    }
}

#[tokio::test]
async fn completions_cover_finalized_cli_surface_without_home() {
    let empty_env = MapEnv::new();
    let expected_subcommands = [
        "daemon",
        "status",
        "pause",
        "resume",
        "cancel",
        "snooze",
        "test-trigger",
        "replay",
        "sessions",
        "calibration",
        "data",
        "config",
        "wearables",
        "enroll",
        "service",
        "update",
        "completions",
    ];

    // Bash completions
    let (code, bash_out, bash_err) =
        run_inproc_cli(&["bpmute", "completions", "bash"], &empty_env).await;
    assert_eq!(code, ExitCode::Success, "bash stderr: {bash_err}");
    assert!(
        bash_out.contains("_bpmute(") && bash_out.contains("complete "),
        "missing bash completion wrapper in output"
    );
    for sub in expected_subcommands {
        assert!(
            bash_out.contains(sub),
            "bash completion missing subcommand `{sub}`"
        );
    }

    // Zsh completions
    let (code, zsh_out, zsh_err) =
        run_inproc_cli(&["bpmute", "completions", "zsh"], &empty_env).await;
    assert_eq!(code, ExitCode::Success, "zsh stderr: {zsh_err}");
    assert!(
        zsh_out.contains("#compdef bpmute") && zsh_out.contains("_bpmute"),
        "missing zsh compdef header in output"
    );
    for sub in expected_subcommands {
        assert!(
            zsh_out.contains(sub),
            "zsh completion missing subcommand `{sub}`"
        );
    }

    // Fish completions
    let (code, fish_out, fish_err) =
        run_inproc_cli(&["bpmute", "completions", "fish"], &empty_env).await;
    assert_eq!(code, ExitCode::Success, "fish stderr: {fish_err}");
    assert!(
        fish_out.contains("complete -c bpmute"),
        "missing fish completion commands in output"
    );
    for sub in expected_subcommands {
        assert!(
            fish_out.contains(sub),
            "fish completion missing subcommand `{sub}`"
        );
    }

    // Reject TUI-only flags on completions
    let (code, _, err) =
        run_inproc_cli(&["bpmute", "--ascii", "completions", "bash"], &empty_env).await;
    assert_eq!(code, ExitCode::UsageOrConfigError, "stderr: {err}");
}

#[test]
fn systemd_unit_rendering_handles_both_targets_and_path_escaping() {
    let normal_bin = Path::new("/home/umut/.local/bin/bpmute");
    let graphical = render_systemd_unit(normal_bin, ResolvedServiceTarget::GraphicalSession);
    assert!(graphical.contains("After=dbus.socket graphical-session.target bluetooth.target"));
    assert!(graphical.contains("PartOf=graphical-session.target"));
    assert!(graphical.contains("WantedBy=graphical-session.target"));
    assert!(graphical.contains("ExecStart=/home/umut/.local/bin/bpmute daemon"));
    assert!(graphical.contains("ExecReload=/bin/kill -HUP $MAINPID"));
    assert!(graphical.contains("Restart=on-failure"));
    assert!(graphical.contains("RestartSec=3s"));
    assert!(graphical.contains("NoNewPrivileges=true"));

    let fallback = render_systemd_unit(normal_bin, ResolvedServiceTarget::Default);
    assert!(fallback.contains("After=dbus.socket bluetooth.target"));
    assert!(!fallback.contains("PartOf=graphical-session.target"));
    assert!(!fallback.contains("graphical-session.target"));
    assert!(fallback.contains("WantedBy=default.target"));
    assert!(fallback.contains("ExecStart=/home/umut/.local/bin/bpmute daemon"));
    assert!(fallback.contains("ExecReload=/bin/kill -HUP $MAINPID"));

    let spaced_pct_bin = Path::new("/opt/my 100% apps/bpmute");
    let escaped = render_systemd_unit(spaced_pct_bin, ResolvedServiceTarget::Default);
    assert!(
        escaped.contains("ExecStart=\"/opt/my 100%% apps/bpmute\" daemon"),
        "expected systemd specifier and whitespace escaping, got:\n{escaped}"
    );
}

#[test]
fn resolve_service_unit_path_prefers_xdg_config_home_with_home_fallback() {
    let xdg_env = MapEnv::new()
        .with("XDG_CONFIG_HOME", "/tmp/custom-cfg")
        .with("HOME", "/tmp/home-dir");
    assert_eq!(
        resolve_service_unit_path(&xdg_env).unwrap(),
        PathBuf::from("/tmp/custom-cfg/systemd/user/bpmute.service")
    );

    let home_env = MapEnv::new().with("HOME", "/tmp/home-dir");
    assert_eq!(
        resolve_service_unit_path(&home_env).unwrap(),
        PathBuf::from("/tmp/home-dir/.config/systemd/user/bpmute.service")
    );

    let empty_env = MapEnv::new();
    assert!(resolve_service_unit_path(&empty_env).is_err());
}

#[tokio::test]
async fn service_install_auto_detects_graphical_session_and_fallback_default() {
    // 1) graphical-session.target active (exit 0)
    let temp_gfx = TempDir::new().unwrap();
    let bin_gfx = temp_gfx.path().join("bin");
    let log_gfx = temp_gfx.path().join("calls.log");
    write_executable_script(
        &bin_gfx,
        "systemctl",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$*\" in\n  *\"is-active --quiet graphical-session.target\"*) exit 0 ;;\nesac\nexit 0\n",
            log_gfx.display()
        ),
    );
    let (env_gfx, _) = make_test_env(&temp_gfx, Some(&bin_gfx));
    let (code, _, err) = run_inproc_cli(
        &["bpmute", "service", "install", "--target", "auto"],
        &env_gfx,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let unit_gfx = temp_gfx.path().join("cfg/systemd/user/bpmute.service");
    let mode_gfx = fs::metadata(&unit_gfx).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode_gfx, 0o600, "unit file must be 0600");
    let content_gfx = fs::read_to_string(&unit_gfx).unwrap();
    assert!(content_gfx.contains("PartOf=graphical-session.target"));
    assert!(content_gfx.contains("WantedBy=graphical-session.target"));
    let calls_gfx = fs::read_to_string(&log_gfx).unwrap();
    assert!(calls_gfx.contains("--user is-active --quiet graphical-session.target"));
    assert!(calls_gfx.contains("--user daemon-reload"));
    assert!(calls_gfx.contains("--user enable --now bpmute.service"));

    // 2) graphical-session.target inactive (exit 3) -> default.target fallback
    let temp_def = TempDir::new().unwrap();
    let bin_def = temp_def.path().join("bin");
    let log_def = temp_def.path().join("calls.log");
    write_executable_script(
        &bin_def,
        "systemctl",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\ncase \"$*\" in\n  *\"is-active --quiet graphical-session.target\"*) exit 3 ;;\nesac\nexit 0\n",
            log_def.display()
        ),
    );
    let (env_def, _) = make_test_env(&temp_def, Some(&bin_def));
    let (code, _, err) = run_inproc_cli(
        &["bpmute", "service", "install", "--target", "auto"],
        &env_def,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let unit_def = temp_def.path().join("cfg/systemd/user/bpmute.service");
    let content_def = fs::read_to_string(&unit_def).unwrap();
    assert!(!content_def.contains("PartOf=graphical-session.target"));
    assert!(content_def.contains("WantedBy=default.target"));
}

#[tokio::test]
async fn service_install_explicit_targets_reinstall_and_uninstall_purge_config() {
    let temp = TempDir::new().unwrap();
    let bin_dir = temp.path().join("bin");
    let log_path = temp.path().join("calls.log");
    write_executable_script(
        &bin_dir,
        "systemctl",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            log_path.display()
        ),
    );
    let (env, paths) = make_test_env(&temp, Some(&bin_dir));

    // Create config.toml to verify uninstall preserves it unless --purge-config is passed.
    fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
    fs::write(&paths.config_file, "schema_version = 1\n").unwrap();

    // First install with --target graphical-session
    let (code, _, err) = run_inproc_cli(
        &[
            "bpmute",
            "service",
            "install",
            "--target",
            "graphical-session",
        ],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let unit_path = temp.path().join("cfg/systemd/user/bpmute.service");
    assert!(
        fs::read_to_string(&unit_path)
            .unwrap()
            .contains("WantedBy=graphical-session.target")
    );

    // Re-install with --target default -> disables stale symlink before enable --now
    fs::write(&log_path, "").unwrap();
    let (code, _, err) = run_inproc_cli(
        &["bpmute", "service", "install", "--target", "default"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(
        fs::read_to_string(&unit_path)
            .unwrap()
            .contains("WantedBy=default.target")
    );
    let reinstall_calls: Vec<String> = fs::read_to_string(&log_path)
        .unwrap()
        .lines()
        .map(|s| s.to_string())
        .collect();
    let disable_pos = reinstall_calls
        .iter()
        .position(|l| l == "--user disable bpmute.service")
        .expect("reinstall must disable existing unit first");
    let reload_pos = reinstall_calls
        .iter()
        .position(|l| l == "--user daemon-reload")
        .expect("reinstall must call daemon-reload");
    let enable_pos = reinstall_calls
        .iter()
        .position(|l| l == "--user enable --now bpmute.service")
        .expect("reinstall must call enable --now");
    assert!(disable_pos < reload_pos && reload_pos < enable_pos);

    // Uninstall without --purge-config preserves config.toml
    fs::write(&log_path, "").unwrap();
    let (code, _, err) = run_inproc_cli(&["bpmute", "service", "uninstall"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(!unit_path.try_exists().unwrap());
    assert!(paths.config_file.try_exists().unwrap());
    let uninstall_calls = fs::read_to_string(&log_path).unwrap();
    assert!(uninstall_calls.contains("--user disable --now bpmute.service"));
    assert!(uninstall_calls.contains("--user daemon-reload"));

    // Install again, then uninstall --purge-config removes config.toml too
    let (code, _, _) = run_inproc_cli(
        &["bpmute", "service", "install", "--target", "default"],
        &env,
    )
    .await;
    assert_eq!(code, ExitCode::Success);
    let (code, _, err) =
        run_inproc_cli(&["bpmute", "service", "uninstall", "--purge-config"], &env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(!unit_path.try_exists().unwrap());
    assert!(!paths.config_file.try_exists().unwrap());
}

#[test]
fn service_lifecycle_status_logs_and_missing_backend_mapping() {
    let temp = TempDir::new().unwrap();
    let (env, paths) = make_test_env(&temp, None);
    let exec_path = Path::new("/usr/local/bin/bpmute");

    // 1) start, stop, restart, reload delegate to systemctl --user
    let runner = RecordingRunner::default();
    for (cmd, verb) in [
        (ServiceCommand::Start, "start"),
        (ServiceCommand::Stop, "stop"),
        (ServiceCommand::Restart, "restart"),
        (ServiceCommand::Reload, "reload"),
    ] {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_service_with_runner(
            cmd,
            Some(paths.config_file.as_path()),
            &env,
            exec_path,
            &runner,
            &mut out,
            &mut err,
        );
        assert_eq!(code, ExitCode::Success);
        let calls = runner.calls.lock().unwrap();
        assert_eq!(
            calls.last().unwrap(),
            &(
                "systemctl".to_string(),
                vec![
                    "--user".to_string(),
                    verb.to_string(),
                    "bpmute.service".to_string()
                ]
            )
        );
    }

    // 2) status exit codes: 0 -> Success, 3 -> DaemonOffline, 4 -> PreconditionConflict, bus error -> 5
    for (status_code, stderr_msg, expected_exit) in [
        (Some(0), "", ExitCode::Success),
        (Some(3), "", ExitCode::DaemonOffline),
        (Some(4), "", ExitCode::PreconditionConflict),
        (
            Some(1),
            "Failed to connect to bus: No medium found\n",
            ExitCode::PermissionOrBackendRefusal,
        ),
    ] {
        let stderr_owned = stderr_msg.to_string();
        let status_runner = RecordingRunner {
            calls: Arc::new(Mutex::new(Vec::new())),
            capture_handler: Some(Box::new(move |_, _| {
                Ok(CommandOutput {
                    status_code,
                    stdout: "service status output\n".to_string(),
                    stderr: stderr_owned.clone(),
                })
            })),
            passthrough_handler: None,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_service_with_runner(
            ServiceCommand::Status,
            Some(paths.config_file.as_path()),
            &env,
            exec_path,
            &status_runner,
            &mut out,
            &mut err,
        );
        assert_eq!(code, expected_exit);
        assert_eq!(
            status_runner.calls.lock().unwrap()[0],
            (
                "systemctl".to_string(),
                vec![
                    "--user".to_string(),
                    "status".to_string(),
                    "bpmute.service".to_string(),
                    "--no-pager".to_string(),
                ]
            )
        );
    }

    // 3) logs vs logs -f
    let logs_runner = RecordingRunner::default();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_service_with_runner(
        ServiceCommand::Logs { follow: false },
        Some(paths.config_file.as_path()),
        &env,
        exec_path,
        &logs_runner,
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::Success);
    let code = run_service_with_runner(
        ServiceCommand::Logs { follow: true },
        Some(paths.config_file.as_path()),
        &env,
        exec_path,
        &logs_runner,
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::Success);
    let log_calls = logs_runner.calls.lock().unwrap();
    assert_eq!(
        log_calls[0],
        (
            "journalctl".to_string(),
            vec![
                "--user".to_string(),
                "-u".to_string(),
                "bpmute.service".to_string(),
                "--no-pager".to_string()
            ]
        )
    );
    assert_eq!(
        log_calls[1],
        (
            "journalctl".to_string(),
            vec![
                "--user".to_string(),
                "-u".to_string(),
                "bpmute.service".to_string(),
                "-f".to_string()
            ]
        )
    );

    // 4) missing systemctl -> exit 5 (PermissionOrBackendRefusal)
    let missing_runner = RecordingRunner {
        calls: Arc::new(Mutex::new(Vec::new())),
        capture_handler: Some(Box::new(|_, _| {
            Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
        })),
        passthrough_handler: None,
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_service_with_runner(
        ServiceCommand::Install {
            target: ServiceTargetArg::Default,
        },
        Some(paths.config_file.as_path()),
        &env,
        exec_path,
        &missing_runner,
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::PermissionOrBackendRefusal);
}

#[tokio::test]
async fn service_commands_work_with_xdg_config_only_without_home_or_data_dir() {
    let temp = TempDir::new().unwrap();
    let bin_dir = temp.path().join("bin");
    write_executable_script(&bin_dir, "systemctl", "#!/bin/sh\nexit 0\n");

    let xdg_only_env = MapEnv::new()
        .with(
            "XDG_CONFIG_HOME",
            temp.path().join("cfg").to_string_lossy().into_owned(),
        )
        .with("PATH", bin_dir.to_string_lossy().into_owned());

    let (code, _, err) = run_inproc_cli(&["bpmute", "service", "status"], &xdg_only_env).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");

    let (code, _, err) = run_inproc_cli(
        &["bpmute", "service", "install", "--target", "default"],
        &xdg_only_env,
    )
    .await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert!(
        temp.path()
            .join("cfg/systemd/user/bpmute.service")
            .try_exists()
            .unwrap()
    );
}

#[test]
fn find_updater_binary_prefers_sibling_then_path_then_none() {
    let temp = TempDir::new().unwrap();
    let sibling_dir = temp.path().join("sibling-bin");
    let path_dir = temp.path().join("path-bin");

    let fake_exe = sibling_dir.join("bpmute");
    fs::create_dir_all(&sibling_dir).unwrap();
    fs::write(&fake_exe, b"bin").unwrap();

    let path_updater = write_executable_script(&path_dir, "bpmute-update", "#!/bin/sh\nexit 0\n");
    let env_with_path = MapEnv::new().with("PATH", path_dir.to_string_lossy().into_owned());

    // Without sibling, falls back to PATH
    assert_eq!(
        find_updater_binary(Some(&fake_exe), &env_with_path),
        Some(path_updater)
    );

    // With sibling executable, sibling wins over PATH
    let sibling_updater =
        write_executable_script(&sibling_dir, "bpmute-update", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        find_updater_binary(Some(&fake_exe), &env_with_path),
        Some(sibling_updater)
    );

    // Without sibling and without PATH entry, returns None
    let empty_env = MapEnv::new();
    let other_exe = temp.path().join("isolated/bpmute");
    assert_eq!(find_updater_binary(Some(&other_exe), &empty_env), None);
}

#[tokio::test]
async fn update_delegates_to_updater_and_restarts_only_when_service_active() {
    // 1) Active service -> runs bpmute-update then systemctl --user try-restart bpmute.service
    let temp1 = TempDir::new().unwrap();
    let bin1 = temp1.path().join("bin");
    let log1 = temp1.path().join("calls.log");
    write_executable_script(
        &bin1,
        "bpmute-update",
        &format!(
            "#!/bin/sh\nprintf 'bpmute-update\\n' >> \"{}\"\nexit 0\n",
            log1.display()
        ),
    );
    write_executable_script(
        &bin1,
        "systemctl",
        &format!(
            "#!/bin/sh\nprintf 'systemctl %s\\n' \"$*\" >> \"{}\"\nexit 0\n",
            log1.display()
        ),
    );
    let (env1, _) = make_test_env(&temp1, Some(&bin1));
    let (code, _, err) = run_inproc_cli(&["bpmute", "update"], &env1).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let calls1 = fs::read_to_string(&log1).unwrap();
    assert!(calls1.contains("bpmute-update\n"));
    assert!(calls1.contains("systemctl --user is-active --quiet bpmute.service\n"));
    assert!(calls1.contains("systemctl --user try-restart bpmute.service\n"));

    // 2) Inactive service -> runs bpmute-update and does NOT run try-restart
    let temp2 = TempDir::new().unwrap();
    let bin2 = temp2.path().join("bin");
    let log2 = temp2.path().join("calls.log");
    write_executable_script(
        &bin2,
        "bpmute-update",
        &format!(
            "#!/bin/sh\nprintf 'bpmute-update\\n' >> \"{}\"\nexit 0\n",
            log2.display()
        ),
    );
    write_executable_script(
        &bin2,
        "systemctl",
        &format!(
            "#!/bin/sh\nprintf 'systemctl %s\\n' \"$*\" >> \"{}\"\ncase \"$*\" in\n  *\"is-active\"*) exit 3 ;;\nesac\nexit 0\n",
            log2.display()
        ),
    );
    let (env2, _) = make_test_env(&temp2, Some(&bin2));
    let (code, _, err) = run_inproc_cli(&["bpmute", "update"], &env2).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    let calls2 = fs::read_to_string(&log2).unwrap();
    assert!(calls2.contains("bpmute-update\n"));
    assert!(!calls2.contains("try-restart"));

    // 3) Succeeds without systemctl installed -> runs bpmute-update, does not fail
    let temp3 = TempDir::new().unwrap();
    let bin3 = temp3.path().join("bin");
    let log3 = temp3.path().join("calls.log");
    write_executable_script(
        &bin3,
        "bpmute-update",
        &format!(
            "#!/bin/sh\nprintf 'bpmute-update\\n' >> \"{}\"\nexit 0\n",
            log3.display()
        ),
    );
    let (env3, _) = make_test_env(&temp3, Some(&bin3));
    let (code, _, err) = run_inproc_cli(&["bpmute", "update"], &env3).await;
    assert_eq!(code, ExitCode::Success, "stderr: {err}");
    assert_eq!(fs::read_to_string(&log3).unwrap(), "bpmute-update\n");
}

#[test]
fn update_failure_skips_restart_and_missing_updater_prints_fallback() {
    let temp = TempDir::new().unwrap();
    let bin_dir = temp.path().join("bin");
    let updater = write_executable_script(&bin_dir, "bpmute-update", "#!/bin/sh\nexit 1\n");
    let env_with = MapEnv::new().with("PATH", bin_dir.to_string_lossy().into_owned());

    // 1) Non-zero exit from bpmute-update skips systemctl try-restart and returns RuntimeError (1)
    let fail_runner = RecordingRunner {
        calls: Arc::new(Mutex::new(Vec::new())),
        capture_handler: None,
        passthrough_handler: Some(Box::new(|_, _| Ok(Some(2)))),
    };
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = run_update_with_runner(
        &env_with,
        Some(&temp.path().join("nope/bpmute")),
        &fail_runner,
        &mut out,
        &mut err,
    );
    assert_eq!(code, ExitCode::RuntimeError);
    let calls = fail_runner.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, updater.to_string_lossy());

    // 2) Missing bpmute-update prints fallback reinstall instructions and exits PreconditionConflict (4)
    let empty_env = MapEnv::new();
    let mut out_missing = Vec::new();
    let mut err_missing = Vec::new();
    let code = run_update_with_runner(
        &empty_env,
        Some(&temp.path().join("nope/bpmute")),
        &RecordingRunner::default(),
        &mut out_missing,
        &mut err_missing,
    );
    assert_eq!(code, ExitCode::PreconditionConflict);
    let err_str = String::from_utf8_lossy(&err_missing);
    assert!(err_str.contains("bpmute-update"));
    assert!(err_str.contains("cargo binstall bpmute"));
    assert!(err_str.contains(
        "https://github.com/umutaktepe/BPMute/releases/latest/download/bpmute-installer.sh"
    ));
    assert!(err_str.contains("systemctl --user try-restart bpmute.service"));
}

#[test]
fn verify_static_musl_script_rejects_dynamic_gnu_binary_and_packaging_manifests_match() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // 1) verify-static-musl.sh rejects dynamically linked binary built for host gnu
    let verify_script = manifest_dir.join("scripts/verify-static-musl.sh");
    assert!(verify_script.try_exists().unwrap());
    let dynamic_bin = Path::new(env!("CARGO_BIN_EXE_bpmute"));
    let out = std::process::Command::new(&verify_script)
        .arg(dynamic_bin)
        .output()
        .expect("verify-static-musl.sh should execute");
    assert!(
        !out.status.success(),
        "verify-static-musl.sh must reject dynamic binary {}, stdout:\n{}\nstderr:\n{}",
        dynamic_bin.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    // 2) Verify dist-workspace.toml and .github/build-setup.yml
    let dist_raw = fs::read_to_string(manifest_dir.join("dist-workspace.toml")).unwrap();
    let dist_val: toml::Value = toml::from_str(&dist_raw).unwrap();
    let dist = dist_val.get("dist").expect("missing [dist] table");
    assert_eq!(
        dist.get("install-updater").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        dist.get("unix-archive").and_then(|v| v.as_str()),
        Some(".tar.xz")
    );
    assert_eq!(
        dist.get("github-build-setup").and_then(|v| v.as_str()),
        Some("../build-setup.yml")
    );
    assert!(
        manifest_dir
            .join(".github/build-setup.yml")
            .try_exists()
            .unwrap()
    );
    let targets: Vec<&str> = dist
        .get("targets")
        .and_then(|v| v.as_array())
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(targets.contains(&"x86_64-unknown-linux-musl"));
    assert!(targets.contains(&"aarch64-unknown-linux-musl"));

    // 3) Verify Cargo.toml [package.metadata.binstall] and [profile.dist]
    let cargo_raw = fs::read_to_string(manifest_dir.join("Cargo.toml")).unwrap();
    let cargo_val: toml::Value = toml::from_str(&cargo_raw).unwrap();
    let binstall = cargo_val
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("binstall"))
        .expect("missing [package.metadata.binstall]");
    assert_eq!(
        binstall.get("pkg-fmt").and_then(|v| v.as_str()),
        Some("txz")
    );
    assert!(
        binstall
            .get("pkg-url")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("{ repo }") && s.contains("{ target }.tar.xz"))
    );
    assert!(
        binstall
            .get("bin-dir")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.contains("{ name }-{ target }/{ bin }"))
    );
    assert!(
        cargo_val
            .get("profile")
            .and_then(|p| p.get("dist"))
            .is_some()
    );
}
