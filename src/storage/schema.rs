//! SQLite WAL schema initialization, `0600` file permission checks, and UUIDv7 generation.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use uuid::{ContextV7, Timestamp, Uuid};

use crate::storage::StorageError;

/// Current SQLite ledger schema version tracked via `PRAGMA user_version`.
pub const CURRENT_DB_SCHEMA_VERSION: i32 = 1;

/// Returns `<data_dir>/bpmute.db`.
pub fn db_path_in_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bpmute.db")
}

/// Returns `<data_dir>/bpmute.lock`.
pub fn lock_path_in_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bpmute.lock")
}

/// Returns `<data_dir>/backups`.
pub fn backups_dir_in_data_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("backups")
}

/// Checks whether the SQLite ledger database file exists, propagating permission errors.
pub fn db_exists(db_path: &Path) -> Result<bool, StorageError> {
    Ok(db_path.try_exists()?)
}

/// Generates a time-ordered UUIDv7 string using the supplied `ContextV7` and Unix millisecond timestamp.
pub fn generate_uuid_v7_with(ctx: &ContextV7, ts_ms: i64) -> String {
    let clamped_ms = ts_ms.max(0) as u64;
    let secs = clamped_ms / 1000;
    let nanos = ((clamped_ms % 1000) * 1_000_000) as u32;
    Uuid::new_v7(Timestamp::from_unix(ctx, secs, nanos))
        .hyphenated()
        .to_string()
}

/// Ensures `db_path` and any companion SQLite WAL/SHM files carry `0600` permissions.
pub fn enforce_private_db_permissions(db_path: &Path) -> Result<(), StorageError> {
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let shm_path = PathBuf::from(format!("{}-shm", db_path.display()));
    for candidate in [db_path, wal_path.as_path(), shm_path.as_path()] {
        match fs::set_permissions(candidate, fs::Permissions::from_mode(0o600)) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(StorageError::Io(err)),
        }
    }
    Ok(())
}

