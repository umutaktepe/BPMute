pub mod loader;
pub mod patch;
pub mod schema;
pub mod template;

pub use loader::{CliOverrides, LoadedConfig, init_config_file};
pub use patch::{
    ActionsPatch, BluetoothPatch, ConfigPatch, DetectionAdvancedPatch, DetectionPatch,
    LoggingPatch, NotificationsPatch, PatchField, RetentionPatch, SchedulePatch, TuiPatch,
    patch_config_file,
};
pub use schema::{
    ActionsConfig, BluetoothConfig, CURRENT_SCHEMA_VERSION, ClockTime, Config, ConfigError,
    DetectionAdvancedConfig, DetectionConfig, LogFormat, LogLevel, LoggingConfig,
    NotificationsConfig, PowerAction, RawSamplesRetention, RetentionConfig, ScheduleConfig,
    Sensitivity, SessionEpochsRetention, TuiConfig,
};
pub use template::DEFAULT_CONFIG_TEMPLATE;
