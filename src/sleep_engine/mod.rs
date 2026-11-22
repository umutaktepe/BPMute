//! Pure-synchronous, zero-`tokio`, clock-injected sleep-onset detection engine.
//!
//! Driven strictly by explicit timestamped [`EngineEvent`] values with zero I/O
//! or wall-clock calls (ADR-0004, ADR-0007).

//! Pure-synchronous, zero-`tokio`, clock-injected sleep-onset detection engine.
//!
//! Driven strictly by explicit timestamped [`EngineEvent`] values with zero I/O
//! or wall-clock calls (ADR-0004, ADR-0007).
//!
//! Subsystem structure:
//! - [`types`]: Input events, `CalibrationPriors`, `Epoch` / `EpochFeatures`, and [`EngineOutput`].
//! - [`ppg`]: Online median/MAD filters, OLS HR trend (`hr_slope_4m`), HRV metrics, and quantile helpers.
//! - [`baseline`]: `Quiet-Wake Baseline` estimator, warmup acceptance, freeze/re-anchor, and anomaly gate.
//! - [`hmm`]: Forward HMM posteriors (`p_active_wake`, `p_quiet_wake`, `p_sleep`) and dwell scoring.
//! - [`state_machine`]: [`SleepEngine`] phase transitions, gap-tier handling, and grace-period lifecycle.
//!
//! Verification harnesses: Tier 1 regression scenarios in `tests/sleep_engine.rs`
//! and Tier 2 seeded 200-session Monte Carlo gates in `tests/monte_carlo.rs`.

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
    SessionSummary, StreamDisconnectReason, StreamDisconnected, StreamState, StreamStatus,
    WearableFloor,
};
