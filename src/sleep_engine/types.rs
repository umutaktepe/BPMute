//! Core input, configuration, calibration prior, and output types for [`crate::sleep_engine`].

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::config::{DetectionConfig, Sensitivity};

/// BLE Heart Rate Measurement (`0x2A37`) sensor-contact status bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContactStatus {
    /// Contact detection unsupported (`0b00` or `0b01`).
    #[default]
    Unsupported,
    /// Contact detection supported, skin contact NOT detected (`0b10`).
    NoContact,
    /// Contact detection supported, skin contact detected (`0b11`).
    Contact,
}

impl ContactStatus {
    /// Returns the canonical lowercase identifier stored in SQLite and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::NoContact => "no_contact",
            Self::Contact => "contact",
        }
    }
}

impl fmt::Display for ContactStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ContactStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "unsupported" => Ok(Self::Unsupported),
            "no_contact" => Ok(Self::NoContact),
            "contact" => Ok(Self::Contact),
            other => Err(format!("unknown contact_status `{other}`")),
        }
    }
}

/// Incoming heart-rate packet with explicit millisecond timestamp.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HrPacket {
    pub ts_ms: i64,
    pub bpm: f64,
    #[serde(default)]
    pub rr_ms: Vec<f64>,
    #[serde(default)]
    pub contact_status: ContactStatus,
}

/// Wearable BLE stream connection/acquisition state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamStatus {
    Disconnected,
    Connecting,
    Acquiring,
    Streaming,
    SignalLost,
}

/// Explicit Wearable stream status transition event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamState {
    pub ts_ms: i64,
    pub status: StreamStatus,
}

/// Desktop user activity classification observed outside the pure engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopActivityKind {
    UserInput,
    MediaUnpaused,
}

/// Timestamped desktop activity hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesktopActivity {
    pub ts_ms: i64,
    pub kind: DesktopActivityKind,
}

/// Outcome of an active `Grace Period` or immediate post-trigger watch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraceResult {
    Expired,
    Cancelled,
    Snoozed { duration_sec: u32 },
    AbortedNoPenalty,
    PostTriggerRapidWake,
}

/// Timestamped `Grace Period` outcome event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraceOutcome {
    pub ts_ms: i64,
    pub outcome: GraceResult,
}

/// Explicit timestamped input event driving [`crate::sleep_engine::SleepEngine`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineEvent {
    HrPacket(HrPacket),
    StreamState(StreamState),
    DesktopActivity(DesktopActivity),
    GraceOutcome(GraceOutcome),
}

impl EngineEvent {
    /// Returns the explicit event timestamp in milliseconds.
    pub fn ts_ms(&self) -> i64 {
        match self {
            Self::HrPacket(ev) => ev.ts_ms,
            Self::StreamState(ev) => ev.ts_ms,
            Self::DesktopActivity(ev) => ev.ts_ms,
            Self::GraceOutcome(ev) => ev.ts_ms,
        }
    }
}

/// Personal `Calibration Maturity` tier (GLOSSARY.md / ADR-0005).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalibrationMaturity {
    #[default]
    ColdStart,
    Learning,
    Calibrated,
}

/// Personal physiological calibration priors shared across Wearables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPriors {
    pub maturity: CalibrationMaturity,
    pub mu_wake: Option<f64>,
    #[serde(default)]
    pub wake_median: Option<f64>,
    pub sigma_wake: Option<f64>,
    pub mu_sleep: Option<f64>,
    pub delta_hr_onset: Option<f64>,
    pub mu_ln_rmssd_sleep: Option<f64>,
    pub tau_p_sleep_offset: f64,
    pub dwell_adjust_sec: i32,
    pub wake_mad_s: Option<f64>,
}

impl CalibrationPriors {
    /// Returns uncalibrated `Cold Start` priors relying purely on within-session relative shifts.
    pub fn cold_start() -> Self {
        Self {
            maturity: CalibrationMaturity::ColdStart,
            mu_wake: None,
            wake_median: None,
            sigma_wake: None,
            mu_sleep: None,
            delta_hr_onset: None,
            mu_ln_rmssd_sleep: None,
            tau_p_sleep_offset: 0.0,
            dwell_adjust_sec: 0,
            wake_mad_s: None,
        }
    }
}

impl Default for CalibrationPriors {
    fn default() -> Self {
        Self::cold_start()
    }
}

/// Per-Wearable rolling HR variance ratio floor (ADR-0004, ADR-0005).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WearableFloor {
    pub hr_var_floor: f64,
}

