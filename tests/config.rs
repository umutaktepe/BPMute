use std::fs;
use std::os::unix::fs::PermissionsExt;

use bpmute::config::{
    ActionsConfig, ActionsPatch, BluetoothPatch, CliOverrides, ClockTime, Config, ConfigError,
    ConfigPatch, DetectionPatch, LoadedConfig, LogFormat, LogLevel, PatchField, PowerAction,
    RawSamplesRetention, RetentionPatch, Sensitivity, SessionEpochsRetention, init_config_file,
    patch_config_file,
};
use bpmute::exit_code::ExitCode;
use bpmute::paths::{MapEnv, ResolvedPaths};
use chrono::{Local, NaiveTime, TimeZone};
use tempfile::TempDir;

#[test]
fn missing_config_uses_defaults_without_creating_file_or_dir() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("nested/bpmute/config.toml");
    let env = MapEnv::new();

    let loaded = LoadedConfig::load(&config_path, &env, &CliOverrides::default())
        .expect("missing config should load defaults");

    assert!(!loaded.file_present);
    assert!(!config_path.exists());
    assert!(!config_path.parent().unwrap().exists());

    assert_eq!(loaded.config.schema_version, 1);
    assert_eq!(loaded.config.detection.sensitivity, Sensitivity::Balanced);
    assert!(loaded.config.actions.pause_media);
    assert_eq!(loaded.config.actions.power_action, PowerAction::None);
    assert_eq!(loaded.config.actions.effective_grace_period_sec(), 30);
    assert_eq!(
        loaded.config.retention.raw_samples_days,
        RawSamplesRetention::Days(14)
    );
    assert_eq!(
        loaded.config.retention.session_epochs_days,
        SessionEpochsRetention::Unlimited
    );
    assert_eq!(loaded.config.logging.level, LogLevel::Info);
    assert_eq!(loaded.config.logging.format, LogFormat::Auto);
    assert_eq!(loaded.log_filter, "info");
    assert!(!loaded.log_biometrics);
}

#[test]
fn init_config_creates_0700_dir_and_0600_file_and_respects_force() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("xdg-cfg/bpmute");
    let config_path = config_dir.join("config.toml");

    init_config_file(&config_path, false).expect("initial config init succeeds");
    assert!(config_path.exists());

    let dir_mode = fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
    let file_mode = fs::metadata(&config_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(file_mode, 0o600);

    // Default template itself must parse cleanly under strict schema validation.
    let loaded = LoadedConfig::load(&config_path, &MapEnv::new(), &CliOverrides::default())
        .expect("default template should parse cleanly");
    assert!(loaded.file_present);
    assert_eq!(loaded.config, Config::default());

    // Without --force, second init fails with AlreadyExists (exit 4).
    let err = init_config_file(&config_path, false).expect_err("second init without force fails");
    assert!(matches!(err, ConfigError::AlreadyExists(_)));
    assert_eq!(err.exit_code(), ExitCode::PreconditionConflict);

    // Modify file, then overwrite with --force.
    fs::write(&config_path, "schema_version = 1\n").unwrap();
    init_config_file(&config_path, true).expect("init with force overwrites");
    let content = fs::read_to_string(&config_path).unwrap();
    assert!(content.contains("[detection]"));
    assert!(content.contains("schema_version = 1"));
}

#[test]
fn rejects_missing_or_wrong_schema_version_and_unknown_fields() {
    let env = MapEnv::new();
    let cli = CliOverrides::default();

    let missing_ver =
        LoadedConfig::from_toml_str("[detection]\nsensitivity = \"balanced\"\n", &env, &cli)
            .expect_err("missing schema_version must fail");
    assert_eq!(missing_ver.exit_code(), ExitCode::UsageOrConfigError);

    let wrong_ver = LoadedConfig::from_toml_str("schema_version = 2\n", &env, &cli)
        .expect_err("wrong schema_version must fail");
    assert_eq!(wrong_ver.exit_code(), ExitCode::UsageOrConfigError);

    let unknown_top =
        LoadedConfig::from_toml_str("schema_version = 1\nwearables = []\n", &env, &cli)
            .expect_err("unknown top-level key must fail");
    assert_eq!(unknown_top.exit_code(), ExitCode::UsageOrConfigError);

    let unknown_nested = LoadedConfig::from_toml_str(
        "schema_version = 1\n[actions]\nunknown_toggle = true\n",
        &env,
        &cli,
    )
    .expect_err("unknown nested key must fail");
    assert_eq!(unknown_nested.exit_code(), ExitCode::UsageOrConfigError);
}

