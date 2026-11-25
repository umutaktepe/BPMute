//! SQLite WAL schema initialization (`v2`), `v1 -> v2` migration, `0600` permissions, and UUIDv7 generation.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use uuid::{ContextV7, Timestamp, Uuid};

use crate::sleep_engine::{ContactStatus, HrPacket};
use crate::storage::StorageError;
use crate::storage::raw_codec::{RAW_CHUNK_MAX_SAMPLES, encode_raw_chunk_with_bounds};

/// Current SQLite ledger schema version tracked via `PRAGMA user_version`.
pub const CURRENT_DB_SCHEMA_VERSION: i32 = 2;

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
        conn.execute_batch(DDL_V2)?;
        conn.execute_batch(&format!(
            "PRAGMA user_version = {CURRENT_DB_SCHEMA_VERSION};"
        ))?;
    } else if user_version == 1 {
        if let Some(path) = db_path {
            backup_pre_migration(conn, path, user_version)?;
        }
        migrate_v1_to_v2(conn)?;
    } else if user_version == CURRENT_DB_SCHEMA_VERSION {
        conn.execute_batch(WAL_RUNTIME_PRAGMAS)?;
    } else {
        return Err(StorageError::Validation(format!(
            "unsupported SQLite ledger schema version {user_version}; expected {CURRENT_DB_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

/// Configures a read-only SQLite connection (`query_only = ON`) and verifies schema `v2`.
pub fn initialize_ro_connection(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA query_only = ON;",
    )?;
    let user_version: i32 = conn.query_row("PRAGMA user_version;", [], |row| row.get(0))?;
    if user_version < CURRENT_DB_SCHEMA_VERSION {
        return Err(StorageError::Validation(format!(
            "SQLite ledger schema version {user_version} requires migration to v{CURRENT_DB_SCHEMA_VERSION}; run a read-write command (such as `bpmute daemon`) to upgrade before opening read-only"
        )));
    }
    if user_version > CURRENT_DB_SCHEMA_VERSION {
        return Err(StorageError::Validation(format!(
            "unsupported SQLite ledger schema version {user_version}; expected {CURRENT_DB_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, StorageError> {
    let sql = format!("PRAGMA table_info({table});");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_v1_additive_columns(conn: &Connection) -> Result<(), StorageError> {
    if !table_has_column(conn, "wearables", "address_type")? {
        conn.execute(
            "ALTER TABLE wearables ADD COLUMN address_type TEXT NOT NULL DEFAULT 'public';",
            [],
        )?;
        conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_wearables_mac_type ON wearables(mac, address_type);",
            [],
        )?;
    }
    for col in [
        "pre_candidate_mu_hr",
        "pre_candidate_sigma_hr",
        "pre_candidate_ln_rmssd",
    ] {
        if !table_has_column(conn, "sessions", col)? {
            conn.execute(&format!("ALTER TABLE sessions ADD COLUMN {col} REAL;"), [])?;
        }
    }
    Ok(())
}

fn migrate_v1_to_v2(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(WAL_RUNTIME_PRAGMAS)?;
    ensure_v1_additive_columns(conn)?;

    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let migration_result: Result<(), StorageError> = (|| {
        let tx = conn.unchecked_transaction()?;

        tx.execute_batch(
            "DROP INDEX IF EXISTS idx_sessions_start_ts;
             DROP INDEX IF EXISTS idx_session_epochs_session_ts;
             DROP INDEX IF EXISTS idx_raw_samples_session_ts;
             ALTER TABLE sessions RENAME TO _v1_sessions;
             ALTER TABLE session_epochs RENAME TO _v1_session_epochs;
             ALTER TABLE raw_samples RENAME TO _v1_raw_samples;
             ALTER TABLE session_feedback RENAME TO _v1_session_feedback;",
        )?;

        tx.execute_batch(DDL_V2)?;

        tx.execute_batch(
            "INSERT INTO sessions (
                 id, wearable_id, status, start_ts_ms, end_ts_ms, end_reason,
                 detection_sensitivity, had_rr, warmup_mu_hr, warmup_sigma_hr,
                 warmup_ln_rmssd, pre_candidate_mu_hr, pre_candidate_sigma_hr,
                 pre_candidate_ln_rmssd, onset_candidate_ts_ms, onset_confirmed_ts_ms,
                 sleeping_ts_ms, sleep_mu_hr, onset_hr_bpm, delta_hr_onset,
                 sleep_ln_rmssd, sleep_hr_var_ratio, typical_sqi, post_trigger_sleep_sec,
                 trigger_power_action, rapid_resume_within_5m, grace_cancel_count,
                 rapid_wake_fp_count, snooze_count, auto_anomalous, epochs_count, raw_samples_count
             )
             SELECT
                 id, wearable_id, status, start_ts_ms, end_ts_ms, end_reason,
                 detection_sensitivity, had_rr, warmup_mu_hr, warmup_sigma_hr,
                 warmup_ln_rmssd, pre_candidate_mu_hr, pre_candidate_sigma_hr,
                 pre_candidate_ln_rmssd, onset_candidate_ts_ms, onset_confirmed_ts_ms,
                 sleeping_ts_ms, sleep_mu_hr, onset_hr_bpm, delta_hr_onset,
                 sleep_ln_rmssd, sleep_hr_var_ratio, typical_sqi, post_trigger_sleep_sec,
                 trigger_power_action, rapid_resume_within_5m, grace_cancel_count,
                 rapid_wake_fp_count, snooze_count, auto_anomalous, epochs_count, raw_samples_count
             FROM _v1_sessions
             ORDER BY start_ts_ms ASC, id ASC;

             INSERT INTO session_epochs (
                 session_pk, seq, ts_ms, hr_bpm_60s, z_hr, hr_slope_4m, r_var_120s,
                 ln_rmssd_60s, delta_ln_rmssd_60s, rmssd_sdnn_120s, hrv_valid,
                 rr_artifact_frac, coverage, flatline_stale, accumulators_frozen,
                 baseline_mu_hr, baseline_sigma_hr, baseline_frozen,
                 p_active_wake, p_quiet_wake, p_sleep, state
             )
             SELECT
                 s.pk, e.seq, e.ts_ms, e.hr_bpm_60s, e.z_hr, e.hr_slope_4m, e.r_var_120s,
                 e.ln_rmssd_60s, e.delta_ln_rmssd_60s, e.rmssd_sdnn_120s, e.hrv_valid,
                 e.rr_artifact_frac, e.coverage, e.flatline_stale, e.accumulators_frozen,
                 e.baseline_mu_hr, e.baseline_sigma_hr, e.baseline_frozen,
                 e.p_active_wake, e.p_quiet_wake, e.p_sleep, e.state
             FROM _v1_session_epochs e
             JOIN sessions s ON s.id = e.session_id
             ORDER BY s.pk ASC, e.seq ASC;

             INSERT INTO session_feedback (
                 id, session_id, verdict, manual_onset_ts_ms, manual_avg_sleep_hr, notes, updated_at_ms
             )
             SELECT id, session_id, verdict, manual_onset_ts_ms, manual_avg_sleep_hr, notes, updated_at_ms
             FROM _v1_session_feedback;",
        )?;

        pack_v1_raw_samples_into_v2_chunks(&tx)?;

        tx.execute_batch(
            "UPDATE sessions
             SET epochs_count = (SELECT COUNT(*) FROM session_epochs WHERE session_pk = sessions.pk),
                 raw_samples_count = COALESCE((SELECT SUM(sample_count) FROM raw_sample_chunks WHERE session_pk = sessions.pk), 0);

             DROP TABLE _v1_raw_samples;
             DROP TABLE _v1_session_epochs;
             DROP TABLE _v1_session_feedback;
             DROP TABLE _v1_sessions;",
        )?;
        tx.execute_batch(&format!(
            "PRAGMA user_version = {CURRENT_DB_SCHEMA_VERSION};"
        ))?;
        tx.commit()?;
        Ok(())
    })();

    if let Err(err) = migration_result {
        let _ = conn.execute_batch("PRAGMA foreign_keys = ON;");
        return Err(err);
    }

    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    let fk_violation: Option<String> = {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check;")?;
        let mut rows = stmt.query([])?;
        rows.next()?.map(|r| r.get::<_, String>(0)).transpose()?
    };
    if let Some(table) = fk_violation {
        return Err(StorageError::Validation(format!(
            "foreign key integrity violation in `{table}` after v1 -> v2 migration"
        )));
    }

    vacuum_and_truncate_wal(conn)?;
    Ok(())
}

/// Switches to `auto_vacuum = INCREMENTAL`, runs `VACUUM`, restores WAL pragmas, and truncates WAL.
pub(crate) fn vacuum_and_truncate_wal(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(&format!(
        "PRAGMA secure_delete = ON;
         PRAGMA auto_vacuum = INCREMENTAL;
         VACUUM;
         {WAL_RUNTIME_PRAGMAS}
         PRAGMA foreign_keys = ON;
         PRAGMA wal_checkpoint(TRUNCATE);"
    ))?;
    Ok(())
}

fn pack_v1_raw_samples_into_v2_chunks(conn: &Connection) -> Result<(), StorageError> {
    let mut select_stmt = conn.prepare(
        "SELECT s.pk, r.seq, r.ts_ms, r.bpm, r.rr_ms_json, r.contact_status
         FROM _v1_raw_samples r
         JOIN sessions s ON s.id = r.session_id
         ORDER BY s.pk ASC, r.seq ASC",
    )?;
    let mut insert_stmt = conn.prepare(
        "INSERT INTO raw_sample_chunks (
             session_pk, seq, start_ts_ms, end_ts_ms, sample_count, payload_blob
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;

    let mut current_session_pk: Option<i64> = None;
    let mut chunk_first_seq: i64 = 0;
    let mut chunk_packets: Vec<HrPacket> = Vec::with_capacity(RAW_CHUNK_MAX_SAMPLES);

    let mut rows = select_stmt.query([])?;
    while let Some(row) = rows.next()? {
        let session_pk: i64 = row.get(0)?;
        let seq: i64 = row.get(1)?;
        let ts_ms: i64 = row.get(2)?;
        let bpm: f64 = row.get(3)?;
        let rr_json: String = row.get(4)?;
        let contact_str: String = row.get(5)?;
        let rr_ms: Vec<f64> = serde_json::from_str(&rr_json)?;
        let contact_status = contact_str.parse().unwrap_or(ContactStatus::Unsupported);

        let should_flush = match current_session_pk {
            Some(active_pk) => {
                active_pk != session_pk || chunk_packets.len() >= RAW_CHUNK_MAX_SAMPLES
            }
            None => false,
        };
        if should_flush {
            let active_pk = current_session_pk.expect("active session_pk present when flushing");
            flush_migration_chunk(
                &mut insert_stmt,
                active_pk,
                chunk_first_seq,
                &mut chunk_packets,
            )?;
        }

        if current_session_pk != Some(session_pk) || chunk_packets.is_empty() {
            current_session_pk = Some(session_pk);
            chunk_first_seq = seq;
        }
        chunk_packets.push(HrPacket {
            ts_ms,
            bpm,
            rr_ms,
            contact_status,
        });
    }

    if let Some(active_pk) = current_session_pk
        && !chunk_packets.is_empty()
    {
        flush_migration_chunk(
            &mut insert_stmt,
            active_pk,
            chunk_first_seq,
            &mut chunk_packets,
        )?;
    }

    Ok(())
}

fn flush_migration_chunk(
    insert_stmt: &mut rusqlite::Statement<'_>,
    session_pk: i64,
    chunk_seq: i64,
    packets: &mut Vec<HrPacket>,
) -> Result<(), StorageError> {
    let encoded = encode_raw_chunk_with_bounds(packets)?;
    insert_stmt.execute(params![
        session_pk,
        chunk_seq,
        encoded.start_ts_ms,
        encoded.end_ts_ms,
        encoded.sample_count,
        encoded.payload_blob,
    ])?;
    packets.clear();
    Ok(())
}

const DDL_V2: &str = r#"
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

-- Note: Column projection order is synchronized with `SESSION_LEDGER_COLUMNS_COUNT`
-- and `map_session_ledger_row` in `src/storage/store/session.rs`.
CREATE TABLE IF NOT EXISTS sessions (
    pk INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
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
    session_pk INTEGER NOT NULL REFERENCES sessions(pk) ON DELETE CASCADE,
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
    PRIMARY KEY (session_pk, seq)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS raw_sample_chunks (
    session_pk INTEGER NOT NULL REFERENCES sessions(pk) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    start_ts_ms INTEGER NOT NULL,
    end_ts_ms INTEGER NOT NULL,
    sample_count INTEGER NOT NULL,
    payload_blob BLOB NOT NULL,
    PRIMARY KEY (session_pk, seq)
) WITHOUT ROWID;

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
