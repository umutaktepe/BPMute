use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::cli::args::{ServiceCommand, ServiceTargetArg};
use crate::cli::commands::system_exec::{
    CommandOutput, OsSystemRunner, SystemRunner, classify_systemd_io_error, classify_systemd_stderr,
};
use crate::exit_code::ExitCode;
use crate::paths::{EnvSource, PathResolutionError, ResolvedPaths};

const SERVICE_NAME: &str = "bpmute.service";
static UNIT_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Resolved `systemd --user` target mode for `bpmute.service`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedServiceTarget {
    GraphicalSession,
    Default,
}

impl ResolvedServiceTarget {
    fn label(self) -> &'static str {
        match self {
            Self::GraphicalSession => "graphical-session.target",
            Self::Default => "default.target",
        }
    }
}

/// Resolves the user systemd unit path (`$XDG_CONFIG_HOME/systemd/user/bpmute.service`).
pub fn resolve_service_unit_path(env: &dyn EnvSource) -> Result<PathBuf, PathResolutionError> {
    if let Some(xdg) = env.get("XDG_CONFIG_HOME").filter(|v| !v.trim().is_empty()) {
        return Ok(PathBuf::from(xdg).join("systemd/user").join(SERVICE_NAME));
    }
    if let Some(home) = env.get("HOME").filter(|v| !v.trim().is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".config/systemd/user")
            .join(SERVICE_NAME));
    }
    Err(PathResolutionError::MissingHome {
        kind: "systemd user unit",
    })
}

/// Formats an executable path for a `systemd` `ExecStart=` argument.
fn format_systemd_exec_arg(exec_path: &Path) -> String {
    let escaped_pct = exec_path.to_string_lossy().replace('%', "%%");
    let needs_quotes = escaped_pct.is_empty()
        || escaped_pct
            .chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\'' || c == '\\');
    if !needs_quotes {
        return escaped_pct;
    }
    let mut quoted = String::with_capacity(escaped_pct.len() + 4);
    quoted.push('"');
    for ch in escaped_pct.chars() {
        match ch {
            '\\' | '"' => {
                quoted.push('\\');
                quoted.push(ch);
            }
            _ => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

/// Renders the `bpmute.service` INI unit file for `exec_path` and `target`.
pub fn render_systemd_unit(exec_path: &Path, target: ResolvedServiceTarget) -> String {
    let exec = format_systemd_exec_arg(exec_path);
    match target {
        ResolvedServiceTarget::GraphicalSession => format!(
            "[Unit]\n\
             Description=BPMute heart-rate sleep automation daemon\n\
             After=dbus.socket graphical-session.target bluetooth.target\n\
             PartOf=graphical-session.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exec} daemon\n\
             ExecReload=/bin/kill -HUP $MAINPID\n\
             Restart=on-failure\n\
             RestartSec=3s\n\
             NoNewPrivileges=true\n\
             \n\
             [Install]\n\
             WantedBy=graphical-session.target\n"
        ),
        ResolvedServiceTarget::Default => format!(
            "[Unit]\n\
             Description=BPMute heart-rate sleep automation daemon\n\
             After=dbus.socket bluetooth.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={exec} daemon\n\
             ExecReload=/bin/kill -HUP $MAINPID\n\
             Restart=on-failure\n\
             RestartSec=3s\n\
             NoNewPrivileges=true\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        ),
    }
}

/// Executes a `bpmute service ...` subcommand using the default OS process runner.
pub fn run_service<WOut: Write, WErr: Write>(
    action: ServiceCommand,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let exec_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: unable to resolve current bpmute executable path: {err}"
            );
            return ExitCode::RuntimeError;
        }
    };
    run_service_with_runner(
        action,
        paths,
        env,
        &exec_path,
        &OsSystemRunner,
        stdout,
        stderr,
    )
}

