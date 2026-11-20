//! Parsing and validation of Bluetooth SIG GATT Heart Rate (`0x2A37`) and Body Location (`0x2A38`).

use std::fmt;

use thiserror::Error;

use crate::sleep_engine::{ContactStatus, HrPacket};

/// Errors encountered while decoding standard GATT Heart Rate Measurement payloads (`0x2A37`).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HrPacketError {
    #[error("payload is too short (expected at least {expected} bytes, got {actual})")]
    Truncated { expected: usize, actual: usize },
}

/// Standard Bluetooth SIG Body Sensor Location (`0x2A38`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySensorLocation {
    Other,
    Chest,
    Wrist,
    Finger,
    Hand,
    EarLobe,
    Foot,
    Unknown(u8),
}

impl BodySensorLocation {
    /// Returns the human-readable body location label.
    pub fn display_label(self) -> &'static str {
        match self {
            Self::Other => "Other",
            Self::Chest => "Chest",
            Self::Wrist => "Wrist",
            Self::Finger => "Finger",
            Self::Hand => "Hand",
            Self::EarLobe => "Ear Lobe",
            Self::Foot => "Foot",
            Self::Unknown(_) => "Unknown",
        }
    }
}

impl fmt::Display for BodySensorLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_label())
    }
}

/// Decodes standard Body Sensor Location (`0x2A38`).
pub fn parse_body_sensor_location(raw: u8) -> BodySensorLocation {
    match raw {
        0 => BodySensorLocation::Other,
        1 => BodySensorLocation::Chest,
        2 => BodySensorLocation::Wrist,
        3 => BodySensorLocation::Finger,
        4 => BodySensorLocation::Hand,
        5 => BodySensorLocation::EarLobe,
        6 => BodySensorLocation::Foot,
        other => BodySensorLocation::Unknown(other),
    }
}

