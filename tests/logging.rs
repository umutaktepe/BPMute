use std::io;
use std::sync::{Arc, Mutex};

use bpmute::config::LogFormat;
use bpmute::logging::{
    BiometricLogContext, BiometricRedactingLayer, EffectiveLogFormat, build_test_subscriber,
    format_wearable_label, mask_mac, redact_macs_in_text, resolve_log_format,
};
use bpmute::paths::MapEnv;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

#[derive(Clone, Default)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

impl SharedBuffer {
    fn take_string(&self) -> String {
        let guard = self.0.lock().unwrap();
        String::from_utf8_lossy(&guard).into_owned()
    }
}

struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
    type Writer = BufWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufWriter(Arc::clone(&self.0))
    }
}

#[test]
fn journal_stream_auto_detection_selects_journald_or_pretty() {
    let without_stream = MapEnv::new();
    assert_eq!(
        resolve_log_format(LogFormat::Auto, &without_stream),
        EffectiveLogFormat::Pretty
    );

    let empty_stream = MapEnv::new().with("JOURNAL_STREAM", "");
    assert_eq!(
        resolve_log_format(LogFormat::Auto, &empty_stream),
        EffectiveLogFormat::Pretty
    );

    let with_stream = MapEnv::new().with("JOURNAL_STREAM", "8:123456");
    assert_eq!(
        resolve_log_format(LogFormat::Auto, &with_stream),
        EffectiveLogFormat::Journald
    );

    // Explicit formats override auto-detection.
    assert_eq!(
        resolve_log_format(LogFormat::Json, &with_stream),
        EffectiveLogFormat::Json
    );
    assert_eq!(
        resolve_log_format(LogFormat::Pretty, &with_stream),
        EffectiveLogFormat::Pretty
    );
    assert_eq!(
        resolve_log_format(LogFormat::Journald, &without_stream),
        EffectiveLogFormat::Journald
    );
}

#[test]
fn mac_masking_and_wearable_label_formatting() {
    assert_eq!(mask_mac("aa:bb:cc:dd:ee:05"), "..:EE:05");
    assert_eq!(mask_mac("AA-BB-CC-DD-EE-05"), "..:EE:05");
    assert_eq!(mask_mac("..:ee:05"), "..:EE:05");

    assert_eq!(
        format_wearable_label(Some("Huawei Watch"), "aa:bb:cc:dd:ee:05", false),
        "Huawei Watch (..:EE:05)"
    );
    assert_eq!(
        format_wearable_label(None, "aa:bb:cc:dd:ee:05", false),
        "..:EE:05"
    );
    assert_eq!(
        format_wearable_label(Some("Huawei Watch"), "aa:bb:cc:dd:ee:05", true),
        "Huawei Watch (AA:BB:CC:DD:EE:05)"
    );

    let text = "connected to AA:BB:CC:DD:EE:FF and 11-22-33-44-55-66";
    assert_eq!(
        redact_macs_in_text(text),
        "connected to ..:EE:FF and ..:55:66"
    );
}

#[test]
fn trace_and_debug_logs_redact_biometrics_and_macs_by_default_in_pretty_and_json() {
    for format in [EffectiveLogFormat::Pretty, EffectiveLogFormat::Json] {
        let buf = SharedBuffer::default();
        let subscriber = build_test_subscriber("trace", format, false, buf.clone())
            .expect("test subscriber should build");

        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(
                wearable = %format_wearable_label(Some("Watch GT5"), "D4:A9:28:11:22:C5", false),
                mac = "D4:A9:28:11:22:C5",
                bpm = 57.5,
                rr_ms = "[1043, 1052]",
                avg_sleep_hr = 54,
                "received 0x2A37 notification from D4:A9:28:11:22:C5"
            );
            tracing::debug!(
                hr = 61,
                rmssd = 42.3,
                "epoch computed for D4-A9-28-11-22-C5"
            );

            let ctx = BiometricLogContext {
                log_biometrics: false,
            };
            ctx.log_telemetry_sample(Some("Watch GT5"), "D4:A9:28:11:22:C5", 58.0, &[1024, 1031]);
        });

        let output = buf.take_string();
        assert!(!output.is_empty());

        // Sensitive values must NEVER appear anywhere at DEBUG or TRACE when disabled.
        assert!(!output.contains("57.5"), "raw bpm leaked:\n{output}");
        assert!(!output.contains("58.0"), "raw bpm leaked:\n{output}");
        assert!(!output.contains("61"), "raw hr leaked:\n{output}");
        assert!(!output.contains("54"), "avg_sleep_hr leaked:\n{output}");
        assert!(!output.contains("1043"), "raw rr leaked:\n{output}");
        assert!(!output.contains("1024"), "raw rr leaked:\n{output}");
        assert!(!output.contains("42.3"), "rmssd leaked:\n{output}");
        assert!(
            !output.contains("D4:A9:28:11:22:C5") && !output.contains("D4-A9-28-11-22-C5"),
            "full MAC leaked:\n{output}"
        );

        // Masked MAC and alias must be preserved.
        assert!(output.contains("..:22:C5"), "masked MAC missing:\n{output}");
        assert!(
            output.contains("Watch GT5 (..:22:C5)"),
            "formatted wearable label missing:\n{output}"
        );
        assert!(
            output.contains("[REDACTED]"),
            "redaction marker missing:\n{output}"
        );

        if format == EffectiveLogFormat::Json {
            for line in output.lines() {
                let parsed: serde_json::Value =
                    serde_json::from_str(line).expect("each JSON log line must be valid JSON");
                assert!(parsed.get("level").is_some());
                assert!(parsed.get("message").is_some());
            }
        }
    }
}

