use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Duration, Local, LocalResult, NaiveDateTime, NaiveTime, TimeZone};
use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

pub use super::retention::{RawSamplesRetention, RetentionConfig, SessionEpochsRetention};

use crate::exit_code::ExitCode;

/// Current configuration schema version.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Errors returned when loading, validating, initializing, or patching configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file already exists at {0} (pass --force to overwrite)")]
    AlreadyExists(PathBuf),
    #[error("I/O error accessing configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML syntax: {0}")]
    Parse(String),
    #[error("invalid configuration value: {0}")]
    Validation(String),
    #[error("invalid environment variable override: {0}")]
    Env(String),
    #[error("invalid log filter directive: {0}")]
    LogFilter(String),
    #[error("failed to patch TOML document: {0}")]
    TomlEdit(String),
    #[error("failed to initialize logging subscriber: {0}")]
    LoggingInit(String),
}

impl ConfigError {
    /// Maps this configuration error to its semantic CLI exit code.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            Self::AlreadyExists(_) => ExitCode::PreconditionConflict,
            Self::Io(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                ExitCode::PermissionOrBackendRefusal
            }
            Self::Io(_) | Self::LoggingInit(_) => ExitCode::RuntimeError,
            Self::Parse(_)
            | Self::Validation(_)
            | Self::Env(_)
            | Self::LogFilter(_)
            | Self::TomlEdit(_) => ExitCode::UsageOrConfigError,
        }
    }
}

/// Top-level declarative configuration persisted in `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    #[serde(default)]
    pub bluetooth: BluetoothConfig,
    #[serde(default)]
    pub detection: DetectionConfig,
    #[serde(default)]
    pub schedule: ScheduleConfig,
    #[serde(default)]
    pub actions: ActionsConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub retention: RetentionConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            bluetooth: BluetoothConfig::default(),
            detection: DetectionConfig::default(),
            schedule: ScheduleConfig::default(),
            actions: ActionsConfig::default(),
            notifications: NotificationsConfig::default(),
            retention: RetentionConfig::default(),
            tui: TuiConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    /// Validates structural bounds and cross-field invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::Validation(format!(
                "unsupported schema_version {}: expected {}",
                self.schema_version, CURRENT_SCHEMA_VERSION
            )));
        }
        self.bluetooth.validate()?;
        self.detection.validate()?;
        self.schedule.validate()?;
        self.actions.validate()?;
        self.retention.validate()?;
        Ok(())
    }
}

/// Bluetooth adapter pinning preferences.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BluetoothConfig {
    pub adapter: Option<String>,
}

impl BluetoothConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(adapter) = &self.adapter
            && adapter.trim().is_empty()
        {
            return Err(ConfigError::Validation(
                "bluetooth.adapter must not be empty when specified".to_string(),
            ));
        }
        Ok(())
    }
}

/// Sleep-onset sensitivity presets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Conservative,
    #[default]
    Balanced,
    Responsive,
}

impl Sensitivity {
    /// Returns the canonical lowercase TOML/JSON representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Conservative => "conservative",
            Self::Balanced => "balanced",
            Self::Responsive => "responsive",
        }
    }
}

impl fmt::Display for Sensitivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Sensitivity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "conservative" => Ok(Self::Conservative),
            "balanced" => Ok(Self::Balanced),
            "responsive" => Ok(Self::Responsive),
            _ => Err(format!(
                "invalid sensitivity '{s}': expected conservative, balanced, or responsive"
            )),
        }
    }
}

/// Optional advanced HMM threshold and dwell overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectionAdvancedConfig {
    pub p_sleep_threshold: Option<f64>,
    pub dwell_bpm_rr_sec: Option<u32>,
    pub dwell_bpm_only_sec: Option<u32>,
}

impl DetectionAdvancedConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(threshold) = self.p_sleep_threshold
            && (!threshold.is_finite() || !(0.50..=0.99).contains(&threshold))
        {
            return Err(ConfigError::Validation(format!(
                "detection.advanced.p_sleep_threshold ({threshold}) must be within 0.50..=0.99"
            )));
        }
        if let Some(dwell) = self.dwell_bpm_rr_sec
            && !(60..=900).contains(&dwell)
        {
            return Err(ConfigError::Validation(format!(
                "detection.advanced.dwell_bpm_rr_sec ({dwell}) must be within 60..=900"
            )));
        }
        if let Some(dwell) = self.dwell_bpm_only_sec
            && !(60..=900).contains(&dwell)
        {
            return Err(ConfigError::Validation(format!(
                "detection.advanced.dwell_bpm_only_sec ({dwell}) must be within 60..=900"
            )));
        }
        Ok(())
    }
}

