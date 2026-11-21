use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::schema::ConfigError;

/// Retention policy for 1 Hz `raw_samples` (`0`, positive `N`, or `"unlimited"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawSamplesRetention {
    Disabled,
    Days(u32),
    Unlimited,
}

impl Default for RawSamplesRetention {
    fn default() -> Self {
        Self::Days(14)
    }
}

impl fmt::Display for RawSamplesRetention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("0"),
            Self::Days(days) => write!(f, "{days}"),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

impl FromStr for RawSamplesRetention {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("unlimited") {
            return Ok(Self::Unlimited);
        }
        let days: i64 = trimmed.parse().map_err(|_| {
            format!(
                "invalid raw_samples_days '{s}': expected 0, positive integer, or \"unlimited\""
            )
        })?;
        match days {
            0 => Ok(Self::Disabled),
            d if d > 0 => u32::try_from(d)
                .map(Self::Days)
                .map_err(|_| format!("raw_samples_days '{s}' exceeds u32 range")),
            _ => Err(format!(
                "invalid raw_samples_days '{s}': negative values are not allowed"
            )),
        }
    }
}

impl Serialize for RawSamplesRetention {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Disabled => serializer.serialize_u32(0),
            Self::Days(days) => serializer.serialize_u32(*days),
            Self::Unlimited => serializer.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for RawSamplesRetention {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RawSamplesVisitor;

        impl<'de> Visitor<'de> for RawSamplesVisitor {
            type Value = RawSamplesRetention;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("0, a positive integer, or \"unlimited\"")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                match v {
                    0 => Ok(RawSamplesRetention::Disabled),
                    d if d > 0 => u32::try_from(d)
                        .map(RawSamplesRetention::Days)
                        .map_err(|_| E::custom("raw_samples_days exceeds u32 range")),
                    _ => Err(E::custom("raw_samples_days cannot be negative")),
                }
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 {
                    Ok(RawSamplesRetention::Disabled)
                } else {
                    u32::try_from(v)
                        .map(RawSamplesRetention::Days)
                        .map_err(|_| E::custom("raw_samples_days exceeds u32 range"))
                }
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v == "unlimited" {
                    Ok(RawSamplesRetention::Unlimited)
                } else {
                    Err(E::custom(format!(
                        "invalid raw_samples_days '{v}': expected 0, positive integer, or \"unlimited\""
                    )))
                }
            }
        }

        deserializer.deserialize_any(RawSamplesVisitor)
    }
}

/// Retention policy for 15 s `session_epochs` (positive `N` or `"unlimited"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SessionEpochsRetention {
    Days(u32),
    #[default]
    Unlimited,
}

impl fmt::Display for SessionEpochsRetention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Days(days) => write!(f, "{days}"),
            Self::Unlimited => f.write_str("unlimited"),
        }
    }
}

impl FromStr for SessionEpochsRetention {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.eq_ignore_ascii_case("unlimited") {
            return Ok(Self::Unlimited);
        }
        let days: i64 = trimmed.parse().map_err(|_| {
            format!("invalid session_epochs_days '{s}': expected positive integer or \"unlimited\"")
        })?;
        if days <= 0 {
            return Err(format!(
                "invalid session_epochs_days '{s}': must be >= 1 or \"unlimited\""
            ));
        }
        u32::try_from(days)
            .map(Self::Days)
            .map_err(|_| format!("session_epochs_days '{s}' exceeds u32 range"))
    }
}

impl Serialize for SessionEpochsRetention {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Days(days) => serializer.serialize_u32(*days),
            Self::Unlimited => serializer.serialize_str("unlimited"),
        }
    }
}

impl<'de> Deserialize<'de> for SessionEpochsRetention {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SessionEpochsVisitor;

        impl<'de> Visitor<'de> for SessionEpochsVisitor {
            type Value = SessionEpochsRetention;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a positive integer (>= 1) or \"unlimited\"")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                if v <= 0 {
                    return Err(E::custom(
                        "session_epochs_days must be >= 1 or \"unlimited\"",
                    ));
                }
                u32::try_from(v)
                    .map(SessionEpochsRetention::Days)
                    .map_err(|_| E::custom("session_epochs_days exceeds u32 range"))
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                if v == 0 {
                    return Err(E::custom(
                        "session_epochs_days must be >= 1 or \"unlimited\"",
                    ));
                }
                u32::try_from(v)
                    .map(SessionEpochsRetention::Days)
                    .map_err(|_| E::custom("session_epochs_days exceeds u32 range"))
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                if v == "unlimited" {
                    Ok(SessionEpochsRetention::Unlimited)
                } else {
                    Err(E::custom(format!(
                        "invalid session_epochs_days '{v}': expected positive integer or \"unlimited\""
                    )))
                }
            }
        }

        deserializer.deserialize_any(SessionEpochsVisitor)
    }
}

/// Three-tier retention policies for biometric SQLite tables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub raw_samples_days: RawSamplesRetention,
    pub session_epochs_days: SessionEpochsRetention,
}

impl RetentionConfig {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if let RawSamplesRetention::Days(0) = self.raw_samples_days {
            return Err(ConfigError::Validation(
                "retention.raw_samples_days Days(0) should be Disabled".to_string(),
            ));
        }
        if let SessionEpochsRetention::Days(0) = self.session_epochs_days {
            return Err(ConfigError::Validation(
                "retention.session_epochs_days must be >= 1".to_string(),
            ));
        }
        Ok(())
    }
}
