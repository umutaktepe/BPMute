use thiserror::Error;

/// Parsed pause target from `bpmute pause [duration|until-morning]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseTarget {
    Indefinite,
    UntilMorning,
    DurationSecs(u64),
}

#[derive(Debug, Error)]
#[error(
    "invalid pause target '{0}': expected `until-morning` or a positive duration like `30m` or `1h30m`"
)]
pub struct DurationParseError(String);

/// Parses optional `pause` positional argument (`None` -> indefinite, `"until-morning"`, or `<n><unit>`).
pub fn parse_pause_target(raw: Option<&str>) -> Result<PauseTarget, DurationParseError> {
    let Some(arg) = raw else {
        return Ok(PauseTarget::Indefinite);
    };
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Err(DurationParseError(arg.to_string()));
    }
    if trimmed.eq_ignore_ascii_case("until-morning") {
        return Ok(PauseTarget::UntilMorning);
    }

    let secs = parse_duration_secs(trimmed).ok_or_else(|| DurationParseError(arg.to_string()))?;
    if secs == 0 {
        return Err(DurationParseError(arg.to_string()));
    }
    Ok(PauseTarget::DurationSecs(secs))
}

fn parse_duration_secs(input: &str) -> Option<u64> {
    let bytes = input.as_bytes();
    let mut idx = 0usize;
    let mut total: u64 = 0;

    while idx < bytes.len() {
        let num_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == num_start || idx >= bytes.len() {
            return None;
        }
        let amount: u64 = input[num_start..idx].parse().ok()?;
        let multiplier: u64 = match bytes[idx] {
            b's' | b'S' => 1,
            b'm' | b'M' => 60,
            b'h' | b'H' => 3_600,
            b'd' | b'D' => 86_400,
            _ => return None,
        };
        idx += 1;
        let part = amount.checked_mul(multiplier)?;
        total = total.checked_add(part)?;
    }

    Some(total)
}
