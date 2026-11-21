//! Session trace export to CSV and JSONL replay formats.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rusqlite::params;

use crate::paths::ensure_private_dir;
use crate::replay::TraceFormat;
use crate::storage::StorageError;

use super::LedgerStore;
use super::session::{load_raw_samples, load_session_epochs, resolve_session_id};

impl LedgerStore {
    /// Exports a session trace to canonical `CSV` or `JSONL` format directly replayable by `bpmute replay`.
    /// Anonymizes by default (stripping Wearable MAC/name and shifting timestamps so `session.start_ts_ms` is `0`).
    pub fn export_trace(
        &self,
        id_or_prefix: &str,
        dest_path: &Path,
        format: TraceFormat,
        anonymize: bool,
    ) -> Result<usize, StorageError> {
        let session_id = resolve_session_id(&self.conn, id_or_prefix)?;
        let start_ts_ms: i64 = self.conn.query_row(
            "SELECT start_ts_ms FROM sessions WHERE id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;

        let raw_samples = load_raw_samples(&self.conn, &session_id)?;
        let epochs = if raw_samples.is_empty() {
            load_session_epochs(&self.conn, &session_id)?
        } else {
            Vec::new()
        };

        if raw_samples.is_empty() && epochs.is_empty() {
            return Err(StorageError::NotFound(format!(
                "no trace data available for session '{session_id}'"
            )));
        }

        if let Some(parent) = dest_path.parent()
            && !parent.as_os_str().is_empty()
        {
            ensure_private_dir(parent)?;
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(dest_path)?;

        let mut count = 0;
        let time_shift = if anonymize { start_ts_ms } else { 0 };

        match format {
            TraceFormat::Csv => {
                writeln!(file, "ts_ms,bpm,rr_ms,contact_status,event")?;
                if !raw_samples.is_empty() {
                    for pkt in &raw_samples {
                        let ts = pkt.ts_ms - time_shift;
                        let rr_str = if pkt.rr_ms.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "\"{}\"",
                                pkt.rr_ms
                                    .iter()
                                    .map(|v| format!("{v:.1}"))
                                    .collect::<Vec<_>>()
                                    .join(";")
                            )
                        };
                        writeln!(
                            file,
                            "{},{:.1},{},{},",
                            ts,
                            pkt.bpm,
                            rr_str,
                            pkt.contact_status.as_str()
                        )?;
                        count += 1;
                    }
                } else {
                    for ep in &epochs {
                        let ts = ep.ts_ms - time_shift;
                        writeln!(file, "{},{:.1},,,", ts, ep.features.hr_bpm_60s)?;
                        count += 1;
                    }
                }
            }
            TraceFormat::Jsonl => {
                if !raw_samples.is_empty() {
                    for pkt in &raw_samples {
                        let ts = pkt.ts_ms - time_shift;
                        let row = serde_json::json!({
                            "ts_ms": ts,
                            "bpm": pkt.bpm,
                            "rr_ms": pkt.rr_ms,
                            "contact_status": pkt.contact_status.as_str(),
                        });
                        serde_json::to_writer(&mut file, &row)?;
                        writeln!(file)?;
                        count += 1;
                    }
                } else {
                    for ep in &epochs {
                        let ts = ep.ts_ms - time_shift;
                        let row = serde_json::json!({
                            "ts_ms": ts,
                            "bpm": ep.features.hr_bpm_60s,
                        });
                        serde_json::to_writer(&mut file, &row)?;
                        writeln!(file)?;
                        count += 1;
                    }
                }
            }
            TraceFormat::SqliteLedger => {
                return Err(StorageError::Validation(
                    "sqlite_ledger format cannot be exported as a trace file".to_string(),
                ));
            }
        }

        let _ = std::fs::set_permissions(dest_path, std::fs::Permissions::from_mode(0o600));
        Ok(count)
    }
}