#[test]
fn validates_field_bounds_and_formats() {
    let env = MapEnv::new();
    let cli = CliOverrides::default();

    let bad_cases = [
        "schema_version = 1\n[bluetooth]\nadapter = \"   \"\n",
        "schema_version = 1\n[detection.advanced]\np_sleep_threshold = 0.49\n",
        "schema_version = 1\n[detection.advanced]\np_sleep_threshold = 1.0\n",
        "schema_version = 1\n[detection.advanced]\ndwell_bpm_rr_sec = 59\n",
        "schema_version = 1\n[detection.advanced]\ndwell_bpm_only_sec = 901\n",
        "schema_version = 1\n[schedule]\nstart = \"25:00\"\n",
        "schema_version = 1\n[schedule]\nactive_hours_enabled = true\nstart = \"22:00\"\nend = \"22:00\"\n",
        "schema_version = 1\n[actions]\nignore_players = [\"\"]\n",
        "schema_version = 1\n[actions]\ncustom_command = [\"\"]\n",
        "schema_version = 1\n[actions]\ncustom_command_timeout_sec = 61\n",
        "schema_version = 1\n[actions]\ngrace_period_sec = 9\n",
        "schema_version = 1\n[actions]\ngrace_period_sec = 301\n",
        "schema_version = 1\n[actions]\nsnooze_duration_min = 4\n",
        "schema_version = 1\n[retention]\nraw_samples_days = -1\n",
        "schema_version = 1\n[retention]\nraw_samples_days = \"forever\"\n",
        "schema_version = 1\n[retention]\nsession_epochs_days = 0\n",
        "schema_version = 1\n[retention]\nsession_epochs_days = \"never\"\n",
    ];

    for toml in bad_cases {
        let err = LoadedConfig::from_toml_str(toml, &env, &cli)
            .expect_err(&format!("expected validation failure for:\n{toml}"));
        assert_eq!(
            err.exit_code(),
            ExitCode::UsageOrConfigError,
            "wrong exit code for:\n{toml}"
        );
    }

    // Valid retention variants: 0, positive N, "unlimited"
    let raw_zero = LoadedConfig::from_toml_str(
        "schema_version = 1\n[retention]\nraw_samples_days = 0\nsession_epochs_days = 365\n",
        &env,
        &cli,
    )
    .expect("raw_samples_days = 0 and session_epochs_days = 365 should be valid");
    assert_eq!(
        raw_zero.config.retention.raw_samples_days,
        RawSamplesRetention::Disabled
    );
    assert_eq!(
        raw_zero.config.retention.session_epochs_days,
        SessionEpochsRetention::Days(365)
    );

    let raw_unlim = LoadedConfig::from_toml_str(
        "schema_version = 1\n[retention]\nraw_samples_days = \"unlimited\"\nsession_epochs_days = \"unlimited\"\n",
        &env,
        &cli,
    )
    .expect("unlimited should be valid");
    assert_eq!(
        raw_unlim.config.retention.raw_samples_days,
        RawSamplesRetention::Unlimited
    );
}

#[test]
fn tiered_grace_period_defaults_follow_power_action_unless_overridden() {
    let mut actions = ActionsConfig::default();
    assert_eq!(actions.effective_grace_period_sec(), 30);

    actions.power_action = PowerAction::Suspend;
    assert_eq!(actions.effective_grace_period_sec(), 60);

    actions.power_action = PowerAction::Poweroff;
    assert_eq!(actions.effective_grace_period_sec(), 90);

    actions.grace_period_sec = Some(45);
    assert_eq!(actions.effective_grace_period_sec(), 45);
}

