use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::commands::system_exec::{
    OsSystemRunner, SYSTEMD_SERVICE_NAME, SystemRunner, classify_systemd_io_error,
    classify_systemd_stderr, command_failure_detail, io_error_exit_code, is_executable_file,
    resolve_program_in_env,
};
use crate::exit_code::ExitCode;
use crate::paths::EnvSource;

const UPDATER_BINARY: &str = "bpmute-update";
const INSTALLER_URL: &str =
    "https://github.com/umutaktepe/BPMute/releases/latest/download/bpmute-installer.sh";

/// Locates `bpmute-update` as a sibling of `current_exe`, falling back to `PATH`.
pub fn find_updater_binary(current_exe: Option<&Path>, env: &dyn EnvSource) -> Option<PathBuf> {
    if let Some(parent) = current_exe.and_then(|exe| exe.parent()) {
        let sibling = parent.join(UPDATER_BINARY);
        if is_executable_file(&sibling) {
            return Some(sibling);
        }
    }
    resolve_program_in_env(UPDATER_BINARY, env)
}

/// Executes `bpmute update` using the default OS process runner.
pub fn run_update<WOut: Write, WErr: Write>(
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let current_exe = std::env::current_exe().ok();
    run_update_with_runner(env, current_exe.as_deref(), &OsSystemRunner, stdout, stderr)
}

/// Executes `bpmute update` with an injected `current_exe` and `SystemRunner`.
pub fn run_update_with_runner<WOut: Write, WErr: Write>(
    env: &dyn EnvSource,
    current_exe: Option<&Path>,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let Some(updater_path) = find_updater_binary(current_exe, env) else {
        write_missing_updater_instructions(stderr);
        return ExitCode::PreconditionConflict;
    };

    let updater_str = updater_path.to_string_lossy();
    match runner.run_passthrough(&updater_str, &[], env) {
        Ok(Some(0)) => maybe_restart_service(env, runner, stdout, stderr),
        Ok(Some(code)) => {
            let _ = writeln!(stderr, "error: {UPDATER_BINARY} exited with status {code}");
            ExitCode::RuntimeError
        }
        Ok(None) => {
            let _ = writeln!(stderr, "error: {UPDATER_BINARY} terminated by signal");
            ExitCode::RuntimeError
        }
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to launch {}: {err}",
                updater_path.display()
            );
            io_error_exit_code(&err)
        }
    }
}

fn maybe_restart_service<WOut: Write, WErr: Write>(
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let active = matches!(
        runner.run_capture(
            "systemctl",
            &["--user", "is-active", "--quiet", SYSTEMD_SERVICE_NAME],
            env,
        ),
        Ok(ref out) if out.status_code == Some(0)
    );

    if !active {
        let _ = writeln!(
            stdout,
            "Updated BPMute ({SYSTEMD_SERVICE_NAME} is not active; skipped service restart)."
        );
        return ExitCode::Success;
    }

    match runner.run_capture(
        "systemctl",
        &["--user", "try-restart", SYSTEMD_SERVICE_NAME],
        env,
    ) {
        Ok(out) if out.status_code == Some(0) => {
            let _ = writeln!(
                stdout,
                "Updated BPMute and restarted {SYSTEMD_SERVICE_NAME}."
            );
            ExitCode::Success
        }
        Ok(out) => {
            let _ = writeln!(
                stderr,
                "error: failed to restart {SYSTEMD_SERVICE_NAME}: {}",
                command_failure_detail(&out)
            );
            classify_systemd_stderr(&out.stderr)
        }
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to restart {SYSTEMD_SERVICE_NAME}: {err}"
            );
            classify_systemd_io_error(&err)
        }
    }
}

fn write_missing_updater_instructions<WErr: Write>(stderr: &mut WErr) {
    let _ = writeln!(
        stderr,
        "error: companion updater `{UPDATER_BINARY}` was not found (expected for `cargo-binstall` or manual builds without a cargo-dist install receipt).\n\n\
         To upgrade BPMute, reinstall using one of:\n  \
           - cargo binstall bpmute\n  \
           - curl --proto '=https' --tlsv1.2 -LsSf {INSTALLER_URL} | sh\n\n\
         If the background daemon is running, restart it afterwards with:\n  \
           systemctl --user try-restart {SYSTEMD_SERVICE_NAME}"
    );
}
