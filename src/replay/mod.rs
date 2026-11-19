//! Read-only canonical `CSV` and `JSONL` trace parsing and `bpmute replay` execution.

pub mod runner;
pub mod trace;

pub use runner::{
    ReplayFidelity, ReplayOptions, ReplayReport, ReplaySessionSummary, ReplayTransition,
    replay_events, replay_file, replay_session_epochs, replay_stored_session,
};
pub use trace::{ReplayError, TraceFormat, detect_trace_format, parse_trace_str};
