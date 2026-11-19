pub mod redact;

use std::fmt;
use std::io::Write;

use chrono::{SecondsFormat, Utc};
use tracing::Subscriber;
use tracing::field::{Field, Visit};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

use crate::config::{ConfigError, LoadedConfig, LogFormat};
use crate::paths::EnvSource;

pub use redact::{
    BiometricLogContext, format_wearable_label, is_biometric_field, is_mac_field, mask_mac,
    redact_macs_in_text,
};

/// Resolved concrete log output backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveLogFormat {
    Pretty,
    Json,
    Journald,
}

/// Resolves `auto` using `JOURNAL_STREAM` (per ADR-0009), keeping explicit formats intact.
pub fn resolve_log_format(configured: LogFormat, env: &dyn EnvSource) -> EffectiveLogFormat {
    match configured {
        LogFormat::Auto => match env.get("JOURNAL_STREAM") {
            Some(stream) if !stream.trim().is_empty() => EffectiveLogFormat::Journald,
            _ => EffectiveLogFormat::Pretty,
        },
        LogFormat::Pretty => EffectiveLogFormat::Pretty,
        LogFormat::Json => EffectiveLogFormat::Json,
        LogFormat::Journald => EffectiveLogFormat::Journald,
    }
}

/// Initializes global structured logging from `LoadedConfig` and environment context.
pub fn init_logging(loaded: &LoadedConfig, env: &dyn EnvSource) -> Result<(), ConfigError> {
    let filter = EnvFilter::try_new(&loaded.log_filter)
        .map_err(|err| ConfigError::LogFilter(format!("'{}': {err}", loaded.log_filter)))?;
    let resolved_format = resolve_log_format(loaded.log_format, env);

    match resolved_format {
        EffectiveLogFormat::Journald => match tracing_journald::Layer::new() {
            Ok(journald_layer) => {
                let subscriber = tracing_subscriber::registry()
                    .with(filter)
                    .with(journald_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|err| ConfigError::Validation(err.to_string()))?;
            }
            Err(err) => {
                if loaded.log_format == LogFormat::Journald {
                    return Err(ConfigError::Io(err));
                }
                let fmt_layer = BiometricFmtLayer::new(
                    EffectiveLogFormat::Pretty,
                    loaded.log_biometrics,
                    std::io::stderr,
                );
                let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|e| ConfigError::Validation(e.to_string()))?;
            }
        },
        EffectiveLogFormat::Pretty | EffectiveLogFormat::Json => {
            let fmt_layer =
                BiometricFmtLayer::new(resolved_format, loaded.log_biometrics, std::io::stderr);
            let subscriber = tracing_subscriber::registry().with(filter).with(fmt_layer);
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|err| ConfigError::Validation(err.to_string()))?;
        }
    }

    if loaded.log_biometrics {
        tracing::warn!(
            "BIOMETRIC LOGGING ENABLED: raw heart-rate telemetry and full Wearable MAC addresses will be written to logs"
        );
    }

    Ok(())
}

/// Builds a scoped subscriber for testing biometric redaction without touching global state.
pub fn build_test_subscriber<W>(
    filter_expr: &str,
    format: EffectiveLogFormat,
    log_biometrics: bool,
    make_writer: W,
) -> Result<impl Subscriber + Send + Sync, ConfigError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = EnvFilter::try_new(filter_expr)
        .map_err(|err| ConfigError::LogFilter(format!("'{filter_expr}': {err}")))?;
    let layer = BiometricFmtLayer::new(format, log_biometrics, make_writer);
    Ok(tracing_subscriber::registry().with(filter).with(layer))
}

/// Pure-Rust formatting layer enforcing biometric and MAC address redaction across all levels.
pub struct BiometricFmtLayer<W> {
    format: EffectiveLogFormat,
    log_biometrics: bool,
    make_writer: W,
}

impl<W> BiometricFmtLayer<W> {
    pub fn new(format: EffectiveLogFormat, log_biometrics: bool, make_writer: W) -> Self {
        Self {
            format,
            log_biometrics,
            make_writer,
        }
    }
}

impl<S, W> Layer<S> for BiometricFmtLayer<W>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    W: for<'writer> MakeWriter<'writer> + 'static,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = BiometricVisitor::new(self.log_biometrics);
        event.record(&mut visitor);

        let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
        let level = event.metadata().level().as_str();
        let target = event.metadata().target();

        let mut line = Vec::with_capacity(256);
        match self.format {
            EffectiveLogFormat::Json => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "timestamp".to_string(),
                    serde_json::Value::String(timestamp),
                );
                map.insert(
                    "level".to_string(),
                    serde_json::Value::String(level.to_string()),
                );
                map.insert(
                    "target".to_string(),
                    serde_json::Value::String(target.to_string()),
                );
                if let Some(msg) = visitor.message {
                    map.insert("message".to_string(), serde_json::Value::String(msg));
                }
                for (key, value) in visitor.fields {
                    map.insert(key, serde_json::Value::String(value));
                }
                if serde_json::to_writer(&mut line, &map).is_err() {
                    return;
                }
                line.push(b'\n');
            }
            EffectiveLogFormat::Pretty | EffectiveLogFormat::Journald => {
                let _ = write!(line, "{timestamp} {level:>5} {target}:");
                if let Some(msg) = visitor.message {
                    let _ = write!(line, " {msg}");
                }
                for (key, value) in visitor.fields {
                    let _ = write!(line, " {key}={value}");
                }
                line.push(b'\n');
            }
        }

        let mut writer = self.make_writer.make_writer();
        let _ = writer.write_all(&line);
    }
}

struct BiometricVisitor {
    log_biometrics: bool,
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl BiometricVisitor {
    fn new(log_biometrics: bool) -> Self {
        Self {
            log_biometrics,
            message: None,
            fields: Vec::new(),
        }
    }

    fn record_value(&mut self, field: &Field, raw: String) {
        let name = field.name();
        let sanitized = if self.log_biometrics {
            raw
        } else if is_biometric_field(name) {
            "[REDACTED]".to_string()
        } else if is_mac_field(name) {
            mask_mac(&raw)
        } else {
            redact_macs_in_text(&raw).into_owned()
        };

        if name == "message" {
            self.message = Some(sanitized);
        } else {
            self.fields.push((name.to_string(), sanitized));
        }
    }
}

impl Visit for BiometricVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record_value(field, format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_value(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_value(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record_value(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_value(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_value(field, value.to_string());
    }
}