#[test]
fn schedule_wraparound_and_next_occurrence_work() {
    let start: ClockTime = "22:00".parse().unwrap();
    let end: ClockTime = "08:00".parse().unwrap();
    let mut schedule = bpmute::config::ScheduleConfig {
        active_hours_enabled: true,
        start,
        end,
    };

    assert!(schedule.is_within_active_hours(NaiveTime::from_hms_opt(23, 30, 0).unwrap()));
    assert!(schedule.is_within_active_hours(NaiveTime::from_hms_opt(2, 0, 0).unwrap()));
    assert!(!schedule.is_within_active_hours(NaiveTime::from_hms_opt(12, 0, 0).unwrap()));
    assert!(!schedule.is_within_active_hours(NaiveTime::from_hms_opt(8, 0, 0).unwrap()));

    // Non-wrapping daytime schedule: 09:00..17:00
    schedule.start = "09:00".parse().unwrap();
    schedule.end = "17:00".parse().unwrap();
    assert!(schedule.is_within_active_hours(NaiveTime::from_hms_opt(12, 0, 0).unwrap()));
    assert!(!schedule.is_within_active_hours(NaiveTime::from_hms_opt(22, 0, 0).unwrap()));

    let morning: ClockTime = "08:00".parse().unwrap();
    let before = Local.with_ymd_and_hms(2026, 6, 1, 2, 15, 0).unwrap();
    let next_same_day = morning.next_occurrence_after(before);
    assert_eq!(next_same_day.date_naive(), before.date_naive());
    assert_eq!(
        next_same_day.time(),
        NaiveTime::from_hms_opt(8, 0, 0).unwrap()
    );

    let after = Local.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap();
    let next_tomorrow = morning.next_occurrence_after(after);
    assert_eq!(
        next_tomorrow.date_naive(),
        before.date_naive().succ_opt().unwrap()
    );
}

#[test]
fn precedence_cli_gt_env_gt_toml_gt_defaults_and_invalid_env_fails_fast() {
    let toml = r#"
schema_version = 1

[detection]
sensitivity = "conservative"

[actions]
power_action = "suspend"
dry_run = false

[logging]
level = "warn"
format = "pretty"
"#;

    let env = MapEnv::new()
        .with("BPMUTE_DETECTION_SENSITIVITY", "responsive")
        .with("BPMUTE_ACTIONS_DRY_RUN", "true")
        .with("BPMUTE_LOG", "bpmute=debug")
        .with("BPMUTE_LOG_FORMAT", "json")
        .with("BPMUTE_LOG_BIOMETRICS", "1");

    // Env overrides TOML; omitted CLI flags do NOT clobber env.
    let loaded = LoadedConfig::from_toml_str(toml, &env, &CliOverrides::default()).unwrap();
    assert_eq!(loaded.config.detection.sensitivity, Sensitivity::Responsive);
    assert_eq!(loaded.config.actions.power_action, PowerAction::Suspend);
    assert!(loaded.config.actions.dry_run);
    assert_eq!(loaded.log_filter, "bpmute=debug");
    assert_eq!(loaded.log_format, LogFormat::Json);
    assert!(loaded.log_biometrics);

    // CLI overrides Env and TOML.
    let cli = CliOverrides {
        dry_run: Some(false),
        log_level: Some("trace".to_string()),
        log_format: Some(LogFormat::Journald),
        log_biometrics: Some(false),
    };
    let loaded_cli = LoadedConfig::from_toml_str(toml, &env, &cli).unwrap();
    assert!(!loaded_cli.config.actions.dry_run);
    assert_eq!(loaded_cli.log_filter, "trace");
    assert_eq!(loaded_cli.log_format, LogFormat::Journald);
    assert!(!loaded_cli.log_biometrics);

    // Invalid BPMUTE_LOG filter fails fast with exit 2.
    let bad_filter_env = MapEnv::new().with("BPMUTE_LOG", "invalid[directive");
    let err = LoadedConfig::from_toml_str(
        "schema_version = 1\n",
        &bad_filter_env,
        &CliOverrides::default(),
    )
    .expect_err("invalid EnvFilter must fail fast");
    assert_eq!(err.exit_code(), ExitCode::UsageOrConfigError);

    // Invalid BPMUTE_* config env fails fast with exit 2.
    let bad_val_env = MapEnv::new().with("BPMUTE_ACTIONS_GRACE_PERIOD_SEC", "999");
    let err2 = LoadedConfig::from_toml_str(
        "schema_version = 1\n",
        &bad_val_env,
        &CliOverrides::default(),
    )
    .expect_err("invalid BPMUTE_ACTIONS_GRACE_PERIOD_SEC must fail fast");
    assert_eq!(err2.exit_code(), ExitCode::UsageOrConfigError);
}