/// Executes a `bpmute service ...` subcommand with an injected binary path and command runner.
pub fn run_service_with_runner<WOut: Write, WErr: Write>(
    action: ServiceCommand,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
    exec_path: &Path,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    match action {
        ServiceCommand::Install { target } => {
            install_service(target, env, exec_path, runner, stdout, stderr)
        }
        ServiceCommand::Uninstall { purge_config } => {
            uninstall_service(purge_config, paths, env, runner, stdout, stderr)
        }
        ServiceCommand::Start => run_systemctl_verb("start", env, runner, stdout, stderr),
        ServiceCommand::Stop => run_systemctl_verb("stop", env, runner, stdout, stderr),
        ServiceCommand::Restart => run_systemctl_verb("restart", env, runner, stdout, stderr),
        ServiceCommand::Reload => run_systemctl_verb("reload", env, runner, stdout, stderr),
        ServiceCommand::Status => run_service_status(env, runner, stdout, stderr),
        ServiceCommand::Logs { follow } => run_service_logs(follow, env, runner, stdout, stderr),
    }
}

fn install_service<WOut: Write, WErr: Write>(
    target_arg: ServiceTargetArg,
    env: &dyn EnvSource,
    exec_path: &Path,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let unit_path = match resolve_service_unit_path(env) {
        Ok(p) => p,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return ExitCode::UsageOrConfigError;
        }
    };

    let target = match resolve_target(target_arg, env, runner, stderr) {
        Ok(t) => t,
        Err(code) => return code,
    };

    match unit_path.try_exists() {
        Ok(true) => {
            if let Err(code) = run_systemctl_allow_nonfatal(
                &["--user", "disable", SERVICE_NAME],
                env,
                runner,
                stderr,
            ) {
                return code;
            }
        }
        Ok(false) => {}
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to inspect {}: {err}",
                unit_path.display()
            );
            return io_error_exit_code(&err);
        }
    }

    let unit_content = render_systemd_unit(exec_path, target);
    if let Err(err) = write_unit_file_atomic(&unit_path, unit_content.as_bytes()) {
        let _ = writeln!(
            stderr,
            "error: failed to write systemd unit {}: {err}",
            unit_path.display()
        );
        return io_error_exit_code(&err);
    }

    if let Err(code) = run_systemctl_checked(&["--user", "daemon-reload"], env, runner, stderr) {
        return code;
    }
    if let Err(code) = run_systemctl_checked(
        &["--user", "enable", "--now", SERVICE_NAME],
        env,
        runner,
        stderr,
    ) {
        return code;
    }

    let _ = writeln!(
        stdout,
        "Installed and enabled {} (target: {}) at {}.",
        SERVICE_NAME,
        target.label(),
        unit_path.display()
    );
    ExitCode::Success
}

fn uninstall_service<WOut: Write, WErr: Write>(
    purge_config: bool,
    paths: &ResolvedPaths,
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    let unit_path = match resolve_service_unit_path(env) {
        Ok(p) => p,
        Err(err) => {
            let _ = writeln!(stderr, "error: {err}");
            return ExitCode::UsageOrConfigError;
        }
    };

    if let Err(code) = run_systemctl_allow_nonfatal(
        &["--user", "disable", "--now", SERVICE_NAME],
        env,
        runner,
        stderr,
    ) {
        return code;
    }

    match unit_path.try_exists() {
        Ok(true) => {
            if let Err(err) = fs::remove_file(&unit_path) {
                let _ = writeln!(
                    stderr,
                    "error: failed to remove {}: {err}",
                    unit_path.display()
                );
                return io_error_exit_code(&err);
            }
        }
        Ok(false) => {}
        Err(err) => {
            let _ = writeln!(
                stderr,
                "error: failed to inspect {}: {err}",
                unit_path.display()
            );
            return io_error_exit_code(&err);
        }
    }

    if let Err(code) = run_systemctl_checked(&["--user", "daemon-reload"], env, runner, stderr) {
        return code;
    }

    let mut purged_config = false;
    if purge_config {
        match paths.config_file.try_exists() {
            Ok(true) => {
                if let Err(err) = fs::remove_file(&paths.config_file) {
                    let _ = writeln!(
                        stderr,
                        "error: failed to remove config file {}: {err}",
                        paths.config_file.display()
                    );
                    return io_error_exit_code(&err);
                }
                purged_config = true;
            }
            Ok(false) => {}
            Err(err) => {
                let _ = writeln!(
                    stderr,
                    "error: failed to inspect config file {}: {err}",
                    paths.config_file.display()
                );
                return io_error_exit_code(&err);
            }
        }
    }

    if purged_config {
        let _ = writeln!(
            stdout,
            "Uninstalled {} and purged configuration at {}.",
            SERVICE_NAME,
            paths.config_file.display()
        );
    } else {
        let _ = writeln!(
            stdout,
            "Uninstalled {} ({}).",
            SERVICE_NAME,
            unit_path.display()
        );
    }
    ExitCode::Success
}

