use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

static TEMP_FILE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Abstraction over environment variable lookups for deterministic testing.
pub trait EnvSource: Send + Sync {
    /// Retrieves an environment variable value by name, if present.
    fn get(&self, key: &str) -> Option<String>;
}

/// Process environment implementation backed by `std::env::var`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// In-memory environment map for unit and virtual-time integration tests.
#[derive(Debug, Default, Clone)]
pub struct MapEnv(pub BTreeMap<String, String>);

impl MapEnv {
    /// Creates an empty in-memory environment map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts an environment variable into the map, returning `self` for chaining.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(key.into(), value.into());
        self
    }
}

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Errors that can occur while resolving XDG/CLI/env filesystem paths.
#[derive(Debug, Error)]
pub enum PathResolutionError {
    #[error("unable to resolve {kind} path: neither XDG directories nor HOME are set")]
    MissingHome { kind: &'static str },
    #[error("unable to determine current user UID for fallback runtime socket path: {0}")]
    UidLookup(#[source] io::Error),
}

/// Resolved filesystem paths for config, persistent data, and the IPC socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPaths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl ResolvedPaths {
    /// Resolves paths using precedence: `CLI flag > BPMUTE_* env var > XDG_* > HOME fallback`.
    pub fn resolve(
        cli_config: Option<&Path>,
        cli_data_dir: Option<&Path>,
        cli_socket: Option<&Path>,
        env: &dyn EnvSource,
    ) -> Result<Self, PathResolutionError> {
        let config_file = resolve_config_file(cli_config, env)?;
        let data_dir = resolve_data_dir(cli_data_dir, env)?;
        let socket_path = resolve_socket_path(cli_socket, env)?;

        Ok(Self {
            config_file,
            data_dir,
            socket_path,
        })
    }
}

fn non_empty_env(env: &dyn EnvSource, key: &str) -> Option<String> {
    env.get(key).filter(|val| !val.is_empty())
}

fn resolve_config_file(
    cli_config: Option<&Path>,
    env: &dyn EnvSource,
) -> Result<PathBuf, PathResolutionError> {
    if let Some(path) = cli_config {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = non_empty_env(env, "BPMUTE_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(xdg_config) = non_empty_env(env, "XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(xdg_config).join("bpmute").join("config.toml"));
    }
    if let Some(home) = non_empty_env(env, "HOME") {
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("bpmute")
            .join("config.toml"));
    }
    Err(PathResolutionError::MissingHome { kind: "config" })
}

fn resolve_data_dir(
    cli_data_dir: Option<&Path>,
    env: &dyn EnvSource,
) -> Result<PathBuf, PathResolutionError> {
    if let Some(path) = cli_data_dir {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = non_empty_env(env, "BPMUTE_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(xdg_data) = non_empty_env(env, "XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg_data).join("bpmute"));
    }
    if let Some(home) = non_empty_env(env, "HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("bpmute"));
    }
    Err(PathResolutionError::MissingHome { kind: "data_dir" })
}

fn resolve_socket_path(
    cli_socket: Option<&Path>,
    env: &dyn EnvSource,
) -> Result<PathBuf, PathResolutionError> {
    if let Some(path) = cli_socket {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = non_empty_env(env, "BPMUTE_SOCKET") {
        return Ok(PathBuf::from(path));
    }
    if let Some(xdg_runtime) = non_empty_env(env, "XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(xdg_runtime)
            .join("bpmute")
            .join("daemon.sock"));
    }
    let uid = current_uid().map_err(PathResolutionError::UidLookup)?;
    Ok(PathBuf::from(format!("/tmp/bpmute-{uid}")).join("daemon.sock"))
}

fn current_uid() -> io::Result<u32> {
    Ok(fs::metadata("/proc/self")?.uid())
}

fn is_protected_system_dir(dir: &Path) -> bool {
    matches!(
        dir.to_str(),
        Some("/" | "/tmp" | "/var/tmp" | "/run" | "/var" | "/home" | "/etc" | "/usr")
    )
}

/// Ensures `dir` exists as a directory with strict `0700` permissions.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if dir.as_os_str().is_empty() {
        return Ok(());
    }
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;

    if !is_protected_system_dir(dir) {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomically writes `content` to `path` with `0700` parent directory and `0600` file mode.
pub fn write_private_file_atomic(path: &Path, content: &[u8]) -> io::Result<()> {
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if parent != Path::new(".") {
        ensure_private_dir(parent)?;
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bpmute");
    let pid = std::process::id();

    for _ in 0..64 {
        let seq = TEMP_FILE_SEQ.fetch_add(1, Ordering::Relaxed);
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
        "failed to allocate unique temporary file for atomic write",
    ))
}