/// Creates an automatic WAL-safe `0600` pre-migration backup before schema alterations.
pub fn backup_pre_migration(
    conn: &Connection,
    db_path: &Path,
    user_version: i32,
) -> Result<PathBuf, StorageError> {
    let data_dir = db_path.parent().unwrap_or(Path::new("."));
    let backups_dir = backups_dir_in_data_dir(data_dir);
    if !backups_dir.try_exists().unwrap_or(false) {
        fs::create_dir_all(&backups_dir)?;
        fs::set_permissions(&backups_dir, fs::Permissions::from_mode(0o700))?;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let backup_path = backups_dir.join(format!(
        "bpmute_backup_pre_migration_v{user_version}_{now_ms}.db"
    ));

    let mut dst = Connection::open(&backup_path)?;
    let backup = rusqlite::backup::Backup::new(conn, &mut dst)?;
    backup.run_to_completion(100, std::time::Duration::from_millis(50), None)?;
    drop(backup);
    drop(dst);

    enforce_private_db_permissions(&backup_path)?;
    Ok(backup_path)
}

/// WAL runtime pragmas applied to every read-write SQLite connection (`1 MiB` journal size cap).
pub(crate) const WAL_RUNTIME_PRAGMAS: &str = "PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA journal_size_limit = 1048576;";

/// Initializes a read-write SQLite WAL connection with `secure_delete` and `auto_vacuum = INCREMENTAL`.
/// Creates an automatic pre-migration backup if upgrading an existing older database schema version.
pub fn initialize_rw_connection(
    conn: &Connection,
    db_path: Option<&Path>,
) -> Result<(), StorageError> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;
         PRAGMA secure_delete = ON;",
    )?;

    let user_version: i32 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    if user_version == 0 {
        // Must precede first table creation so SQLite enables incremental auto-vacuuming.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        conn.execute_batch(WAL_RUNTIME_PRAGMAS)?;
        conn.execute_batch(DDL_V1)?;
        conn.execute_batch(&format!(
            "PRAGMA user_version = {CURRENT_DB_SCHEMA_VERSION};"
        ))?;
    } else if user_version < CURRENT_DB_SCHEMA_VERSION {
        if let Some(path) = db_path {
            let _ = backup_pre_migration(conn, path, user_version);
        }
        conn.execute_batch(WAL_RUNTIME_PRAGMAS)?;
        conn.execute_batch(&format!(
            "PRAGMA user_version = {CURRENT_DB_SCHEMA_VERSION};"
        ))?;
    } else if user_version == CURRENT_DB_SCHEMA_VERSION {
        conn.execute_batch(WAL_RUNTIME_PRAGMAS)?;
        let _ = conn.execute(
            "ALTER TABLE wearables ADD COLUMN address_type TEXT NOT NULL DEFAULT 'public';",
            [],
        );
        let _ = conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_wearables_mac_type ON wearables(mac, address_type);",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN pre_candidate_mu_hr REAL;",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN pre_candidate_sigma_hr REAL;",
            [],
        );
        let _ = conn.execute(
            "ALTER TABLE sessions ADD COLUMN pre_candidate_ln_rmssd REAL;",
            [],
        );
    } else {
        return Err(StorageError::Validation(format!(
            "unsupported SQLite ledger schema version {user_version}; expected {CURRENT_DB_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

/// Configures a read-only SQLite connection (`query_only = ON`).
pub fn initialize_ro_connection(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA query_only = ON;",
    )?;
    let user_version: i32 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    if user_version > CURRENT_DB_SCHEMA_VERSION {
        return Err(StorageError::Validation(format!(
            "unsupported SQLite ledger schema version {user_version}; expected {CURRENT_DB_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

const DDL_V1: &str = r#"
CREATE TABLE IF NOT EXISTS wearables (
    id TEXT PRIMARY KEY,
    mac TEXT NOT NULL,
    address_type TEXT NOT NULL DEFAULT 'public',
    alias TEXT,
    advertised_name TEXT,
    supports_rr INTEGER NOT NULL DEFAULT 0,
    supports_contact INTEGER NOT NULL DEFAULT 0,
    hr_var_floor REAL NOT NULL DEFAULT 0.25,
    typical_sqi REAL,
    enrolled_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    UNIQUE (mac, address_type)
);

CREATE TABLE IF NOT EXISTS sessions (
    id TEXT PRIMARY KEY,
    wearable_id TEXT NOT NULL REFERENCES wearables(id),
    status TEXT NOT NULL CHECK (status IN ('active', 'closed')),
    start_ts_ms INTEGER NOT NULL,
    end_ts_ms INTEGER NOT NULL,
    end_reason TEXT,
    detection_sensitivity TEXT NOT NULL,
    had_rr INTEGER NOT NULL DEFAULT 0,
    warmup_mu_hr REAL,
    warmup_sigma_hr REAL,
    warmup_ln_rmssd REAL,
    pre_candidate_mu_hr REAL,
    pre_candidate_sigma_hr REAL,
    pre_candidate_ln_rmssd REAL,
    onset_candidate_ts_ms INTEGER,
    onset_confirmed_ts_ms INTEGER,
    sleeping_ts_ms INTEGER,
    sleep_mu_hr REAL,
    onset_hr_bpm REAL,
    delta_hr_onset REAL,
    sleep_ln_rmssd REAL,
    sleep_hr_var_ratio REAL,
    typical_sqi REAL,
    post_trigger_sleep_sec INTEGER NOT NULL DEFAULT 0,
    trigger_power_action TEXT NOT NULL DEFAULT 'none',
    rapid_resume_within_5m INTEGER NOT NULL DEFAULT 0,
    grace_cancel_count INTEGER NOT NULL DEFAULT 0,
    rapid_wake_fp_count INTEGER NOT NULL DEFAULT 0,
    snooze_count INTEGER NOT NULL DEFAULT 0,
    auto_anomalous INTEGER NOT NULL DEFAULT 0,
    epochs_count INTEGER NOT NULL DEFAULT 0,
    raw_samples_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_sessions_start_ts
    ON sessions(start_ts_ms DESC, id DESC);

CREATE TABLE IF NOT EXISTS session_epochs (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    ts_ms INTEGER NOT NULL,
    hr_bpm_60s REAL NOT NULL,
    z_hr REAL NOT NULL,
    hr_slope_4m REAL NOT NULL,
    r_var_120s REAL NOT NULL,
    ln_rmssd_60s REAL,
    delta_ln_rmssd_60s REAL,
    rmssd_sdnn_120s REAL,
    hrv_valid INTEGER NOT NULL,
    rr_artifact_frac REAL NOT NULL,
    coverage REAL NOT NULL,
    flatline_stale INTEGER NOT NULL,
    accumulators_frozen INTEGER NOT NULL,
    baseline_mu_hr REAL NOT NULL,
    baseline_sigma_hr REAL NOT NULL,
    baseline_frozen INTEGER NOT NULL,
    p_active_wake REAL NOT NULL,
    p_quiet_wake REAL NOT NULL,
    p_sleep REAL NOT NULL,
    state TEXT NOT NULL,
    UNIQUE(session_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_session_epochs_session_ts
    ON session_epochs(session_id, ts_ms);

CREATE TABLE IF NOT EXISTS raw_samples (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    ts_ms INTEGER NOT NULL,
    bpm REAL NOT NULL,
    rr_ms_json TEXT NOT NULL,
    contact_status TEXT NOT NULL,
    UNIQUE(session_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_raw_samples_session_ts
    ON raw_samples(session_id, ts_ms);

CREATE TABLE IF NOT EXISTS session_feedback (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id) ON DELETE CASCADE,
    verdict TEXT,
    manual_onset_ts_ms INTEGER,
    manual_avg_sleep_hr REAL,
    notes TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS calibration_boundaries (
    id TEXT PRIMARY KEY,
    effective_ts_ms INTEGER NOT NULL,
    reason TEXT NOT NULL,
    seed_sleep_hr REAL,
    reset_floors INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    reverted_at_ms INTEGER
);

CREATE TABLE IF NOT EXISTS calibration_meta (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    day1_seed_sleep_hr REAL,
    day1_seed_updated_at_ms INTEGER,
    dismissed_shift_anchor_ts_ms INTEGER,
    dismissed_shift_direction TEXT
);

CREATE TABLE IF NOT EXISTS calibration_state (
    singleton_id INTEGER PRIMARY KEY CHECK (singleton_id = 1),
    algorithm_version INTEGER NOT NULL,
    computed_at_ms INTEGER NOT NULL,
    state_json TEXT NOT NULL
);
"#;
