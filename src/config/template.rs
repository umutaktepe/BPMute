/// Commented declarative configuration template written by `bpmute config init`
/// and used as the base document for lazy creation on `config.update`.
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"# BPMute declarative configuration (ADR-0009)
# Enrolled Wearables and calibration ledger state live exclusively in SQLite.

schema_version = 1

[bluetooth]
# Pin a specific BlueZ adapter (e.g. "hci0"); omit to use all powered adapters.
# adapter = "hci0"

[detection]
# Sensitivity preset: "conservative", "balanced", or "responsive".
sensitivity = "balanced"

# Optional advanced HMM overrides:
# [detection.advanced]
# p_sleep_threshold = 0.85
# dwell_bpm_rr_sec = 180
# dwell_bpm_only_sec = 300

[schedule]
active_hours_enabled = false
start = "22:00"
end = "08:00"

[actions]
pause_media = true
ignore_players = []
custom_command = []
custom_command_timeout_sec = 10
abort_on_hook_failure = false
lock_screen = false
# Terminal power action: "none", "suspend", or "poweroff".
power_action = "none"
# Optional explicit Grace Period override (10..=300 seconds).
# When omitted, tiered defaults apply: 30s (none), 60s (suspend), 90s (poweroff).
# grace_period_sec = 30
snooze_duration_min = 30
dry_run = false
strict_idle_backend = false

[notifications]
connection_lifecycle = true

[retention]
# 0 (disable raw storage), positive integer days, or "unlimited".
raw_samples_days = 14
# Positive integer days or "unlimited".
session_epochs_days = "unlimited"

[tui]
mouse = true
ascii = false

[logging]
# Default level: "error", "warn", "info", "debug", or "trace".
level = "info"
# Output format: "auto", "pretty", "json", or "journald".
format = "auto"
"#;
