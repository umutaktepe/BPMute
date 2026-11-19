//! Canonical `CSV` and `JSONL` heart-rate trace parser for read-only `bpmute replay`.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::exit_code::ExitCode;
use crate::sleep_engine::{
    ContactStatus, DesktopActivity, DesktopActivityKind, EngineEvent, GraceOutcome, GraceResult,
    HrPacket, StreamState, StreamStatus,
};

const DEFAULT_SNOOZE_DURATION_SEC: u32 = 1800;

/// Supported canonical trace file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceFormat {
    Csv,
    Jsonl,
}

impl TraceFormat {
    /// Returns the canonical lowercase format identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::Jsonl => "jsonl",
        }
    }
}

/// Errors produced when reading, parsing, or validating a replay trace.
#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("I/O error reading trace: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid trace syntax: {0}")]
    Parse(String),
    #[error("invalid trace data: {0}")]
    Validation(String),
}

impl ReplayError {
    /// Maps this replay error to its semantic CLI exit code (ADR-0009).
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::Io(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                ExitCode::PermissionOrBackendRefusal
            }
            Self::Io(_) => ExitCode::RuntimeError,
            Self::Parse(_) | Self::Validation(_) => ExitCode::UsageOrConfigError,
        }
    }
}

/// Infers trace format from file extension or content peek when extension is unrecognized.
pub fn detect_trace_format(path: &Path, content: &str) -> TraceFormat {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        match ext.to_ascii_lowercase().as_str() {
            "csv" => return TraceFormat::Csv,
            "jsonl" | "ndjson" => return TraceFormat::Jsonl,
            _ => {}
        }
    }
    let first_non_empty = content
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'));
    if first_non_empty.is_some_and(|l| l.starts_with('{')) {
        TraceFormat::Jsonl
    } else {
        TraceFormat::Csv
    }
}