impl WearableFloor {
    /// Default non-zero `R_var` floor preventing false flatline/zero-variance artifacts.
    pub const DEFAULT_HR_VAR_FLOOR: f64 = 0.25;

    /// Returns the floor clamped into `[0.05, 0.80]`.
    pub fn clamped(self) -> f64 {
        if self.hr_var_floor.is_finite() {
            self.hr_var_floor.clamp(0.05, 0.80)
        } else {
            Self::DEFAULT_HR_VAR_FLOOR
        }
    }
}

impl Default for WearableFloor {
    fn default() -> Self {
        Self {
            hr_var_floor: Self::DEFAULT_HR_VAR_FLOOR,
        }
    }
}

/// Resolved HMM entry threshold, confirmation dwell, and reset hysteresis parameters.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PresetParams {
    pub p_sleep_threshold: f64,
    pub dwell_bpm_rr_sec: u32,
    pub dwell_bpm_only_sec: u32,
    pub reset_threshold: f64,
}

impl PresetParams {
    /// Resolves effective detection thresholds from user sensitivity preset, optional advanced
    /// overrides, and calibrated personal offset adjustments.
    pub fn resolve(detection: &DetectionConfig, priors: &CalibrationPriors) -> Self {
        let (base_p, base_bpm_rr, base_bpm_only) = match detection.sensitivity {
            Sensitivity::Balanced => (0.85, 180_u32, 300_u32),
            Sensitivity::Conservative => (0.90, 300_u32, 420_u32),
            Sensitivity::Responsive => (0.78, 120_u32, 240_u32),
        };

        let adv = detection.advanced.as_ref();

        let p_sleep_threshold = adv
            .and_then(|a| a.p_sleep_threshold)
            .unwrap_or_else(|| {
                let offset = if priors.tau_p_sleep_offset.is_finite() {
                    priors.tau_p_sleep_offset
                } else {
                    0.0
                };
                base_p + offset
            })
            .clamp(0.50, 0.99);

        let dwell_bpm_rr_sec = adv
            .and_then(|a| a.dwell_bpm_rr_sec)
            .unwrap_or_else(|| {
                let adjusted = base_bpm_rr as i32 + priors.dwell_adjust_sec;
                adjusted.clamp(60, 900) as u32
            })
            .clamp(60, 900);

        let dwell_bpm_only_sec = adv
            .and_then(|a| a.dwell_bpm_only_sec)
            .unwrap_or_else(|| {
                let adjusted = base_bpm_only as i32 + priors.dwell_adjust_sec;
                adjusted.clamp(60, 900) as u32
            })
            .clamp(60, 900);

        Self {
            p_sleep_threshold,
            dwell_bpm_rr_sec,
            dwell_bpm_only_sec,
            reset_threshold: 0.50,
        }
    }
}

/// Top-level configuration passed to [`crate::sleep_engine::SleepEngine`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EngineConfig {
    pub detection: DetectionConfig,
    pub priors: CalibrationPriors,
    pub wearable_floor: WearableFloor,
}

/// Deterministic outer state machine states (ADR-0004).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorState {
    #[default]
    Idle,
    Warmup,
    Monitoring,
    Candidate,
    Confirmed,
    Sleeping,
}

impl DetectorState {
    /// Returns the canonical lowercase state identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Warmup => "warmup",
            Self::Monitoring => "monitoring",
            Self::Candidate => "candidate",
            Self::Confirmed => "confirmed",
            Self::Sleeping => "sleeping",
        }
    }
}

impl fmt::Display for DetectorState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DetectorState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "idle" => Ok(Self::Idle),
            "warmup" => Ok(Self::Warmup),
            "monitoring" => Ok(Self::Monitoring),
            "candidate" => Ok(Self::Candidate),
            "confirmed" => Ok(Self::Confirmed),
            "sleeping" => Ok(Self::Sleeping),
            other => Err(format!("unknown detector_state `{other}`")),
        }
    }
}

/// Tiered `Signal Lost` / BLE disconnect gap classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapTier {
    /// `≤ 2 min`: pause HMM and dwell timer in place.
    ShortPause,
    /// `2–15 min`: stay in same `Monitoring Session`, reset `Candidate`, run `2 min` stabilization.
    MediumStabilize,
    /// `> 15 min`: close `Monitoring Session`.
    LongSessionClose,
}

