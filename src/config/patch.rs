use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::loader::{CliOverrides, LoadedConfig};
use super::schema::{
    ClockTime, ConfigError, LogFormat, LogLevel, PowerAction, RawSamplesRetention, Sensitivity,
    SessionEpochsRetention,
};
use super::template::DEFAULT_CONFIG_TEMPLATE;
use crate::paths::EnvSource;
use crate::paths::write_private_file_atomic;

/// Tri-state field representation for JSON merge-patch style config updates.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PatchField<T> {
    /// Leave the key unchanged in `config.toml`.
    #[default]
    Unchanged,
    /// Remove the key from `config.toml` (represented as JSON `null`).
    Clear,
    /// Set the key to `T`.
    Set(T),
}

impl<T> PatchField<T> {
    /// Returns `true` if this field should be skipped during serialization.
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchField<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Option::<T>::deserialize(deserializer)? {
            Some(value) => Ok(Self::Set(value)),
            None => Ok(Self::Clear),
        }
    }
}

impl<T: Serialize> Serialize for PatchField<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Unchanged | Self::Clear => serializer.serialize_none(),
            Self::Set(value) => value.serialize(serializer),
        }
    }
}

/// Top-level patch payload accepted by `config.update`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigPatch {
    pub bluetooth: Option<BluetoothPatch>,
    pub detection: Option<DetectionPatch>,
    pub schedule: Option<SchedulePatch>,
    pub actions: Option<ActionsPatch>,
    pub notifications: Option<NotificationsPatch>,
    pub retention: Option<RetentionPatch>,
    pub tui: Option<TuiPatch>,
    pub logging: Option<LoggingPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BluetoothPatch {
    #[serde(skip_serializing_if = "PatchField::is_unchanged")]
    pub adapter: PatchField<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectionPatch {
    pub sensitivity: Option<Sensitivity>,
    #[serde(skip_serializing_if = "PatchField::is_unchanged")]
    pub advanced: PatchField<DetectionAdvancedPatch>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DetectionAdvancedPatch {
    #[serde(skip_serializing_if = "PatchField::is_unchanged")]
    pub p_sleep_threshold: PatchField<f64>,
    #[serde(skip_serializing_if = "PatchField::is_unchanged")]
    pub dwell_bpm_rr_sec: PatchField<u32>,
    #[serde(skip_serializing_if = "PatchField::is_unchanged")]
    pub dwell_bpm_only_sec: PatchField<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulePatch {
    pub active_hours_enabled: Option<bool>,
    pub start: Option<ClockTime>,
    pub end: Option<ClockTime>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ActionsPatch {
    pub pause_media: Option<bool>,
    pub ignore_players: Option<Vec<String>>,
    pub custom_command: Option<Vec<String>>,
    pub custom_command_timeout_sec: Option<u32>,
    pub abort_on_hook_failure: Option<bool>,
    pub lock_screen: Option<bool>,
    pub power_action: Option<PowerAction>,
    #[serde(skip_serializing_if = "PatchField::is_unchanged")]
    pub grace_period_sec: PatchField<u32>,
    pub snooze_duration_min: Option<u32>,
    pub dry_run: Option<bool>,
    pub strict_idle_backend: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsPatch {
    pub connection_lifecycle: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionPatch {
    pub raw_samples_days: Option<RawSamplesRetention>,
    pub session_epochs_days: Option<SessionEpochsRetention>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiPatch {
    pub mouse: Option<bool>,
    pub ascii: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingPatch {
    pub level: Option<LogLevel>,
    pub format: Option<LogFormat>,
}

/// Atomically patches `config.toml` via `toml_edit`, preserving comments and ordering.
///
/// If `path` does not exist yet, it is initialized from [`DEFAULT_CONFIG_TEMPLATE`]
/// and created lazily with `0700`/`0600` permissions after strict schema validation.
pub fn patch_config_file(
    path: &Path,
    patch: &ConfigPatch,
    env: &dyn EnvSource,
    cli: &CliOverrides,
) -> Result<LoadedConfig, ConfigError> {
    let base_text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => DEFAULT_CONFIG_TEMPLATE.to_string(),
        Err(err) => return Err(ConfigError::Io(err)),
    };

    let mut doc = base_text
        .parse::<DocumentMut>()
        .map_err(|err| ConfigError::Parse(err.to_string()))?;

    apply_patch_to_document(&mut doc, patch)?;

    let rendered = doc.to_string();
    let loaded = LoadedConfig::from_toml_str(&rendered, env, cli)?;

    write_private_file_atomic(path, rendered.as_bytes())?;
    Ok(loaded)
}

fn apply_patch_to_document(doc: &mut DocumentMut, patch: &ConfigPatch) -> Result<(), ConfigError> {
    let root = doc.as_table_mut();

    if let Some(bt) = &patch.bluetooth {
        apply_bluetooth_patch(root, bt)?;
    }
    if let Some(det) = &patch.detection {
        apply_detection_patch(root, det)?;
    }
    if let Some(sched) = &patch.schedule {
        apply_schedule_patch(root, sched)?;
    }
    if let Some(actions) = &patch.actions {
        apply_actions_patch(root, actions)?;
    }
    if let Some(notif) = &patch.notifications {
        apply_notifications_patch(root, notif)?;
    }
    if let Some(ret) = &patch.retention {
        apply_retention_patch(root, ret)?;
    }
    if let Some(tui) = &patch.tui {
        apply_tui_patch(root, tui)?;
    }
    if let Some(logging) = &patch.logging {
        apply_logging_patch(root, logging)?;
    }

    Ok(())
}

fn apply_bluetooth_patch(root: &mut Table, bt: &BluetoothPatch) -> Result<(), ConfigError> {
    match &bt.adapter {
        PatchField::Unchanged => Ok(()),
        PatchField::Clear => {
            if let Some(table) = existing_table_mut(root, &["bluetooth"])? {
                table.remove("adapter");
            }
            Ok(())
        }
        PatchField::Set(adapter) => {
            let table = ensure_table_mut(root, &["bluetooth"])?;
            set_table_value(table, "adapter", Value::from(adapter.as_str()));
            Ok(())
        }
    }
}

fn apply_detection_patch(root: &mut Table, det: &DetectionPatch) -> Result<(), ConfigError> {
    if let Some(sensitivity) = det.sensitivity {
        let table = ensure_table_mut(root, &["detection"])?;
        set_table_value(table, "sensitivity", Value::from(sensitivity.as_str()));
    }

    match &det.advanced {
        PatchField::Unchanged => {}
        PatchField::Clear => {
            if let Some(table) = existing_table_mut(root, &["detection"])? {
                table.remove("advanced");
            }
        }
        PatchField::Set(adv) => {
            apply_detection_advanced_patch(root, adv)?;
        }
    }
    Ok(())
}

fn apply_detection_advanced_patch(
    root: &mut Table,
    adv: &DetectionAdvancedPatch,
) -> Result<(), ConfigError> {
    if adv.p_sleep_threshold.is_unchanged()
        && adv.dwell_bpm_rr_sec.is_unchanged()
        && adv.dwell_bpm_only_sec.is_unchanged()
    {
        return Ok(());
    }

    let table = ensure_table_mut(root, &["detection", "advanced"])?;
    patch_optional_value(table, "p_sleep_threshold", &adv.p_sleep_threshold, |v| {
        Value::from(*v)
    });
    patch_optional_value(table, "dwell_bpm_rr_sec", &adv.dwell_bpm_rr_sec, |v| {
        Value::from(i64::from(*v))
    });
    patch_optional_value(table, "dwell_bpm_only_sec", &adv.dwell_bpm_only_sec, |v| {
        Value::from(i64::from(*v))
    });
    Ok(())
}

fn apply_schedule_patch(root: &mut Table, sched: &SchedulePatch) -> Result<(), ConfigError> {
    if sched.active_hours_enabled.is_none() && sched.start.is_none() && sched.end.is_none() {
        return Ok(());
    }
    let table = ensure_table_mut(root, &["schedule"])?;
    if let Some(enabled) = sched.active_hours_enabled {
        set_table_value(table, "active_hours_enabled", Value::from(enabled));
    }
    if let Some(start) = sched.start {
        set_table_value(table, "start", Value::from(start.to_string()));
    }
    if let Some(end) = sched.end {
        set_table_value(table, "end", Value::from(end.to_string()));
    }
    Ok(())
}

fn apply_actions_patch(root: &mut Table, actions: &ActionsPatch) -> Result<(), ConfigError> {
    let has_any = actions.pause_media.is_some()
        || actions.ignore_players.is_some()
        || actions.custom_command.is_some()
        || actions.custom_command_timeout_sec.is_some()
        || actions.abort_on_hook_failure.is_some()
        || actions.lock_screen.is_some()
        || actions.power_action.is_some()
        || !actions.grace_period_sec.is_unchanged()
        || actions.snooze_duration_min.is_some()
        || actions.dry_run.is_some()
        || actions.strict_idle_backend.is_some();

    if !has_any {
        return Ok(());
    }

    let table = ensure_table_mut(root, &["actions"])?;
    if let Some(v) = actions.pause_media {
        set_table_value(table, "pause_media", Value::from(v));
    }
    if let Some(players) = &actions.ignore_players {
        set_table_value(table, "ignore_players", string_array_value(players));
    }
    if let Some(cmd) = &actions.custom_command {
        set_table_value(table, "custom_command", string_array_value(cmd));
    }
    if let Some(v) = actions.custom_command_timeout_sec {
        set_table_value(
            table,
            "custom_command_timeout_sec",
            Value::from(i64::from(v)),
        );
    }
    if let Some(v) = actions.abort_on_hook_failure {
        set_table_value(table, "abort_on_hook_failure", Value::from(v));
    }
    if let Some(v) = actions.lock_screen {
        set_table_value(table, "lock_screen", Value::from(v));
    }
    if let Some(v) = actions.power_action {
        set_table_value(table, "power_action", Value::from(v.as_str()));
    }
    patch_optional_value(table, "grace_period_sec", &actions.grace_period_sec, |v| {
        Value::from(i64::from(*v))
    });
    if let Some(v) = actions.snooze_duration_min {
        set_table_value(table, "snooze_duration_min", Value::from(i64::from(v)));
    }
    if let Some(v) = actions.dry_run {
        set_table_value(table, "dry_run", Value::from(v));
    }
    if let Some(v) = actions.strict_idle_backend {
        set_table_value(table, "strict_idle_backend", Value::from(v));
    }
    Ok(())
}

fn apply_notifications_patch(
    root: &mut Table,
    notif: &NotificationsPatch,
) -> Result<(), ConfigError> {
    if let Some(v) = notif.connection_lifecycle {
        let table = ensure_table_mut(root, &["notifications"])?;
        set_table_value(table, "connection_lifecycle", Value::from(v));
    }
    Ok(())
}

fn apply_retention_patch(root: &mut Table, ret: &RetentionPatch) -> Result<(), ConfigError> {
    if ret.raw_samples_days.is_none() && ret.session_epochs_days.is_none() {
        return Ok(());
    }
    let table = ensure_table_mut(root, &["retention"])?;
    if let Some(raw) = ret.raw_samples_days {
        let val = match raw {
            RawSamplesRetention::Disabled => Value::from(0_i64),
            RawSamplesRetention::Days(d) => Value::from(i64::from(d)),
            RawSamplesRetention::Unlimited => Value::from("unlimited"),
        };
        set_table_value(table, "raw_samples_days", val);
    }
    if let Some(epochs) = ret.session_epochs_days {
        let val = match epochs {
            SessionEpochsRetention::Days(d) => Value::from(i64::from(d)),
            SessionEpochsRetention::Unlimited => Value::from("unlimited"),
        };
        set_table_value(table, "session_epochs_days", val);
    }
    Ok(())
}

fn apply_tui_patch(root: &mut Table, tui: &TuiPatch) -> Result<(), ConfigError> {
    if tui.mouse.is_none() && tui.ascii.is_none() {
        return Ok(());
    }
    let table = ensure_table_mut(root, &["tui"])?;
    if let Some(v) = tui.mouse {
        set_table_value(table, "mouse", Value::from(v));
    }
    if let Some(v) = tui.ascii {
        set_table_value(table, "ascii", Value::from(v));
    }
    Ok(())
}

fn apply_logging_patch(root: &mut Table, logging: &LoggingPatch) -> Result<(), ConfigError> {
    if logging.level.is_none() && logging.format.is_none() {
        return Ok(());
    }
    let table = ensure_table_mut(root, &["logging"])?;
    if let Some(v) = logging.level {
        set_table_value(table, "level", Value::from(v.as_str()));
    }
    if let Some(v) = logging.format {
        set_table_value(table, "format", Value::from(v.as_str()));
    }
    Ok(())
}

fn patch_optional_value<T>(
    table: &mut Table,
    key: &str,
    field: &PatchField<T>,
    to_value: impl FnOnce(&T) -> Value,
) {
    match field {
        PatchField::Unchanged => {}
        PatchField::Clear => {
            table.remove(key);
        }
        PatchField::Set(v) => {
            set_table_value(table, key, to_value(v));
        }
    }
}

fn string_array_value(items: &[String]) -> Value {
    let mut array = Array::new();
    for item in items {
        array.push(item.as_str());
    }
    Value::Array(array)
}

fn existing_table_mut<'a>(
    root: &'a mut Table,
    path: &[&str],
) -> Result<Option<&'a mut Table>, ConfigError> {
    let mut current = root;
    for &segment in path {
        let Some(item) = current.get_mut(segment) else {
            return Ok(None);
        };
        current = item.as_table_mut().ok_or_else(|| {
            ConfigError::TomlEdit(format!("expected [{segment}] to be a TOML table"))
        })?;
    }
    Ok(Some(current))
}

fn ensure_table_mut<'a>(root: &'a mut Table, path: &[&str]) -> Result<&'a mut Table, ConfigError> {
    let mut current = root;
    for &segment in path {
        if !current.contains_key(segment) {
            let mut new_table = Table::new();
            new_table.set_implicit(false);
            current.insert(segment, Item::Table(new_table));
        }
        current = current
            .get_mut(segment)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| {
                ConfigError::TomlEdit(format!("expected [{segment}] to be a TOML table"))
            })?;
    }
    Ok(current)
}

fn set_table_value(table: &mut Table, key: &str, mut new_val: Value) {
    if let Some(existing_item) = table.get_mut(key)
        && let Some(old_val) = existing_item.as_value()
    {
        *new_val.decor_mut() = old_val.decor().clone();
        *existing_item = Item::Value(new_val);
        return;
    }
    table.insert(key, Item::Value(new_val));
}
