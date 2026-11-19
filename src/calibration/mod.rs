//! Pure-synchronous, deterministic chronological calibration-ledger recomputation.
//!
//! Zero I/O, zero SQLite, zero `tokio`, and zero wall-clock calls (ADR-0005, ADR-0007).

pub mod recompute;
pub mod types;

pub use recompute::{CalibrationFolder, recompute_calibration};
pub use types::{
    BoundaryReason, CALIBRATION_ALGORITHM_VERSION, CalibrationBoundaryRecord, CalibrationState,
    DismissedShift, FeedbackVerdict, LedgerRecomputeInput, PersistentShiftAlert,
    SessionFeedbackRecord, SessionLedgerRecord, SessionReplayInput, ShiftDirection,
    WearableCalibration, WearableSeedMeta, validate_sleep_hr_bpm,
};