/// Sleep-onset detection preferences.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectionConfig {
    pub sensitivity: Sensitivity,
    pub advanced: Option<DetectionAdvancedConfig>,
}

impl DetectionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if let Some(advanced) = &self.advanced {
            advanced.validate()?;
        }
        Ok(())
    }
}

/// Local 24-hour `HH:MM` clock time used for active-hours scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockTime {
    hour: u8,
    minute: u8,
}

impl ClockTime {
    /// Creates a new `ClockTime` if `hour < 24` and `minute < 60`.
    pub fn new(hour: u8, minute: u8) -> Result<Self, String> {
        if hour >= 24 || minute >= 60 {
            return Err(format!(
                "invalid HH:MM time '{hour:02}:{minute:02}': expected 00:00..=23:59"
            ));
        }
        Ok(Self { hour, minute })
    }

    /// Returns the hour component (`0..=23`).
    pub fn hour(self) -> u8 {
        self.hour
    }

    /// Returns the minute component (`0..=59`).
    pub fn minute(self) -> u8 {
        self.minute
    }

    /// Converts to `chrono::NaiveTime`.
    pub fn to_naive_time(self) -> NaiveTime {
        NaiveTime::from_hms_opt(u32::from(self.hour), u32::from(self.minute), 0)
            .expect("ClockTime invariants guarantee valid NaiveTime")
    }

    /// Computes the next local-timezone occurrence strictly after `now`.
    pub fn next_occurrence_after(self, now: DateTime<Local>) -> DateTime<Local> {
        let target_time = self.to_naive_time();
        let start_date = if now.time() < target_time {
            now.date_naive()
        } else {
            now.date_naive().succ_opt().unwrap_or(now.date_naive())
        };

        for day_offset in 0..=2 {
            let date = start_date
                .checked_add_days(chrono::Days::new(day_offset))
                .unwrap_or(start_date);
            let naive_dt = NaiveDateTime::new(date, target_time);
            match Local.from_local_datetime(&naive_dt) {
                LocalResult::Single(dt) if dt > now => return dt,
                LocalResult::Ambiguous(earliest, latest) => {
                    if earliest > now {
                        return earliest;
                    }
                    if latest > now {
                        return latest;
                    }
                }
                LocalResult::None => {
                    // Spring-forward DST gap: step forward minute-by-minute to first valid local instant.
                    for mins in 1..=180 {
                        let shifted = naive_dt + Duration::minutes(mins);
                        match Local.from_local_datetime(&shifted) {
                            LocalResult::Single(dt) if dt > now => return dt,
                            LocalResult::Ambiguous(earliest, _) if earliest > now => {
                                return earliest;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        now + Duration::hours(24)
    }
}

impl fmt::Display for ClockTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

impl FromStr for ClockTime {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = s.as_bytes();
        if bytes.len() != 5 || bytes[2] != b':' {
            return Err(format!(
                "invalid time '{s}': expected strict 5-character HH:MM format"
            ));
        }
        if !bytes[0].is_ascii_digit()
            || !bytes[1].is_ascii_digit()
            || !bytes[3].is_ascii_digit()
            || !bytes[4].is_ascii_digit()
        {
            return Err(format!(
                "invalid time '{s}': expected ASCII digits in HH:MM"
            ));
        }
        let hour: u8 = s[0..2]
            .parse()
            .map_err(|_| format!("invalid hour in '{s}'"))?;
        let minute: u8 = s[3..5]
            .parse()
            .map_err(|_| format!("invalid minute in '{s}'"))?;
        Self::new(hour, minute)
    }
}

impl Serialize for ClockTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ClockTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

/// Optional active-hours schedule window for action arming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    pub active_hours_enabled: bool,
    pub start: ClockTime,
    pub end: ClockTime,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            active_hours_enabled: false,
            start: ClockTime::new(22, 0).expect("valid default start"),
            end: ClockTime::new(8, 0).expect("valid default end"),
        }
    }
}

impl ScheduleConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.active_hours_enabled && self.start == self.end {
            return Err(ConfigError::Validation(
                "schedule.start and schedule.end must differ when active_hours_enabled is true"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Returns whether `local_time` falls inside `[start, end)` supporting midnight wraparound.
    pub fn is_within_active_hours(&self, local_time: NaiveTime) -> bool {
        let start = self.start.to_naive_time();
        let end = self.end.to_naive_time();
        if start < end {
            local_time >= start && local_time < end
        } else if start > end {
            local_time >= start || local_time < end
        } else {
            false
        }
    }
}

/// Terminal host power action in the Sleep Action Pipeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    #[default]
    None,
    Suspend,
    Poweroff,
}

impl PowerAction {
    /// Returns the canonical lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Suspend => "suspend",
            Self::Poweroff => "poweroff",
        }
    }
}

impl fmt::Display for PowerAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PowerAction {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Ok(Self::None),
            "suspend" => Ok(Self::Suspend),
            "poweroff" => Ok(Self::Poweroff),
            _ => Err(format!(
                "invalid power_action '{s}': expected none, suspend, or poweroff"
            )),
        }
    }
}

