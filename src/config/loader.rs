use std::fs;
use std::io;
use std::path::Path;
use std::str::FromStr;

use tracing_subscriber::EnvFilter;

use super::schema::{
    Config, ConfigError, DetectionAdvancedConfig, LogFormat, LogLevel, PowerAction,
    RawSamplesRetention, SessionEpochsRetention,
};
use super::template::DEFAULT_CONFIG_TEMPLATE;
use crate::paths::{EnvSource, split_shell_words, write_private_file_atomic};

/// CLI flag overrides applied with highest precedence over env vars and `config.toml`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CliOverrides {
    pub dry_run: Option<bool>,
    pub log_level: Option<String>,
    pub log_format: Option<LogFormat>,
    pub log_biometrics: Option<bool>,
}

/// Validated effective configuration together with resolved logging controls.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub file_present: bool,
    pub log_filter: String,
    pub log_format: LogFormat,
    pub log_biometrics: bool,
}

impl LoadedConfig {
    /// Loads configuration from `path` if present, falling back to built-in defaults
    /// without creating any files or directories, then applies `BPMUTE_*` env vars and CLI flags.
    pub fn load(path: &Path, env: &dyn EnvSource, cli: &CliOverrides) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(raw) => Self::from_toml_str_with_flag(&raw, true, env, cli),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Self::resolve_from_base(Config::default(), false, env, cli)
            }
            Err(err) => Err(ConfigError::Io(err)),
        }
    }

    /// Parses TOML text strictly and applies env and CLI precedence.
    pub fn from_toml_str(
        raw: &str,
        env: &dyn EnvSource,
        cli: &CliOverrides,
    ) -> Result<Self, ConfigError> {
        Self::from_toml_str_with_flag(raw, true, env, cli)
    }

    fn from_toml_str_with_flag(
        raw: &str,
        file_present: bool,
        env: &dyn EnvSource,
        cli: &CliOverrides,
    ) -> Result<Self, ConfigError> {
        let base: Config =
            toml::from_str(raw).map_err(|err| ConfigError::Parse(err.to_string()))?;
        base.validate()?;
        Self::resolve_from_base(base, file_present, env, cli)
    }

    fn resolve_from_base(
        mut config: Config,
        file_present: bool,
        env: &dyn EnvSource,
        cli: &CliOverrides,
    ) -> Result<Self, ConfigError> {
        apply_env_overrides(&mut config, env)?;

        if let Some(dry_run) = cli.dry_run {
            config.actions.dry_run = dry_run;
        }

        config.validate()?;

        let log_filter = resolve_log_filter(&config, env, cli)?;
        let log_format = resolve_log_format(&config, env, cli)?;
        let log_biometrics = resolve_log_biometrics(env, cli)?;

        Ok(Self {
            config,
            file_present,
            log_filter,
            log_format,
            log_biometrics,
        })
    }
}

/// Initializes `config.toml` with `0700`/`0600` permissions using the commented template.
pub fn init_config_file(path: &Path, force: bool) -> Result<(), ConfigError> {
    if path.try_exists().map_err(ConfigError::Io)? && !force {
        return Err(ConfigError::AlreadyExists(path.to_path_buf()));
    }
    write_private_file_atomic(path, DEFAULT_CONFIG_TEMPLATE.as_bytes())?;
    Ok(())
}

fn non_empty_env(env: &dyn EnvSource, key: &str) -> Option<String> {
    env.get(key).filter(|val| !val.trim().is_empty())
}

fn parse_env<T: FromStr<Err = String>>(key: &str, raw: &str) -> Result<T, ConfigError> {
    raw.parse::<T>()
        .map_err(|msg| ConfigError::Env(format!("{key}: {msg}")))
}

fn parse_env_bool(key: &str, raw: &str) -> Result<bool, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::Env(format!(
            "{key}: invalid boolean '{raw}' (expected true/false or 1/0)"
        ))),
    }
}

fn ensure_advanced(config: &mut Config) -> &mut DetectionAdvancedConfig {
    config
        .detection
        .advanced
        .get_or_insert_with(DetectionAdvancedConfig::default)
}

