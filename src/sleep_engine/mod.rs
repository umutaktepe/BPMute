//! Pure-synchronous, zero-`tokio`, clock-injected sleep-onset detection engine.
//!
//! Driven strictly by explicit timestamped [`EngineEvent`] values with zero I/O
//! or wall-clock calls (ADR-0004, ADR-0007).

pub mod baseline;
pub mod hmm;
pub mod ppg;
pub mod state_machine;
pub mod types;

pub use state_machine::SleepEngine;
pub use types::{
    CalibrationMaturity, CalibrationPriors, ContactStatus, DesktopActivity, DesktopActivityKind,
    DetectorState, EngineConfig, EngineEvent, EngineOutput, Epoch, EpochFeatures, GapTier,
    GraceAbortReason, GraceOutcome, GraceResult, HrPacket, PresetParams, SessionEndReason,
    SessionSummary, StreamState, StreamStatus, WearableFloor,
};
