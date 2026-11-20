use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use crate::exit_code::ExitCode;
use crate::paths::EnvSource;

const PROPAGATED_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Captured stdout, stderr, and exit status from an external OS command invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Boundary trait for spawning external CLI utilities (`systemctl`, `journalctl`, `bpmute-update`).
pub trait SystemRunner: Send + Sync {
    /// Runs `program` with `args` under `env`, capturing stdout and stderr.
    fn run_capture(
        &self,
        program: &str,
        args: &[&str],
        env: &dyn EnvSource,
    ) -> io::Result<CommandOutput>;

    /// Runs `program` with `args` under `env`, inheriting stdio and returning its exit status.
    fn run_passthrough(
        &self,
        program: &str,
        args: &[&str],
        env: &dyn EnvSource,
    ) -> io::Result<Option<i32>>;
}

/// Production OS command runner backed by `std::process::Command` and injected `EnvSource`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsSystemRunner;

impl SystemRunner for OsSystemRunner {
    fn run_capture(
        &self,
        program: &str,
        args: &[&str],
        env: &dyn EnvSource,
    ) -> io::Result<CommandOutput> {
        let mut cmd = build_command(program, args, env)?;
        let out = cmd.output()?;
        Ok(CommandOutput {
            status_code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_passthrough(
        &self,
        program: &str,
        args: &[&str],
        env: &dyn EnvSource,
    ) -> io::Result<Option<i32>> {
        let mut cmd = build_command(program, args, env)?;
        let status = cmd.status()?;
        Ok(status.code())
    }
}

fn build_command(program: &str, args: &[&str], env: &dyn EnvSource) -> io::Result<ProcessCommand> {
    let resolved = resolve_program_in_env(program, env).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("executable `{program}` not found in PATH"),
        )
    })?;
    let mut cmd = ProcessCommand::new(resolved);
    cmd.args(args);
    for &key in PROPAGATED_ENV_KEYS {
        if let Some(val) = env.get(key) {
            cmd.env(key, val);
        } else {
            cmd.env_remove(key);
        }
    }
    Ok(cmd)
}

/// Checks whether `path` refers to a regular file (following symlinks) with at least one execute bit.
pub fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && (meta.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}

/// Resolves `program` against injected `EnvSource` `PATH` (or directly if it contains `/`).
pub fn resolve_program_in_env(program: &str, env: &dyn EnvSource) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return is_executable_file(&candidate).then_some(candidate);
    }
    let path_var = env.get("PATH")?;
    for dir in path_var.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = Path::new(dir).join(program);
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Maps an I/O error spawning `systemctl`/`journalctl` to a semantic CLI exit code.
pub fn classify_systemd_io_error(err: &io::Error) -> ExitCode {
    match err.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied => {
            ExitCode::PermissionOrBackendRefusal
        }
        _ => ExitCode::RuntimeError,
    }
}

/// Maps `systemctl`/`journalctl` stderr text on failure to a semantic CLI exit code.
pub fn classify_systemd_stderr(stderr: &str) -> ExitCode {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("failed to connect to bus")
        || lower.contains("system has not been booted with systemd")
        || lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("interactive authentication required")
    {
        ExitCode::PermissionOrBackendRefusal
    } else {
        ExitCode::RuntimeError
    }
}