fn apply_env_overrides(config: &mut Config, env: &dyn EnvSource) -> Result<(), ConfigError> {
    if let Some(val) = env.get("BPMUTE_BLUETOOTH_ADAPTER") {
        let trimmed = val.trim();
        config.bluetooth.adapter = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_DETECTION_SENSITIVITY") {
        config.detection.sensitivity = parse_env("BPMUTE_DETECTION_SENSITIVITY", &val)?;
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_DETECTION_ADVANCED_P_SLEEP_THRESHOLD") {
        let parsed: f64 = val.trim().parse().map_err(|_| {
            ConfigError::Env(format!(
                "BPMUTE_DETECTION_ADVANCED_P_SLEEP_THRESHOLD: invalid float '{val}'"
            ))
        })?;
        ensure_advanced(config).p_sleep_threshold = Some(parsed);
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_DETECTION_ADVANCED_DWELL_BPM_RR_SEC") {
        let parsed: u32 = val.trim().parse().map_err(|_| {
            ConfigError::Env(format!(
                "BPMUTE_DETECTION_ADVANCED_DWELL_BPM_RR_SEC: invalid integer '{val}'"
            ))
        })?;
        ensure_advanced(config).dwell_bpm_rr_sec = Some(parsed);
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_DETECTION_ADVANCED_DWELL_BPM_ONLY_SEC") {
        let parsed: u32 = val.trim().parse().map_err(|_| {
            ConfigError::Env(format!(
                "BPMUTE_DETECTION_ADVANCED_DWELL_BPM_ONLY_SEC: invalid integer '{val}'"
            ))
        })?;
        ensure_advanced(config).dwell_bpm_only_sec = Some(parsed);
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_SCHEDULE_ACTIVE_HOURS_ENABLED") {
        config.schedule.active_hours_enabled =
            parse_env_bool("BPMUTE_SCHEDULE_ACTIVE_HOURS_ENABLED", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_SCHEDULE_START") {
        config.schedule.start = parse_env("BPMUTE_SCHEDULE_START", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_SCHEDULE_END") {
        config.schedule.end = parse_env("BPMUTE_SCHEDULE_END", &val)?;
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_PAUSE_MEDIA") {
        config.actions.pause_media = parse_env_bool("BPMUTE_ACTIONS_PAUSE_MEDIA", &val)?;
    }
    if let Some(val) = env.get("BPMUTE_ACTIONS_IGNORE_PLAYERS") {
        let trimmed = val.trim();
        config.actions.ignore_players = if trimmed.is_empty() {
            Vec::new()
        } else if trimmed.starts_with('[') {
            serde_json::from_str::<Vec<String>>(trimmed)
                .map_err(|err| ConfigError::Env(format!("BPMUTE_ACTIONS_IGNORE_PLAYERS: {err}")))?
        } else {
            trimmed
                .split(',')
                .map(|item| item.trim().to_string())
                .collect()
        };
    }
    if let Some(val) = env.get("BPMUTE_ACTIONS_CUSTOM_COMMAND") {
        let trimmed = val.trim();
        config.actions.custom_command = if trimmed.is_empty() {
            Vec::new()
        } else if trimmed.starts_with('[') {
            serde_json::from_str::<Vec<String>>(trimmed)
                .map_err(|err| ConfigError::Env(format!("BPMUTE_ACTIONS_CUSTOM_COMMAND: {err}")))?
        } else {
            split_shell_words(trimmed).ok_or_else(|| {
                ConfigError::Env(
                    "BPMUTE_ACTIONS_CUSTOM_COMMAND: unclosed quote in command string".to_string(),
                )
            })?
        };
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_CUSTOM_COMMAND_TIMEOUT_SEC") {
        config.actions.custom_command_timeout_sec = val.trim().parse().map_err(|_| {
            ConfigError::Env(format!(
                "BPMUTE_ACTIONS_CUSTOM_COMMAND_TIMEOUT_SEC: invalid integer '{val}'"
            ))
        })?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_ABORT_ON_HOOK_FAILURE") {
        config.actions.abort_on_hook_failure =
            parse_env_bool("BPMUTE_ACTIONS_ABORT_ON_HOOK_FAILURE", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_LOCK_SCREEN") {
        config.actions.lock_screen = parse_env_bool("BPMUTE_ACTIONS_LOCK_SCREEN", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_POWER_ACTION") {
        config.actions.power_action =
            parse_env::<PowerAction>("BPMUTE_ACTIONS_POWER_ACTION", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_GRACE_PERIOD_SEC") {
        if val.trim().eq_ignore_ascii_case("none") || val.trim().eq_ignore_ascii_case("default") {
            config.actions.grace_period_sec = None;
        } else {
            let parsed: u32 = val.trim().parse().map_err(|_| {
                ConfigError::Env(format!(
                    "BPMUTE_ACTIONS_GRACE_PERIOD_SEC: invalid integer '{val}'"
                ))
            })?;
            config.actions.grace_period_sec = Some(parsed);
        }
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_SNOOZE_DURATION_MIN") {
        config.actions.snooze_duration_min = val.trim().parse().map_err(|_| {
            ConfigError::Env(format!(
                "BPMUTE_ACTIONS_SNOOZE_DURATION_MIN: invalid integer '{val}'"
            ))
        })?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_DRY_RUN") {
        config.actions.dry_run = parse_env_bool("BPMUTE_ACTIONS_DRY_RUN", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_DRY_RUN") {
        config.actions.dry_run = parse_env_bool("BPMUTE_DRY_RUN", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_ACTIONS_STRICT_IDLE_BACKEND") {
        config.actions.strict_idle_backend =
            parse_env_bool("BPMUTE_ACTIONS_STRICT_IDLE_BACKEND", &val)?;
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_NOTIFICATIONS_CONNECTION_LIFECYCLE") {
        config.notifications.connection_lifecycle =
            parse_env_bool("BPMUTE_NOTIFICATIONS_CONNECTION_LIFECYCLE", &val)?;
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_RETENTION_RAW_SAMPLES_DAYS") {
        config.retention.raw_samples_days =
            parse_env::<RawSamplesRetention>("BPMUTE_RETENTION_RAW_SAMPLES_DAYS", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_RETENTION_SESSION_EPOCHS_DAYS") {
        config.retention.session_epochs_days =
            parse_env::<SessionEpochsRetention>("BPMUTE_RETENTION_SESSION_EPOCHS_DAYS", &val)?;
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_TUI_MOUSE") {
        config.tui.mouse = parse_env_bool("BPMUTE_TUI_MOUSE", &val)?;
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_TUI_ASCII") {
        config.tui.ascii = parse_env_bool("BPMUTE_TUI_ASCII", &val)?;
    }

    if let Some(val) = non_empty_env(env, "BPMUTE_LOGGING_LEVEL") {
        config.logging.level = parse_env::<LogLevel>("BPMUTE_LOGGING_LEVEL", &val)?;
    }

    Ok(())
}

fn resolve_log_filter(
    config: &Config,
    env: &dyn EnvSource,
    cli: &CliOverrides,
) -> Result<String, ConfigError> {
    let filter = if let Some(cli_filter) = &cli.log_level {
        if cli_filter.trim().is_empty() {
            return Err(ConfigError::LogFilter(
                "--log-level must not be empty".to_string(),
            ));
        }
        cli_filter.clone()
    } else if let Some(env_filter) = non_empty_env(env, "BPMUTE_LOG") {
        env_filter
    } else {
        config.logging.level.as_str().to_string()
    };

    EnvFilter::try_new(&filter)
        .map_err(|err| ConfigError::LogFilter(format!("'{filter}': {err}")))?;

    Ok(filter)
}

fn resolve_log_format(
    config: &Config,
    env: &dyn EnvSource,
    cli: &CliOverrides,
) -> Result<LogFormat, ConfigError> {
    if let Some(format) = cli.log_format {
        return Ok(format);
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_LOG_FORMAT") {
        return parse_env::<LogFormat>("BPMUTE_LOG_FORMAT", &val);
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_LOGGING_FORMAT") {
        return parse_env::<LogFormat>("BPMUTE_LOGGING_FORMAT", &val);
    }
    Ok(config.logging.format)
}

fn resolve_log_biometrics(env: &dyn EnvSource, cli: &CliOverrides) -> Result<bool, ConfigError> {
    if let Some(explicit) = cli.log_biometrics {
        return Ok(explicit);
    }
    if let Some(val) = non_empty_env(env, "BPMUTE_LOG_BIOMETRICS") {
        return parse_env_bool("BPMUTE_LOG_BIOMETRICS", &val);
    }
    Ok(false)
}