/// Reason a `Monitoring Session` closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    GapExceeded,
    SustainedWakefulness,
    DesktopWakeRebound,
    StreamEnded,
    HostSuspend,
    HostPoweroff,
}

impl SessionEndReason {
    /// Returns the canonical lowercase snake_case identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GapExceeded => "gap_exceeded",
            Self::SustainedWakefulness => "sustained_wakefulness",
            Self::DesktopWakeRebound => "desktop_wake_rebound",
            Self::StreamEnded => "stream_ended",
            Self::HostSuspend => "host_suspend",
            Self::HostPoweroff => "host_poweroff",
        }
    }
}

impl fmt::Display for SessionEndReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SessionEndReason {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gap_exceeded" => Ok(Self::GapExceeded),
            "sustained_wakefulness" => Ok(Self::SustainedWakefulness),
            "desktop_wake_rebound" => Ok(Self::DesktopWakeRebound),
            "stream_ended" => Ok(Self::StreamEnded),
            "host_suspend" => Ok(Self::HostSuspend),
            "host_poweroff" => Ok(Self::HostPoweroff),
            _ => Err(format!("invalid session end reason '{s}'")),
        }
    }
}

/// Reason the engine requested aborting an active `Grace Period`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraceAbortReason {
    PhysiologicalArousal,
}

/// Extracted 15-second `Epoch` feature vector.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpochFeatures {
    pub hr_bpm_60s: f64,
    pub z_hr: f64,
    pub hr_slope_4m: f64,
    pub r_var_120s: f64,
    pub ln_rmssd_60s: Option<f64>,
    pub delta_ln_rmssd_60s: Option<f64>,
    pub rmssd_sdnn_120s: Option<f64>,
}

/// Emitted 15-second `Epoch` snapshot with filter diagnostics, baseline state, and HMM posteriors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Epoch {
    pub session_index: u64,
    pub ts_ms: i64,
    pub features: EpochFeatures,
    pub hrv_valid: bool,
    pub rr_artifact_frac: f64,
    pub coverage: f64,
    pub flatline_stale: bool,
    pub accumulators_frozen: bool,
    pub baseline_mu_hr: f64,
    pub baseline_sigma_hr: f64,
    pub baseline_frozen: bool,
    pub p_active_wake: f64,
    pub p_quiet_wake: f64,
    pub p_sleep: f64,
    pub state: DetectorState,
}

/// Summary emitted when a `Monitoring Session` closes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_index: u64,
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub end_reason: SessionEndReason,
    pub candidate_ts_ms: Option<i64>,
    pub confirmed_ts_ms: Option<i64>,
    pub sleeping_ts_ms: Option<i64>,
    pub anomalous: bool,
    pub fp_count: u8,
    pub warmup_mu_hr: Option<f64>,
    #[serde(default)]
    pub warmup_sigma_hr: Option<f64>,
    #[serde(default)]
    pub warmup_ln_rmssd: Option<f64>,
    #[serde(default)]
    pub onset_hr_bpm: Option<f64>,
    #[serde(default)]
    pub sleep_mu_hr: Option<f64>,
    #[serde(default)]
    pub sleep_ln_rmssd: Option<f64>,
    #[serde(default)]
    pub observed_hr_var_ratio: Option<f64>,
    #[serde(default)]
    pub typical_sqi: Option<f64>,
    #[serde(default)]
    pub post_trigger_sleep_sec: u32,
    #[serde(default)]
    pub had_rr: bool,
    #[serde(default)]
    pub grace_cancel_count: u8,
    #[serde(default)]
    pub rapid_wake_fp_count: u8,
    #[serde(default)]
    pub snooze_count: u8,
    pub epochs_count: usize,
}

/// Output event emitted synchronously by [`crate::sleep_engine::SleepEngine`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineOutput {
    SessionStarted {
        session_index: u64,
        ts_ms: i64,
    },
    EpochEmitted(Epoch),
    StateChanged {
        session_index: u64,
        ts_ms: i64,
        from: DetectorState,
        to: DetectorState,
        p_sleep: f64,
        reason: String,
    },
    GraceAbortRequested {
        session_index: u64,
        ts_ms: i64,
        reason: GraceAbortReason,
    },
    FalsePositiveRecorded {
        session_index: u64,
        ts_ms: i64,
        counted: bool,
    },
    GapResolved {
        session_index: u64,
        ts_ms: i64,
        tier: GapTier,
        duration_ms: i64,
    },
    SessionEnded(SessionSummary),
}
