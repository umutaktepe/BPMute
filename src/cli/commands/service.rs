use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cli::args::{ServiceCommand, ServiceTargetArg};
use crate::cli::commands::system_exec::{
    CommandOutput, OsSystemRunner, SYSTEMD_SERVICE_NAME, SystemRunner, classify_systemd_io_error,
    classify_systemd_stderr, command_failure_detail, io_error_exit_code,
};
use crate::exit_code::ExitCode;
use crate::paths::{EnvSource, PathResolutionError, write_private_file_atomic_preserve_parent};

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
        return Ok(PathBuf::from(xdg)
            .join("systemd/user")
            .join(SYSTEMD_SERVICE_NAME));
    }
    if let Some(home) = env.get("HOME").filter(|v| !v.trim().is_empty()) {
        return Ok(PathBuf::from(home)
            .join(".config/systemd/user")
            .join(SYSTEMD_SERVICE_NAME));
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
    let (after_line, part_of_line, wanted_by) = match target {
        ResolvedServiceTarget::GraphicalSession => (
            "After=dbus.socket graphical-session.target bluetooth.target",
            "PartOf=graphical-session.target\n",
            "graphical-session.target",
        ),
        ResolvedServiceTarget::Default => {
            ("After=dbus.socket bluetooth.target", "", "default.target")
        }
    };
    format!(
        "[Unit]\n\
         Description=BPMute heart-rate sleep automation daemon\n\
         {after_line}\n\
         {part_of_line}\n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} daemon\n\
         ExecReload=/bin/kill -HUP $MAINPID\n\
         Restart=on-failure\n\
         RestartSec=3s\n\
         NoNewPrivileges=true\n\
         \n\
         [Install]\n\
         WantedBy={wanted_by}\n"
    )
}

/// Executes a `bpmute service ...` subcommand using the default OS process runner.
pub fn run_service<WOut: Write, WErr: Write>(
    action: ServiceCommand,
    config_file: Option<&Path>,
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
        config_file,
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
    config_file: Option<&Path>,
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
            uninstall_service(purge_config, config_file, env, runner, stdout, stderr)
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

    match path_exists_checked(&unit_path, "systemd unit", stderr) {
        Ok(true) => {
            if let Err(code) = run_systemctl_allow_nonfatal(
                &["--user", "disable", SYSTEMD_SERVICE_NAME],
                env,
                runner,
                stderr,
            ) {
                return code;
            }
        }
        Ok(false) => {}
        Err(code) => return code,
    }

    let unit_content = render_systemd_unit(exec_path, target);
    if let Err(err) = write_private_file_atomic_preserve_parent(&unit_path, unit_content.as_bytes())
    {
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
        &["--user", "enable", "--now", SYSTEMD_SERVICE_NAME],
        env,
        runner,
        stderr,
    ) {
        return code;
    }

    let _ = writeln!(
        stdout,
        "Installed and enabled {} (target: {}) at {}.",
        SYSTEMD_SERVICE_NAME,
        target.label(),
        unit_path.display()
    );
    ExitCode::Success
}

fn uninstall_service<WOut: Write, WErr: Write>(
    purge_config: bool,
    config_file: Option<&Path>,
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
        &["--user", "disable", "--now", SYSTEMD_SERVICE_NAME],
        env,
        runner,
        stderr,
    ) {
        return code;
    }

    match path_exists_checked(&unit_path, "systemd unit", stderr) {
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
        Err(code) => return code,
    }

    if let Err(code) = run_systemctl_checked(&["--user", "daemon-reload"], env, runner, stderr) {
        return code;
    }

    let mut purged_path: Option<&Path> = None;
    if purge_config {
        let Some(cfg_path) = config_file else {
            let _ = writeln!(
                stderr,
                "error: unable to resolve config.toml path for --purge-config"
            );
            return ExitCode::UsageOrConfigError;
        };
        match path_exists_checked(cfg_path, "config file", stderr) {
            Ok(true) => {
                if let Err(err) = fs::remove_file(cfg_path) {
                    let _ = writeln!(
                        stderr,
                        "error: failed to remove config file {}: {err}",
                        cfg_path.display()
                    );
                    return io_error_exit_code(&err);
                }
                purged_path = Some(cfg_path);
            }
            Ok(false) => {}
            Err(code) => return code,
        }
    }

    if let Some(cfg_path) = purged_path {
        let _ = writeln!(
            stdout,
            "Uninstalled {} and purged configuration at {}.",
            SYSTEMD_SERVICE_NAME,
            cfg_path.display()
        );
    } else {
        let _ = writeln!(
            stdout,
            "Uninstalled {} ({}).",
            SYSTEMD_SERVICE_NAME,
            unit_path.display()
        );
    }
    ExitCode::Success
}

fn path_exists_checked<WErr: Write>(
    path: &Path,
    label: &str,
    stderr: &mut WErr,
) -> Result<bool, ExitCode> {
    path.try_exists().map_err(|err| {
        let _ = writeln!(
            stderr,
            "error: failed to inspect {label} {}: {err}",
            path.display()
        );
        io_error_exit_code(&err)
    })
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
                let _ = writeln!(stderr, "error: {}", command_failure_detail(&out));
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
    match runner.run_capture("systemctl", &["--user", verb, SYSTEMD_SERVICE_NAME], env) {
        Ok(out) => {
            if out.status_code == Some(0) {
                if !out.stdout.is_empty() {
                    let _ = write!(stdout, "{}", out.stdout);
                } else {
                    let _ = writeln!(
                        stdout,
                        "systemctl --user {verb} {SYSTEMD_SERVICE_NAME} succeeded."
                    );
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
        &["--user", "status", SYSTEMD_SERVICE_NAME, "--no-pager"],
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
        let args = ["--user", "-u", SYSTEMD_SERVICE_NAME, "-f"];
        match runner.run_passthrough("journalctl", &args, env) {
            Ok(Some(0)) => ExitCode::Success,
            Ok(_) => ExitCode::RuntimeError,
            Err(err) => {
                let _ = writeln!(stderr, "error: failed to execute journalctl: {err}");
                classify_systemd_io_error(&err)
            }
        }
    } else {
        let args = ["--user", "-u", SYSTEMD_SERVICE_NAME, "--no-pager"];
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
    let _ = writeln!(stderr, "error: {}", command_failure_detail(out));
}
