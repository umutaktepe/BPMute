pub mod redact;

use std::fmt;
use std::io::Write;

use chrono::{SecondsFormat, Utc};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Dispatch, Event, Metadata, Subscriber};
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
                let redacting_layer =
                    BiometricRedactingLayer::new(journald_layer, loaded.log_biometrics);
                let subscriber = tracing_subscriber::registry()
                    .with(filter)
                    .with(redacting_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|err| ConfigError::LoggingInit(err.to_string()))?;
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
                let redacting_layer =
                    BiometricRedactingLayer::new(fmt_layer, loaded.log_biometrics);
                let subscriber = tracing_subscriber::registry()
                    .with(filter)
                    .with(redacting_layer);
                tracing::subscriber::set_global_default(subscriber)
                    .map_err(|e| ConfigError::LoggingInit(e.to_string()))?;
            }
        },
        EffectiveLogFormat::Pretty | EffectiveLogFormat::Json => {
            let fmt_layer =
                BiometricFmtLayer::new(resolved_format, loaded.log_biometrics, std::io::stderr);
            let redacting_layer = BiometricRedactingLayer::new(fmt_layer, loaded.log_biometrics);
            let subscriber = tracing_subscriber::registry()
                .with(filter)
                .with(redacting_layer);
            tracing::subscriber::set_global_default(subscriber)
                .map_err(|err| ConfigError::LoggingInit(err.to_string()))?;
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
    let layer = BiometricRedactingLayer::new(
        BiometricFmtLayer::new(format, log_biometrics, make_writer),
        log_biometrics,
    );
    Ok(tracing_subscriber::registry().with(filter).with(layer))
}

fn sanitize_field_value(name: &str, raw: String, log_biometrics: bool) -> String {
    if log_biometrics {
        raw
    } else if is_biometric_field(name) {
        "[REDACTED]".to_string()
    } else if is_mac_field(name) {
        mask_mac(&raw)
    } else {
        redact_macs_in_text(&raw).into_owned()
    }
}

/// Layer wrapper that sanitizes all span attributes, span records, and events before
/// forwarding to an inner [`tracing_subscriber::Layer`] (such as `tracing_journald::Layer`).
pub struct BiometricRedactingLayer<L> {
    inner: L,
    log_biometrics: bool,
}

impl<L> BiometricRedactingLayer<L> {
    /// Wraps `inner`, enforcing biometric and MAC redaction unless `log_biometrics` is `true`.
    pub fn new(inner: L, log_biometrics: bool) -> Self {
        Self {
            inner,
            log_biometrics,
        }
    }
}

struct SlotSanitizingVisitor<'a> {
    slots: &'a mut [Option<String>],
}

impl SlotSanitizingVisitor<'_> {
    fn set_slot(&mut self, field: &Field, raw: String) {
        if let Some(slot) = self.slots.get_mut(field.index()) {
            *slot = Some(sanitize_field_value(field.name(), raw, false));
        }
    }
}

impl Visit for SlotSanitizingVisitor<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.set_slot(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.set_slot(field, format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.set_slot(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.set_slot(field, value.to_string());
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.set_slot(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.set_slot(field, value.to_string());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.set_slot(field, value.to_string());
    }
}

impl<S, L> Layer<S> for BiometricRedactingLayer<L>
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    L: Layer<S> + 'static,
{
    fn on_register_dispatch(&self, subscriber: &Dispatch) {
        self.inner.on_register_dispatch(subscriber);
    }

    fn on_layer(&mut self, subscriber: &mut S) {
        self.inner.on_layer(subscriber);
    }

    fn register_callsite(
        &self,
        metadata: &'static Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        self.inner.register_callsite(metadata)
    }

    fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        self.inner.enabled(metadata, ctx)
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if self.log_biometrics {
            self.inner.on_new_span(attrs, id, ctx);
            return;
        }
        let meta = attrs.metadata();
        let field_set = meta.fields();
        let mut slots = vec![None; field_set.len()];
        attrs.record(&mut SlotSanitizingVisitor { slots: &mut slots });
        let val_refs: Vec<Option<&dyn tracing::Value>> = slots
            .iter()
            .map(|s| s.as_ref().map(|v| v as &dyn tracing::Value))
            .collect();
        let vs = field_set.value_set_all(&val_refs);
        let sanitized = if attrs.is_root() {
            Attributes::new_root(meta, &vs)
        } else if attrs.is_contextual() {
            Attributes::new(meta, &vs)
        } else {
            Attributes::child_of(
                attrs.parent().cloned().expect("explicit span parent"),
                meta,
                &vs,
            )
        };
        self.inner.on_new_span(&sanitized, id, ctx);
    }

    fn on_record(&self, span: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        if self.log_biometrics {
            self.inner.on_record(span, values, ctx);
            return;
        }
        let Some(span_ref) = ctx.span(span) else {
            return;
        };
        let meta = span_ref.metadata();
        let field_set = meta.fields();
        let mut slots = vec![None; field_set.len()];
        values.record(&mut SlotSanitizingVisitor { slots: &mut slots });
        let val_refs: Vec<Option<&dyn tracing::Value>> = slots
            .iter()
            .map(|s| s.as_ref().map(|v| v as &dyn tracing::Value))
            .collect();
        let vs = field_set.value_set_all(&val_refs);
        let sanitized = Record::new(&vs);
        drop(span_ref);
        self.inner.on_record(span, &sanitized, ctx);
    }

    fn on_follows_from(&self, span: &Id, follows: &Id, ctx: Context<'_, S>) {
        self.inner.on_follows_from(span, follows, ctx);
    }

    fn event_enabled(&self, event: &Event<'_>, ctx: Context<'_, S>) -> bool {
        self.inner.event_enabled(event, ctx)
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        if self.log_biometrics {
            self.inner.on_event(event, ctx);
            return;
        }
        let meta = event.metadata();
        let field_set = meta.fields();
        let mut slots = vec![None; field_set.len()];
        event.record(&mut SlotSanitizingVisitor { slots: &mut slots });
        let val_refs: Vec<Option<&dyn tracing::Value>> = slots
            .iter()
            .map(|s| s.as_ref().map(|v| v as &dyn tracing::Value))
            .collect();
        let vs = field_set.value_set_all(&val_refs);
        let sanitized = if event.is_contextual() {
            Event::new(meta, &vs)
        } else {
            Event::new_child_of(event.parent().cloned(), meta, &vs)
        };
        self.inner.on_event(&sanitized, ctx);
    }

    fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
        self.inner.on_exit(id, ctx);
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        self.inner.on_close(id, ctx);
    }

    fn on_id_change(&self, old: &Id, new: &Id, ctx: Context<'_, S>) {
        self.inner.on_id_change(old, new, ctx);
    }
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
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
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
        let sanitized = sanitize_field_value(name, raw, self.log_biometrics);

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
