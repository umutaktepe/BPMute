use std::borrow::Cow;

const BIOMETRIC_FIELD_NAMES: &[&str] = &[
    "bpm",
    "hr",
    "heart_rate",
    "raw_bpm",
    "rr",
    "rr_ms",
    "rr_intervals",
    "raw_rr",
    "rmssd",
    "sdnn",
    "avg_sleep_hr",
    "mu_sleep",
    "mu_wake",
];

const MAC_FIELD_NAMES: &[&str] = &[
    "mac",
    "address",
    "wearable_mac",
    "device_mac",
    "device_address",
];

/// Returns `true` if `name` identifies a sensitive biometric telemetry field.
pub fn is_biometric_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    BIOMETRIC_FIELD_NAMES.contains(&lower.as_str())
}

/// Returns `true` if `name` identifies a Bluetooth MAC address field.
pub fn is_mac_field(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    MAC_FIELD_NAMES.contains(&lower.as_str())
}

/// Masks a 6-octet MAC address (`AA:BB:CC:DD:EE:FF` or `aa-bb-cc-dd-ee-ff`) to `..:EE:FF`.
pub fn mask_mac(mac: &str) -> String {
    let trimmed = mac.trim();
    if let Some((x1, x2, y1, y2)) = parse_mac_tail(trimmed.as_bytes()) {
        format!("..:{x1}{x2}:{y1}{y2}")
    } else {
        "..:REDACTED".to_string()
    }
}

/// Formats a Wearable label respecting `log_biometrics`.
///
/// When redacted, outputs `"<alias> (..:XX:YY)"` or `"..:XX:YY"`.
/// When unlocked, outputs `"<alias> (AA:BB:CC:DD:EE:FF)"` or `"AA:BB:CC:DD:EE:FF"`.
pub fn format_wearable_label(alias: Option<&str>, mac: &str, log_biometrics: bool) -> String {
    let formatted_mac = if log_biometrics {
        normalize_mac_or_raw(mac)
    } else {
        mask_mac(mac)
    };

    match alias.map(str::trim).filter(|a| !a.is_empty()) {
        Some(name) => format!("{name} ({formatted_mac})"),
        None => formatted_mac,
    }
}

/// Replaces any 6-octet colon- or hyphen-separated MAC address substrings in `input` with `..:XX:YY`.
pub fn redact_macs_in_text(input: &str) -> Cow<'_, str> {
    let bytes = input.as_bytes();
    if bytes.len() < 17 {
        return Cow::Borrowed(input);
    }

    let mut out: Option<String> = None;
    let mut last_copied = 0usize;
    let mut i = 0usize;

    while i + 17 <= bytes.len() {
        if input.is_char_boundary(i)
            && input.is_char_boundary(i + 17)
            && (i == 0 || !bytes[i - 1].is_ascii_hexdigit())
            && (i + 17 == bytes.len() || !bytes[i + 17].is_ascii_hexdigit())
            && let Some((x1, x2, y1, y2)) = parse_mac_tail(&bytes[i..i + 17])
        {
            let buf = out.get_or_insert_with(|| String::with_capacity(input.len()));
            buf.push_str(&input[last_copied..i]);
            buf.push_str("..:");
            buf.push(x1);
            buf.push(x2);
            buf.push(':');
            buf.push(y1);
            buf.push(y2);
            i += 17;
            last_copied = i;
            continue;
        }
        i += 1;
    }

    match out {
        Some(mut buf) => {
            buf.push_str(&input[last_copied..]);
            Cow::Owned(buf)
        }
        None => Cow::Borrowed(input),
    }
}

fn parse_mac_tail(bytes: &[u8]) -> Option<(char, char, char, char)> {
    if bytes.len() != 17 {
        return None;
    }
    let sep = bytes[2];
    if sep != b':' && sep != b'-' {
        return None;
    }
    for pos in [5, 8, 11, 14] {
        if bytes[pos] != sep {
            return None;
        }
    }
    for octet_start in [0, 3, 6, 9, 12, 15] {
        if !bytes[octet_start].is_ascii_hexdigit() || !bytes[octet_start + 1].is_ascii_hexdigit() {
            return None;
        }
    }
    Some((
        (bytes[12] as char).to_ascii_uppercase(),
        (bytes[13] as char).to_ascii_uppercase(),
        (bytes[15] as char).to_ascii_uppercase(),
        (bytes[16] as char).to_ascii_uppercase(),
    ))
}

fn normalize_mac_or_raw(mac: &str) -> String {
    let trimmed = mac.trim();
    if trimmed.len() == 17 && parse_mac_tail(trimmed.as_bytes()).is_some() {
        trimmed
            .chars()
            .map(|c| {
                if c == '-' {
                    ':'
                } else {
                    c.to_ascii_uppercase()
                }
            })
            .collect()
    } else {
        trimmed.to_string()
    }
}

/// Emit-time redaction helper so even external layers such as `tracing-journald`
/// never receive raw BPM/RR telemetry or unmasked MAC addresses unless unlocked.
#[derive(Debug, Clone, Copy)]
pub struct BiometricLogContext {
    pub log_biometrics: bool,
}

impl BiometricLogContext {
    /// Logs a Wearable telemetry packet at `TRACE` with emit-time redaction.
    pub fn log_telemetry_sample(&self, alias: Option<&str>, mac: &str, bpm: f64, rr_ms: &[u16]) {
        let wearable = format_wearable_label(alias, mac, self.log_biometrics);
        if self.log_biometrics {
            tracing::trace!(
                wearable = %wearable,
                bpm = bpm,
                rr_ms = ?rr_ms,
                "wearable telemetry sample"
            );
        } else {
            tracing::trace!(
                wearable = %wearable,
                bpm = "[REDACTED]",
                rr_ms = "[REDACTED]",
                "wearable telemetry sample"
            );
        }
    }
}