/// Sleep Action Pipeline and Grace Period configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsConfig {
    pub pause_media: bool,
    pub ignore_players: Vec<String>,
    pub custom_command: Vec<String>,
    pub custom_command_timeout_sec: u32,
    pub abort_on_hook_failure: bool,
    pub lock_screen: bool,
    pub power_action: PowerAction,
    pub grace_period_sec: Option<u32>,
    pub snooze_duration_min: u32,
    pub dry_run: bool,
    pub strict_idle_backend: bool,
}

impl Default for ActionsConfig {
    fn default() -> Self {
        Self {
            pause_media: true,
            ignore_players: Vec::new(),
            custom_command: Vec::new(),
            custom_command_timeout_sec: 10,
            abort_on_hook_failure: false,
            lock_screen: false,
            power_action: PowerAction::None,
            grace_period_sec: None,
            snooze_duration_min: 30,
            dry_run: false,
            strict_idle_backend: false,
        }
    }
}

impl ActionsConfig {
    /// Returns the effective Grace Period duration in seconds, resolving tiered safety defaults.
    pub fn effective_grace_period_sec(&self) -> u32 {
        self.grace_period_sec.unwrap_or(match self.power_action {
            PowerAction::None => 30,
            PowerAction::Suspend => 60,
            PowerAction::Poweroff => 90,
        })
    }

    /// Returns the ordered Sleep Action Pipeline step labels configured for execution.
    pub fn planned_pipeline_steps(&self) -> Vec<String> {
        let mut steps = Vec::new();
        if self.pause_media {
            steps.push("Pause Media".to_string());
        }
        if self
            .custom_command
            .first()
            .is_some_and(|cmd| !cmd.trim().is_empty())
        {
            steps.push("Custom Hook".to_string());
        }
        if self.lock_screen {
            steps.push("Lock Screen".to_string());
        }
        match self.power_action {
            PowerAction::Suspend => steps.push("Suspend".to_string()),
            PowerAction::Poweroff => steps.push("Power Off".to_string()),
            PowerAction::None => {}
        }
        if steps.is_empty() {
            steps.push("Notify Only".to_string());
        }
        steps
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.ignore_players.iter().any(|p| p.trim().is_empty()) {
            return Err(ConfigError::Validation(
                "actions.ignore_players entries must not be empty".to_string(),
            ));
        }
        if let Some(executable) = self.custom_command.first()
            && executable.trim().is_empty()
        {
            return Err(ConfigError::Validation(
                "actions.custom_command[0] must not be empty".to_string(),
            ));
        }
        if !(1..=60).contains(&self.custom_command_timeout_sec) {
            return Err(ConfigError::Validation(format!(
                "actions.custom_command_timeout_sec ({}) must be within 1..=60",
                self.custom_command_timeout_sec
            )));
        }
        if let Some(grace) = self.grace_period_sec
            && !(10..=300).contains(&grace)
        {
            return Err(ConfigError::Validation(format!(
                "actions.grace_period_sec ({grace}) must be within 10..=300"
            )));
        }
        if !(5..=240).contains(&self.snooze_duration_min) {
            return Err(ConfigError::Validation(format!(
                "actions.snooze_duration_min ({}) must be within 5..=240",
                self.snooze_duration_min
            )));
        }
        Ok(())
    }
}

/// Desktop notification settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub connection_lifecycle: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            connection_lifecycle: true,
        }
    }
}

/// Interactive TUI preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    pub mouse: bool,
    pub ascii: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            mouse: true,
            ascii: false,
        }
    }
}

/// Declarative default logging level.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Returns the lowercase level directive string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(format!(
                "invalid log level '{s}': expected error, warn, info, debug, or trace"
            )),
        }
    }
}

/// Structured output format for daemon and CLI logs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Auto,
    Pretty,
    Json,
    Journald,
}

impl LogFormat {
    /// Returns the canonical lowercase format name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Pretty => "pretty",
            Self::Json => "json",
            Self::Journald => "journald",
        }
    }
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            "journald" => Ok(Self::Journald),
            _ => Err(format!(
                "invalid log format '{s}': expected auto, pretty, json, or journald"
            )),
        }
    }
}

/// Logging configuration block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub level: LogLevel,
    pub format: LogFormat,
}