#[test]
fn path_resolution_precedence_handles_cli_env_xdg_and_home() {
    let env = MapEnv::new()
        .with("HOME", "/home/alice")
        .with("XDG_CONFIG_HOME", "/xdg/cfg")
        .with("XDG_DATA_HOME", "/xdg/data")
        .with("XDG_RUNTIME_DIR", "/run/user/1000")
        .with("BPMUTE_CONFIG", "/env/config.toml")
        .with("BPMUTE_DATA_DIR", "/env/data")
        .with("BPMUTE_SOCKET", "/env/daemon.sock");

    let cli_wins = ResolvedPaths::resolve(
        Some(std::path::Path::new("/cli/config.toml")),
        Some(std::path::Path::new("/cli/data")),
        Some(std::path::Path::new("/cli/daemon.sock")),
        &env,
    )
    .unwrap();
    assert_eq!(
        cli_wins.config_file,
        std::path::Path::new("/cli/config.toml")
    );
    assert_eq!(cli_wins.data_dir, std::path::Path::new("/cli/data"));
    assert_eq!(
        cli_wins.socket_path,
        std::path::Path::new("/cli/daemon.sock")
    );

    let env_wins = ResolvedPaths::resolve(None, None, None, &env).unwrap();
    assert_eq!(
        env_wins.config_file,
        std::path::Path::new("/env/config.toml")
    );
    assert_eq!(env_wins.data_dir, std::path::Path::new("/env/data"));
    assert_eq!(
        env_wins.socket_path,
        std::path::Path::new("/env/daemon.sock")
    );

    let xdg_env = MapEnv::new()
        .with("HOME", "/home/alice")
        .with("XDG_CONFIG_HOME", "/xdg/cfg")
        .with("XDG_DATA_HOME", "/xdg/data")
        .with("XDG_RUNTIME_DIR", "");
    let xdg_paths = ResolvedPaths::resolve(None, None, None, &xdg_env).unwrap();
    assert_eq!(
        xdg_paths.config_file,
        std::path::Path::new("/xdg/cfg/bpmute/config.toml")
    );
    assert_eq!(xdg_paths.data_dir, std::path::Path::new("/xdg/data/bpmute"));
    assert!(
        xdg_paths
            .socket_path
            .to_string_lossy()
            .starts_with("/tmp/bpmute-")
    );
}