#[test]
fn explicit_log_biometrics_unlocks_raw_telemetry_and_full_mac() {
    let buf = SharedBuffer::default();
    let subscriber = build_test_subscriber("trace", EffectiveLogFormat::Json, true, buf.clone())
        .expect("test subscriber should build");

    tracing::subscriber::with_default(subscriber, || {
        let ctx = BiometricLogContext {
            log_biometrics: true,
        };
        ctx.log_telemetry_sample(Some("Watch GT5"), "d4:a9:28:11:22:c5", 57.5, &[1043, 1052]);
    });

    let output = buf.take_string();
    assert!(output.contains("57.5"));
    assert!(output.contains("1043"));
    assert!(output.contains("D4:A9:28:11:22:C5"));
    assert!(!output.contains("[REDACTED]"));
}

struct RawCaptureLayer(Arc<Mutex<Vec<String>>>);

struct RawCaptureVisitor<'a>(&'a mut String);

impl Visit for RawCaptureVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={:?}", field.name(), value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push_str(&format!(" {}={}", field.name(), value));
    }
}

impl<S> Layer<S> for RawCaptureLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: Context<'_, S>,
    ) {
        let mut line = format!("SPAN[{}]", attrs.metadata().name());
        attrs.record(&mut RawCaptureVisitor(&mut line));
        self.0.lock().unwrap().push(line);
    }

    fn on_record(
        &self,
        _span: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: Context<'_, S>,
    ) {
        let mut line = String::from("RECORD");
        values.record(&mut RawCaptureVisitor(&mut line));
        self.0.lock().unwrap().push(line);
    }

    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut line = format!("EVENT[{}]", event.metadata().level());
        event.record(&mut RawCaptureVisitor(&mut line));
        self.0.lock().unwrap().push(line);
    }
}

#[test]
fn redacting_wrapper_sanitizes_spans_and_events_for_unredacting_layers() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let wrapped = BiometricRedactingLayer::new(RawCaptureLayer(Arc::clone(&captured)), false);
    let subscriber = tracing_subscriber::registry().with(wrapped);

    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "wearable_stream",
            mac = "AA:BB:CC:DD:EE:FF",
            bpm = tracing::field::Empty
        );
        span.record("bpm", 55.2);
        let _guard = span.enter();
        tracing::trace!(
            hr = 56,
            rr_ms = "[990, 1005]",
            "raw packet from AA:BB:CC:DD:EE:FF"
        );
    });

    let joined = captured.lock().unwrap().join("\n");
    assert!(
        !joined.contains("AA:BB:CC:DD:EE:FF"),
        "full MAC leaked:\n{joined}"
    );
    assert!(!joined.contains("55.2"), "span bpm leaked:\n{joined}");
    assert!(!joined.contains("56"), "event hr leaked:\n{joined}");
    assert!(!joined.contains("990"), "event rr leaked:\n{joined}");
    assert!(joined.contains("..:EE:FF"), "masked MAC missing:\n{joined}");
    assert!(
        joined.contains("[REDACTED]"),
        "redaction marker missing:\n{joined}"
    );
}