/// Parses canonical `CSV` or `JSONL` trace content into chronological [`EngineEvent`] items.
pub fn parse_trace_str(
    content: &str,
    format: TraceFormat,
) -> Result<Vec<EngineEvent>, ReplayError> {
    let events = match format {
        TraceFormat::Csv => parse_csv_trace(content)?,
        TraceFormat::Jsonl => parse_jsonl_trace(content)?,
    };
    if events.is_empty() {
        return Err(ReplayError::Validation(
            "trace contains zero events".to_string(),
        ));
    }
    Ok(events)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CsvCol {
    TsMs,
    Bpm,
    RrMs,
    ContactStatus,
    Event,
}

fn parse_csv_trace(content: &str) -> Result<Vec<EngineEvent>, ReplayError> {
    let mut lines = content.lines().enumerate().peekable();
    let mut header_cols: Option<Vec<CsvCol>> = None;
    let mut events = Vec::new();
    let mut prev_ts: Option<i64> = None;

    for (line_idx, raw_line) in &mut lines {
        let line_no = line_idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let fields = split_csv_row(raw_line)
            .map_err(|msg| ReplayError::Parse(format!("line {line_no}: {msg}")))?;

        let Some(ref cols) = header_cols else {
            header_cols = Some(parse_csv_header(line_no, &fields)?);
            continue;
        };

        if fields.len() != cols.len() {
            return Err(ReplayError::Parse(format!(
                "line {line_no}: expected {} columns, found {}",
                cols.len(),
                fields.len()
            )));
        }

        let mut ts_str = "";
        let mut bpm_str = "";
        let mut rr_str = "";
        let mut contact_str = "";
        let mut event_str = "";

        for (col, val) in cols.iter().zip(fields.iter()) {
            let v = val.trim();
            match col {
                CsvCol::TsMs => ts_str = v,
                CsvCol::Bpm => bpm_str = v,
                CsvCol::RrMs => rr_str = v,
                CsvCol::ContactStatus => contact_str = v,
                CsvCol::Event => event_str = v,
            }
        }

        let ts_ms: i64 = ts_str.parse().map_err(|_| {
            ReplayError::Validation(format!("line {line_no}: invalid ts_ms '{ts_str}'"))
        })?;
        ensure_monotonic(line_no, ts_ms, &mut prev_ts)?;

        let bpm_opt = if bpm_str.is_empty() {
            None
        } else {
            let v: f64 = bpm_str.parse().map_err(|_| {
                ReplayError::Validation(format!("line {line_no}: invalid bpm '{bpm_str}'"))
            })?;
            if !v.is_finite() {
                return Err(ReplayError::Validation(format!(
                    "line {line_no}: bpm must be finite"
                )));
            }
            Some(v)
        };

        let rr_ms = parse_csv_rr_list(line_no, rr_str)?;
        let contact_status = if contact_str.is_empty() {
            ContactStatus::Unsupported
        } else {
            parse_contact_status(line_no, contact_str)?
        };

        events.push(build_event(
            line_no,
            ts_ms,
            bpm_opt,
            rr_ms,
            contact_status,
            (!event_str.is_empty()).then_some(event_str),
        )?);
    }

    if header_cols.is_none() {
        return Err(ReplayError::Validation(
            "CSV trace is missing header row".to_string(),
        ));
    }

    Ok(events)
}

fn parse_csv_header(line_no: usize, fields: &[String]) -> Result<Vec<CsvCol>, ReplayError> {
    let mut cols = Vec::with_capacity(fields.len());
    let mut seen = HashSet::new();
    for raw in fields {
        let name = raw.trim().to_ascii_lowercase();
        let col = match name.as_str() {
            "ts_ms" => CsvCol::TsMs,
            "bpm" => CsvCol::Bpm,
            "rr_ms" => CsvCol::RrMs,
            "contact_status" => CsvCol::ContactStatus,
            "event" => CsvCol::Event,
            other => {
                return Err(ReplayError::Parse(format!(
                    "line {line_no}: unknown CSV column '{other}'"
                )));
            }
        };
        if !seen.insert(col) {
            return Err(ReplayError::Parse(format!(
                "line {line_no}: duplicate CSV column '{name}'"
            )));
        }
        cols.push(col);
    }
    if !seen.contains(&CsvCol::TsMs) || !seen.contains(&CsvCol::Bpm) {
        return Err(ReplayError::Parse(format!(
            "line {line_no}: CSV header must include 'ts_ms' and 'bpm'"
        )));
    }
    Ok(cols)
}

fn split_csv_row(line: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    let mut field_started_with_quote = false;
    let mut closed_quote = false;

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                    closed_quote = true;
                }
            } else {
                cur.push(ch);
            }
        } else if ch == ',' {
            fields.push(std::mem::take(&mut cur));
            field_started_with_quote = false;
            closed_quote = false;
        } else if ch == '"' && cur.is_empty() && !field_started_with_quote {
            in_quotes = true;
            field_started_with_quote = true;
        } else if closed_quote {
            if !ch.is_whitespace() {
                return Err("unexpected character after closing quote".to_string());
            }
        } else {
            cur.push(ch);
        }
    }

    if in_quotes {
        return Err("unterminated quoted CSV field".to_string());
    }
    fields.push(cur);
    Ok(fields)
}

