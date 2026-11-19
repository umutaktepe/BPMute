//! Exclusive `flock` coordination on `$XDG_DATA_HOME/bpmute/bpmute.lock` (ADR-0008).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::paths::ensure_private_dir;
use crate::storage::StorageError;

/// Exclusive POSIX `flock` guard held around ledger write transactions.
#[derive(Debug)]
pub struct LedgerFileLock {
    _file: File,
}

impl LedgerFileLock {
    /// Opens `lock_path` (`0700` dir / `0600` file) and blocks until an exclusive `flock` is acquired.
    pub fn acquire_exclusive(lock_path: &Path) -> Result<Self, StorageError> {
        let file = open_lock_file(lock_path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

fn open_lock_file(lock_path: &Path) -> Result<File, StorageError> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_private_dir(parent)?;
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;

    if let Err(err) = file.set_permissions(std::fs::Permissions::from_mode(0o600))
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(StorageError::Io(err));
    }

    Ok(file)
}