fn resolve_target<WErr: Write>(
    target_arg: ServiceTargetArg,
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stderr: &mut WErr,
) -> Result<ResolvedServiceTarget, ExitCode> {
    match target_arg {
        ServiceTargetArg::GraphicalSession => Ok(ResolvedServiceTarget::GraphicalSession),
        ServiceTargetArg::Default => Ok(ResolvedServiceTarget::Default),
        ServiceTargetArg::Auto => {
            let out = runner
                .run_capture(
                    "systemctl",
                    &["--user", "is-active", "--quiet", "graphical-session.target"],
                    env,
                )
                .map_err(|err| {
                    let _ = writeln!(stderr, "error: failed to execute systemctl: {err}");
                    classify_systemd_io_error(&err)
                })?;

            if out.status_code == Some(0) {
                return Ok(ResolvedServiceTarget::GraphicalSession);
            }
            if classify_systemd_stderr(&out.stderr) == ExitCode::PermissionOrBackendRefusal {
                let msg = out.stderr.trim();
                let _ = writeln!(stderr, "error: {msg}");
                return Err(ExitCode::PermissionOrBackendRefusal);
            }
            Ok(ResolvedServiceTarget::Default)
        }
    }
}

fn run_systemctl_verb<WOut: Write, WErr: Write>(
    verb: &str,
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    match runner.run_capture("systemctl", &["--user", verb, SERVICE_NAME], env) {
        Ok(out) => {
            if out.status_code == Some(0) {
                if !out.stdout.is_empty() {
                    let _ = write!(stdout, "{}", out.stdout);
                } else {
                    let _ = writeln!(stdout, "systemctl --user {verb} {SERVICE_NAME} succeeded.");
                }
                ExitCode::Success
            } else {
                write_failure_output(&out, stderr);
                classify_systemd_stderr(&out.stderr)
            }
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: failed to execute systemctl: {err}");
            classify_systemd_io_error(&err)
        }
    }
}

fn run_service_status<WOut: Write, WErr: Write>(
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    match runner.run_capture(
        "systemctl",
        &["--user", "status", SERVICE_NAME, "--no-pager"],
        env,
    ) {
        Ok(out) => {
            if !out.stdout.is_empty() {
                let _ = write!(stdout, "{}", out.stdout);
            }
            if !out.stderr.is_empty() {
                let _ = write!(stderr, "{}", out.stderr);
            }
            match out.status_code {
                Some(0) => ExitCode::Success,
                Some(3) => {
                    if classify_systemd_stderr(&out.stderr) == ExitCode::PermissionOrBackendRefusal
                    {
                        ExitCode::PermissionOrBackendRefusal
                    } else {
                        ExitCode::DaemonOffline
                    }
                }
                Some(4) => {
                    if classify_systemd_stderr(&out.stderr) == ExitCode::PermissionOrBackendRefusal
                    {
                        ExitCode::PermissionOrBackendRefusal
                    } else {
                        ExitCode::PreconditionConflict
                    }
                }
                _ => classify_systemd_stderr(&out.stderr),
            }
        }
        Err(err) => {
            let _ = writeln!(stderr, "error: failed to execute systemctl: {err}");
            classify_systemd_io_error(&err)
        }
    }
}