fn parse_csv_rr_list(line_no: usize, raw: &str) -> Result<Vec<f64>, ReplayError> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let inner = raw
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(raw)
        .trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for token in inner.split([';', ',', ' ']) {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        let val: f64 = t.parse().map_err(|_| {
            ReplayError::Validation(format!("line {line_no}: invalid rr_ms value '{t}'"))
        })?;
        if !val.is_finite() {
            return Err(ReplayError::Validation(format!(
                "line {line_no}: rr_ms value must be finite"
            )));
        }
        out.push(val);
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonRrField {
    Single(f64),
    Many(Vec<f64>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonContactField {
    Code(u8),
    Text(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonlRow {
    ts_ms: i64,
    bpm: Option<f64>,
    rr_ms: Option<JsonRrField>,
    contact_status: Option<JsonContactField>,
    event: Option<String>,
}

fn parse_jsonl_trace(content: &str) -> Result<Vec<EngineEvent>, ReplayError> {
    let mut events = Vec::new();
    let mut prev_ts: Option<i64> = None;

    for (line_idx, raw_line) in content.lines().enumerate() {
        let line_no = line_idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let row: JsonlRow = serde_json::from_str(trimmed).map_err(|e| {
            ReplayError::Parse(format!("line {line_no}: invalid JSONL record: {e}"))
        })?;

        ensure_monotonic(line_no, row.ts_ms, &mut prev_ts)?;

        if let Some(bpm) = row.bpm
            && !bpm.is_finite()
        {
            return Err(ReplayError::Validation(format!(
                "line {line_no}: bpm must be finite"
            )));
        }

        let rr_ms = match row.rr_ms {
            None => Vec::new(),
            Some(JsonRrField::Single(v)) => {
                if !v.is_finite() {
                    return Err(ReplayError::Validation(format!(
                        "line {line_no}: rr_ms must be finite"
                    )));
                }
                vec![v]
            }
            Some(JsonRrField::Many(vs)) => {
                if vs.iter().any(|v| !v.is_finite()) {
                    return Err(ReplayError::Validation(format!(
                        "line {line_no}: all rr_ms values must be finite"
                    )));
                }
                vs
            }
        };

        let contact_status = match row.contact_status {
            None => ContactStatus::Unsupported,
            Some(JsonContactField::Code(code)) => match code {
                0 | 1 => ContactStatus::Unsupported,
                2 => ContactStatus::NoContact,
                3 => ContactStatus::Contact,
                other => {
                    return Err(ReplayError::Validation(format!(
                        "line {line_no}: invalid numeric contact_status '{other}' (expected 0..=3)"
                    )));
                }
            },
            Some(JsonContactField::Text(ref s)) => parse_contact_status(line_no, s)?,
        };

        events.push(build_event(
            line_no,
            row.ts_ms,
            row.bpm,
            rr_ms,
            contact_status,
            row.event.as_deref(),
        )?);
    }

    Ok(events)
}

fn ensure_monotonic(
    line_no: usize,
    ts_ms: i64,
    prev_ts: &mut Option<i64>,
) -> Result<(), ReplayError> {
    if let Some(prev) = *prev_ts
        && ts_ms < prev
    {
        return Err(ReplayError::Validation(format!(
            "line {line_no}: non-monotonic ts_ms {ts_ms} < previous {prev}"
        )));
    }
    *prev_ts = Some(ts_ms);
    Ok(())
}

fn parse_contact_status(line_no: usize, raw: &str) -> Result<ContactStatus, ReplayError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "unsupported" | "0b00" | "0b01" | "0" | "1" | "unknown" => Ok(ContactStatus::Unsupported),
        "no_contact" | "nocontact" | "0b10" | "2" | "false" => Ok(ContactStatus::NoContact),
        "contact" | "0b11" | "3" | "true" => Ok(ContactStatus::Contact),
        other => Err(ReplayError::Validation(format!(
            "line {line_no}: unrecognized contact_status '{other}'"
        ))),
    }
}

fn build_event(
    line_no: usize,
    ts_ms: i64,
    bpm: Option<f64>,
    rr_ms: Vec<f64>,
    contact_status: ContactStatus,
    event_token: Option<&str>,
) -> Result<EngineEvent, ReplayError> {
    let Some(token) = event_token.map(str::trim).filter(|s| !s.is_empty()) else {
        let bpm_val = bpm.ok_or_else(|| {
            ReplayError::Validation(format!("line {line_no}: missing bpm for heart-rate row"))
        })?;
        return Ok(EngineEvent::HrPacket(HrPacket {
            ts_ms,
            bpm: bpm_val,
            rr_ms,
            contact_status,
        }));
    };

    let lower = token.to_ascii_lowercase();
    if let Some(secs_str) = lower
        .strip_prefix("grace_snoozed:")
        .or_else(|| lower.strip_prefix("grace:snoozed:"))
    {
        let duration_sec: u32 = secs_str.parse().map_err(|_| {
            ReplayError::Validation(format!(
                "line {line_no}: invalid snooze duration '{secs_str}'"
            ))
        })?;
        return Ok(EngineEvent::GraceOutcome(GraceOutcome {
            ts_ms,
            outcome: GraceResult::Snoozed { duration_sec },
        }));
    }

    match lower.as_str() {
        "hr" | "hr_packet" => {
            let bpm_val = bpm.ok_or_else(|| {
                ReplayError::Validation(format!("line {line_no}: missing bpm for hr_packet event"))
            })?;
            Ok(EngineEvent::HrPacket(HrPacket {
                ts_ms,
                bpm: bpm_val,
                rr_ms,
                contact_status,
            }))
        }
        "stream_streaming" | "streaming" | "stream:streaming" => {
            Ok(EngineEvent::StreamState(StreamState {
                ts_ms,
                status: StreamStatus::Streaming,
            }))
        }
        "stream_signal_lost" | "signal_lost" | "stream:signal_lost" => {
            Ok(EngineEvent::StreamState(StreamState {
                ts_ms,
                status: StreamStatus::SignalLost,
            }))
        }
        "stream_disconnected" | "disconnected" | "stream:disconnected" => {
            Ok(EngineEvent::StreamState(StreamState {
                ts_ms,
                status: StreamStatus::Disconnected,
            }))
        }
        "stream_acquiring" | "acquiring" | "stream:acquiring" => {
            Ok(EngineEvent::StreamState(StreamState {
                ts_ms,
                status: StreamStatus::Acquiring,
            }))
        }
        "stream_connecting" | "connecting" | "stream:connecting" => {
            Ok(EngineEvent::StreamState(StreamState {
                ts_ms,
                status: StreamStatus::Connecting,
            }))
        }
        "desktop_activity" | "desktop_user_input" | "user_input" | "desktop:user_input" => {
            Ok(EngineEvent::DesktopActivity(DesktopActivity {
                ts_ms,
                kind: DesktopActivityKind::UserInput,
            }))
        }
        "desktop_media_unpaused" | "media_unpaused" | "desktop:media_unpaused" => {
            Ok(EngineEvent::DesktopActivity(DesktopActivity {
                ts_ms,
                kind: DesktopActivityKind::MediaUnpaused,
            }))
        }
        "grace_expired" | "grace:expired" => Ok(EngineEvent::GraceOutcome(GraceOutcome {
            ts_ms,
            outcome: GraceResult::Expired,
        })),
        "grace_cancelled" | "grace_canceled" | "grace:cancelled" | "grace:canceled" => {
            Ok(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::Cancelled,
            }))
        }
        "grace_snoozed" | "grace:snoozed" => Ok(EngineEvent::GraceOutcome(GraceOutcome {
            ts_ms,
            outcome: GraceResult::Snoozed {
                duration_sec: DEFAULT_SNOOZE_DURATION_SEC,
            },
        })),
        "grace_aborted" | "grace_aborted_no_penalty" | "grace:aborted_no_penalty" => {
            Ok(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::AbortedNoPenalty,
            }))
        }
        "grace_rapid_wake" | "post_trigger_rapid_wake" | "grace:post_trigger_rapid_wake" => {
            Ok(EngineEvent::GraceOutcome(GraceOutcome {
                ts_ms,
                outcome: GraceResult::PostTriggerRapidWake,
            }))
        }
        other => Err(ReplayError::Validation(format!(
            "line {line_no}: unrecognized event '{other}'"
        ))),
    }
}