#[test]
fn patch_creates_lazily_preserves_comments_and_handles_null_clears() {
    let temp = TempDir::new().expect("tempdir");
    let config_dir = temp.path().join("lazy/bpmute");
    let config_path = config_dir.join("config.toml");
    let env = MapEnv::new();
    let cli = CliOverrides::default();

    // 1. Lazy creation when config.toml does not exist yet.
    let lazy_patch = ConfigPatch {
        actions: Some(ActionsPatch {
            dry_run: Some(true),
            grace_period_sec: PatchField::Set(45),
            ..Default::default()
        }),
        ..Default::default()
    };
    let loaded = patch_config_file(&config_path, &lazy_patch, &env, &cli)
        .expect("lazy patch should succeed");
    assert!(loaded.file_present);
    assert!(loaded.config.actions.dry_run);
    assert_eq!(loaded.config.actions.grace_period_sec, Some(45));
    assert_eq!(
        fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // 2. Preserve user comments, inline comments, and key ordering on existing file.
    let custom_toml = r#"# User top header
schema_version = 1

# Custom bluetooth comment
[bluetooth]
adapter = "hci0" # keep bluetooth inline

[actions]
# My power setting
power_action = "none" # keep power inline
pause_media = true
grace_period_sec = 60 # keep grace inline

[detection]
sensitivity = "balanced" # keep sensitivity inline
"#;
    fs::write(&config_path, custom_toml).unwrap();

    let patch: ConfigPatch = serde_json::from_str(
        r#"{
            "bluetooth": { "adapter": "hci1" },
            "detection": { "sensitivity": "conservative" },
            "actions": { "power_action": "suspend", "ignore_players": ["vlc", "spotify"] },
            "retention": { "raw_samples_days": "unlimited" }
        }"#,
    )
    .expect("patch JSON should parse");

    let updated =
        patch_config_file(&config_path, &patch, &env, &cli).expect("patch should succeed");
    assert_eq!(updated.config.bluetooth.adapter.as_deref(), Some("hci1"));
    assert_eq!(
        updated.config.detection.sensitivity,
        Sensitivity::Conservative
    );
    assert_eq!(updated.config.actions.power_action, PowerAction::Suspend);
    assert_eq!(
        updated.config.actions.ignore_players,
        vec!["vlc".to_string(), "spotify".to_string()]
    );
    assert_eq!(updated.config.actions.grace_period_sec, Some(60));
    assert_eq!(
        updated.config.retention.raw_samples_days,
        RawSamplesRetention::Unlimited
    );

    let on_disk = fs::read_to_string(&config_path).unwrap();
    assert!(on_disk.contains("# User top header"));
    assert!(on_disk.contains("# Custom bluetooth comment"));
    assert!(on_disk.contains("# keep bluetooth inline"));
    assert!(on_disk.contains("# My power setting"));
    assert!(on_disk.contains("# keep power inline"));
    assert!(on_disk.contains("# keep grace inline"));
    assert!(on_disk.contains("# keep sensitivity inline"));
    // Ordering check: [bluetooth] remained before [actions], which remained before [detection].
    let bt_pos = on_disk.find("[bluetooth]").unwrap();
    let act_pos = on_disk.find("[actions]").unwrap();
    let det_pos = on_disk.find("[detection]").unwrap();
    assert!(bt_pos < act_pos && act_pos < det_pos);

    // 3. Clear nullable fields via JSON null (`PatchField::Clear`).
    let clear_patch: ConfigPatch = serde_json::from_str(
        r#"{
            "bluetooth": { "adapter": null },
            "actions": { "grace_period_sec": null }
        }"#,
    )
    .expect("clear patch JSON should parse");
    assert_eq!(
        clear_patch.bluetooth.as_ref().unwrap().adapter,
        PatchField::Clear
    );
    assert_eq!(
        clear_patch.actions.as_ref().unwrap().grace_period_sec,
        PatchField::Clear
    );

    let cleared =
        patch_config_file(&config_path, &clear_patch, &env, &cli).expect("clear patch succeeds");
    assert_eq!(cleared.config.bluetooth.adapter, None);
    assert_eq!(cleared.config.actions.grace_period_sec, None);
    assert_eq!(cleared.config.actions.effective_grace_period_sec(), 60);

    let cleared_disk = fs::read_to_string(&config_path).unwrap();
    assert!(!cleared_disk.contains("adapter ="));
    assert!(!cleared_disk.contains("grace_period_sec ="));
    assert!(cleared_disk.contains("# User top header"));

    // Unknown fields in patch JSON must be rejected.
    let bad_patch = serde_json::from_str::<ConfigPatch>(r#"{"unknown": true}"#);
    assert!(bad_patch.is_err());
}

#[test]
fn invalid_patch_leaves_existing_file_untouched() {
    let temp = TempDir::new().expect("tempdir");
    let config_path = temp.path().join("bpmute/config.toml");
    let env = MapEnv::new();
    let cli = CliOverrides::default();

    init_config_file(&config_path, false).unwrap();
    let before = fs::read_to_string(&config_path).unwrap();

    let bad_patch = ConfigPatch {
        actions: Some(ActionsPatch {
            grace_period_sec: PatchField::Set(5),
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = patch_config_file(&config_path, &bad_patch, &env, &cli)
        .expect_err("out-of-bounds grace_period_sec patch must fail");
    assert_eq!(err.exit_code(), ExitCode::UsageOrConfigError);

    let after = fs::read_to_string(&config_path).unwrap();
    assert_eq!(after, before);

    // Also test clearing via BluetoothPatch directly.
    let _ = BluetoothPatch {
        adapter: PatchField::Unchanged,
    };
    let _ = DetectionPatch::default();
    let _ = RetentionPatch::default();
}