/// Decodes a standard Bluetooth SIG Heart Rate Measurement payload (`0x2A37`) into an [`HrPacket`].
pub fn parse_hr_measurement(payload: &[u8], ts_ms: i64) -> Result<HrPacket, HrPacketError> {
    if payload.is_empty() {
        return Err(HrPacketError::Truncated {
            expected: 2,
            actual: 0,
        });
    }

    let flags = payload[0];
    let is_16_bit_hr = (flags & 0x01) != 0;
    let min_len = if is_16_bit_hr { 3 } else { 2 };

    if payload.len() < min_len {
        return Err(HrPacketError::Truncated {
            expected: min_len,
            actual: payload.len(),
        });
    }

    let (bpm, mut offset) = if is_16_bit_hr {
        let raw_bpm = u16::from_le_bytes([payload[1], payload[2]]);
        (f64::from(raw_bpm), 3)
    } else {
        (f64::from(payload[1]), 2)
    };

    let contact_bits = (flags >> 1) & 0x03;
    let contact_status = match contact_bits {
        0b10 => ContactStatus::NoContact,
        0b11 => ContactStatus::Contact,
        _ => ContactStatus::Unsupported,
    };

    let energy_expended_present = (flags & 0x08) != 0;
    if energy_expended_present {
        if payload.len() < offset + 2 {
            return Err(HrPacketError::Truncated {
                expected: offset + 2,
                actual: payload.len(),
            });
        }
        offset += 2;
    }

    let rr_present = (flags & 0x10) != 0;
    let mut rr_ms = Vec::new();
    if rr_present {
        let remaining = &payload[offset..];
        let rr_count = remaining.len() / 2;
        rr_ms.reserve(rr_count);
        for chunk in remaining.as_chunks::<2>().0 {
            let raw_rr = u16::from_le_bytes(*chunk);
            let ms = f64::from(raw_rr) * 1000.0 / 1024.0;
            rr_ms.push(ms);
        }
    }

    Ok(HrPacket {
        ts_ms,
        bpm,
        rr_ms,
        contact_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_huawei_watch_gt5_2_byte_payload() {
        // Huawei Watch GT5: Flags = 0x00 (8-bit BPM, contact unsupported, no EE, no RR), BPM = 68
        let payload = [0x00, 68];
        let pkt = parse_hr_measurement(&payload, 1_700_000_000_000).expect("should parse");
        assert_eq!(pkt.ts_ms, 1_700_000_000_000);
        assert_eq!(pkt.bpm, 68.0);
        assert_eq!(pkt.contact_status, ContactStatus::Unsupported);
        assert!(pkt.rr_ms.is_empty());

        // Huawei Watch GT5 with contact detected: Flags = 0x06 (bit 1 & 2 set = 0b11 = detected)
        let payload_contact = [0x06, 75];
        let pkt_c =
            parse_hr_measurement(&payload_contact, 1_700_000_000_001).expect("should parse");
        assert_eq!(pkt_c.bpm, 75.0);
        assert_eq!(pkt_c.contact_status, ContactStatus::Contact);
        assert!(pkt_c.rr_ms.is_empty());
    }

    #[test]
    fn parses_16_bit_bpm_and_sensor_contact_lost() {
        // Flags = 0x01 (16-bit BPM) | 0x04 (contact supported, not detected = 0b10) => 0x05
        // BPM = 300 (0x012C little-endian => [0x2C, 0x01])
        let payload = [0x05, 0x2C, 0x01];
        let pkt = parse_hr_measurement(&payload, 100).expect("should parse");
        assert_eq!(pkt.bpm, 300.0);
        assert_eq!(pkt.contact_status, ContactStatus::NoContact);
        assert!(pkt.rr_ms.is_empty());
    }

    #[test]
    fn parses_energy_expended_and_rr_intervals() {
        // Flags = 0x18: bit 3 (Energy Expended) + bit 4 (RR-intervals)
        // Offset 1: uint8 BPM = 60
        // Offset 2..4: Energy Expended = 500 kJ (0x01F4 => [0xF4, 0x01])
        // Offset 4..6: RR0 = 1024 raw => 1000.0 ms (0x0400 => [0x00, 0x04])
        // Offset 6..8: RR1 = 1075 raw => 1075 * 1000 / 1024 = 1049.8046875 ms
        let payload = [0x18, 60, 0xF4, 0x01, 0x00, 0x04, 0x33, 0x04];
        let pkt = parse_hr_measurement(&payload, 200).expect("should parse");
        assert_eq!(pkt.bpm, 60.0);
        assert_eq!(pkt.contact_status, ContactStatus::Unsupported);
        assert_eq!(pkt.rr_ms.len(), 2);
        assert!((pkt.rr_ms[0] - 1000.0).abs() < 1e-4);
        let expected_rr1 = 0x0433 as f64 * 1000.0 / 1024.0;
        assert!((pkt.rr_ms[1] - expected_rr1).abs() < 1e-4);
    }

    #[test]
    fn rejects_truncated_payloads() {
        assert!(parse_hr_measurement(&[], 0).is_err());
        assert!(parse_hr_measurement(&[0x00], 0).is_err()); // Needs BPM
        assert!(parse_hr_measurement(&[0x01, 70], 0).is_err()); // 16-bit BPM needs 2 bytes
        assert!(parse_hr_measurement(&[0x08, 70, 0x01], 0).is_err()); // EE needs 2 bytes
    }

    #[test]
    fn parses_all_contact_status_combinations() {
        // 0b00: unsupported
        let p0 = parse_hr_measurement(&[0x00, 60], 0).unwrap();
        assert_eq!(p0.contact_status, ContactStatus::Unsupported);

        // 0b01: unsupported
        let p1 = parse_hr_measurement(&[0x02, 60], 0).unwrap();
        assert_eq!(p1.contact_status, ContactStatus::Unsupported);

        // 0b10: supported, not detected (NoContact)
        let p2 = parse_hr_measurement(&[0x04, 60], 0).unwrap();
        assert_eq!(p2.contact_status, ContactStatus::NoContact);

        // 0b11: supported, detected (Contact)
        let p3 = parse_hr_measurement(&[0x06, 60], 0).unwrap();
        assert_eq!(p3.contact_status, ContactStatus::Contact);
    }

    #[test]
    fn ignores_trailing_odd_byte_in_rr_intervals() {
        // Flags = 0x10, BPM = 70, followed by 1 RR interval (2 bytes) + 1 trailing odd byte
        let payload = [0x10, 70, 0x00, 0x04, 0xFF];
        let pkt =
            parse_hr_measurement(&payload, 0).expect("should parse despite trailing odd byte");
        assert_eq!(pkt.bpm, 70.0);
        assert_eq!(pkt.rr_ms.len(), 1);
        assert!((pkt.rr_ms[0] - 1000.0).abs() < 1e-4);
    }

    #[test]
    fn parses_body_sensor_location() {
        assert_eq!(parse_body_sensor_location(0), BodySensorLocation::Other);
        assert_eq!(parse_body_sensor_location(1), BodySensorLocation::Chest);
        assert_eq!(parse_body_sensor_location(2), BodySensorLocation::Wrist);
        assert_eq!(
            parse_body_sensor_location(99),
            BodySensorLocation::Unknown(99)
        );
    }
}