fn run_service_logs<WOut: Write, WErr: Write>(
    follow: bool,
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stdout: &mut WOut,
    stderr: &mut WErr,
) -> ExitCode {
    if follow {
        let args = ["--user", "-u", SERVICE_NAME, "-f"];
        match runner.run_passthrough("journalctl", &args, env) {
            Ok(Some(0)) => ExitCode::Success,
            Ok(_) => ExitCode::RuntimeError,
            Err(err) => {
                let _ = writeln!(stderr, "error: failed to execute journalctl: {err}");
                classify_systemd_io_error(&err)
            }
        }
    } else {
        let args = ["--user", "-u", SERVICE_NAME, "--no-pager"];
        match runner.run_capture("journalctl", &args, env) {
            Ok(out) => {
                if !out.stdout.is_empty() {
                    let _ = write!(stdout, "{}", out.stdout);
                }
                if !out.stderr.is_empty() {
                    let _ = write!(stderr, "{}", out.stderr);
                }
                if out.status_code == Some(0) {
                    ExitCode::Success
                } else {
                    classify_systemd_stderr(&out.stderr)
                }
            }
            Err(err) => {
                let _ = writeln!(stderr, "error: failed to execute journalctl: {err}");
                classify_systemd_io_error(&err)
            }
        }
    }
}

fn run_systemctl_checked<WErr: Write>(
    args: &[&str],
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stderr: &mut WErr,
) -> Result<(), ExitCode> {
    let out = runner.run_capture("systemctl", args, env).map_err(|err| {
        let _ = writeln!(stderr, "error: failed to execute systemctl: {err}");
        classify_systemd_io_error(&err)
    })?;
    if out.status_code == Some(0) {
        return Ok(());
    }
    write_failure_output(&out, stderr);
    Err(classify_systemd_stderr(&out.stderr))
}

fn run_systemctl_allow_nonfatal<WErr: Write>(
    args: &[&str],
    env: &dyn EnvSource,
    runner: &dyn SystemRunner,
    stderr: &mut WErr,
) -> Result<(), ExitCode> {
    let out = runner.run_capture("systemctl", args, env).map_err(|err| {
        let _ = writeln!(stderr, "error: failed to execute systemctl: {err}");
        classify_systemd_io_error(&err)
    })?;
    if out.status_code != Some(0)
        && classify_systemd_stderr(&out.stderr) == ExitCode::PermissionOrBackendRefusal
    {
        write_failure_output(&out, stderr);
        return Err(ExitCode::PermissionOrBackendRefusal);
    }
    Ok(())
}

fn write_failure_output<WErr: Write>(out: &CommandOutput, stderr: &mut WErr) {
    let detail = if !out.stderr.trim().is_empty() {
        out.stderr.trim()
    } else if !out.stdout.trim().is_empty() {
        out.stdout.trim()
    } else {
        "command exited with non-zero status"
    };
    let _ = writeln!(stderr, "error: {detail}");
}

fn io_error_exit_code(err: &io::Error) -> ExitCode {
    if err.kind() == io::ErrorKind::PermissionDenied {
        ExitCode::PermissionOrBackendRefusal
    } else {
        ExitCode::RuntimeError
    }
}

/// Atomically writes a `0600` unit file without modifying permissions on shared XDG systemd dirs.
fn write_unit_file_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if parent != Path::new(".") {
        fs::create_dir_all(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bpmute.service");
    let pid = std::process::id();

    for _ in 0..64 {
        let seq = UNIT_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = parent.join(format!(".{file_name}.tmp.{pid}.{seq}"));

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp_path)
        {
            Ok(mut file) => {
                let res = (|| -> io::Result<()> {
                    file.write_all(content)?;
                    file.set_permissions(fs::Permissions::from_mode(0o600))?;
                    file.sync_all()?;
                    drop(file);
                    fs::rename(&tmp_path, path)?;
                    Ok(())
                })();
                if let Err(err) = res {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(err);
                }
                return Ok(());
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "failed to allocate unique temporary file for systemd unit",
    ))
}
