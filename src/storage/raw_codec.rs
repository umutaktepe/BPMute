//! Zero-dependency, `unsafe`-free lossless binary codec for `15 s` `HrPacket` chunks (ADR-0014).

use std::borrow::Borrow;

use crate::sleep_engine::{ContactStatus, HrPacket};
use crate::storage::StorageError;

const CHUNK_CODEC_VERSION: u8 = 1;
pub(crate) const RAW_CHUNK_MAX_SAMPLES: usize = 15;
const DEFAULT_TICK_MS: i128 = 1000;
const INITIAL_BPM_BITS: u64 = 60.0_f64.to_bits();
const INITIAL_BPM_HALF: i32 = 120;
const INITIAL_RR_TICKS: i32 = 1024;
const INITIAL_RR_MS: i32 = 1000;

/// Maximum half-BPM value (`1000.0 * 2 = 2000`) encoded via the compact delta path.
const MAX_COMPACT_BPM_HALF: i32 = 2000;

/// Encoded `raw_sample_chunks` payload along with its timestamp span and packet count.
pub(crate) struct EncodedRawChunk {
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub sample_count: u32,
    pub payload_blob: Vec<u8>,
}

struct CodecPredictor {
    prev_ts: i128,
    prev_bpm_bits: u64,
    prev_bpm_half: i32,
    prev_rr_ticks: i32,
    prev_rr_ms: i32,
}

impl CodecPredictor {
    fn new(base_ts_ms: i64) -> Self {
        Self {
            prev_ts: i128::from(base_ts_ms),
            prev_bpm_bits: INITIAL_BPM_BITS,
            prev_bpm_half: INITIAL_BPM_HALF,
            prev_rr_ticks: INITIAL_RR_TICKS,
            prev_rr_ms: INITIAL_RR_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum BpmMode {
    Unchanged = 0,
    HalfDelta = 1,
    ExactF64 = 2,
}

impl TryFrom<u8> for BpmMode {
    type Error = StorageError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Unchanged),
            1 => Ok(Self::HalfDelta),
            2 => Ok(Self::ExactF64),
            other => Err(StorageError::Validation(format!(
                "invalid bpm_mode {other} in raw chunk"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum RrEncMode {
    BleTicksDelta = 0,
    IntegerMsDelta = 1,
    ExactF64 = 2,
}

impl TryFrom<u8> for RrEncMode {
    type Error = StorageError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::BleTicksDelta),
            1 => Ok(Self::IntegerMsDelta),
            2 => Ok(Self::ExactF64),
            other => Err(StorageError::Validation(format!(
                "invalid rr_enc_mode {other} in raw chunk"
            ))),
        }
    }
}

/// Encodes a non-empty slice of packets (owned or borrowed) into an [`EncodedRawChunk`].
pub(crate) fn encode_raw_chunk_with_bounds<P: Borrow<HrPacket>>(
    packets: &[P],
) -> Result<EncodedRawChunk, StorageError> {
    if packets.is_empty() {
        return Err(StorageError::Validation(
            "cannot encode an empty raw_sample_chunks payload".to_string(),
        ));
    }

    let first_ts_ms = packets[0].borrow().ts_ms;
    let mut start_ts_ms = first_ts_ms;
    let mut end_ts_ms = first_ts_ms;

    let mut out = Vec::with_capacity(8 + packets.len() * 4);
    out.push(CHUNK_CODEC_VERSION);
    write_uvarint_u128(&mut out, packets.len() as u128);
    write_svarint_i128(&mut out, i128::from(first_ts_ms));

    let mut pred = CodecPredictor::new(first_ts_ms);

    for (idx, item) in packets.iter().enumerate() {
        let pkt = item.borrow();
        start_ts_ms = start_ts_ms.min(pkt.ts_ms);
        end_ts_ms = end_ts_ms.max(pkt.ts_ms);

        if idx > 0 {
            let cur_ts = i128::from(pkt.ts_ms);
            let dt_residual = cur_ts - pred.prev_ts - DEFAULT_TICK_MS;
            write_svarint_i128(&mut out, dt_residual);
            pred.prev_ts = cur_ts;
        }

        let contact_code: u8 = match pkt.contact_status {
            ContactStatus::Unsupported => 0,
            ContactStatus::NoContact => 1,
            ContactStatus::Contact => 2,
        };

        let bpm_bits = pkt.bpm.to_bits();
        let (bpm_mode, compact_half) = if bpm_bits == pred.prev_bpm_bits {
            (BpmMode::Unchanged, None)
        } else if let Some(half) = try_exact_half_bpm(pkt.bpm) {
            (BpmMode::HalfDelta, Some(half))
        } else {
            (BpmMode::ExactF64, None)
        };

        let rr_count_code: u8 = match pkt.rr_ms.len() {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 3,
        };

        let rr_enc_mode = if pkt.rr_ms.is_empty()
            || pkt.rr_ms.iter().all(|&r| try_exact_ble_ticks(r).is_some())
        {
            RrEncMode::BleTicksDelta
        } else if pkt.rr_ms.iter().all(|&r| try_exact_int_ms(r).is_some()) {
            RrEncMode::IntegerMsDelta
        } else {
            RrEncMode::ExactF64
        };

        let flags = contact_code
            | ((bpm_mode as u8) << 2)
            | (rr_count_code << 4)
            | ((rr_enc_mode as u8) << 6);
        out.push(flags);

        match bpm_mode {
            BpmMode::Unchanged => {}
            BpmMode::HalfDelta => {
                let half = compact_half.expect("HalfDelta sets compact_half");
                let delta = i128::from(half) - i128::from(pred.prev_bpm_half);
                write_svarint_i128(&mut out, delta);
                pred.prev_bpm_half = half;
                pred.prev_bpm_bits = bpm_bits;
            }
            BpmMode::ExactF64 => {
                out.extend_from_slice(&pkt.bpm.to_le_bytes());
                pred.prev_bpm_bits = bpm_bits;
            }
        }

        if !pkt.rr_ms.is_empty() {
            if rr_count_code == 3 {
                write_uvarint_u128(&mut out, pkt.rr_ms.len() as u128);
            }
            match rr_enc_mode {
                RrEncMode::BleTicksDelta => {
                    for &r in &pkt.rr_ms {
                        let ticks = i32::from(try_exact_ble_ticks(r).expect("validated BLE ticks"));
                        write_svarint_i128(
                            &mut out,
                            i128::from(ticks) - i128::from(pred.prev_rr_ticks),
                        );
                        pred.prev_rr_ticks = ticks;
                    }
                }
                RrEncMode::IntegerMsDelta => {
                    for &r in &pkt.rr_ms {
                        let ms = i32::from(try_exact_int_ms(r).expect("validated integer ms"));
                        write_svarint_i128(&mut out, i128::from(ms) - i128::from(pred.prev_rr_ms));
                        pred.prev_rr_ms = ms;
                    }
                }
                RrEncMode::ExactF64 => {
                    for &r in &pkt.rr_ms {
                        out.extend_from_slice(&r.to_le_bytes());
                    }
                }
            }
        }
    }

    Ok(EncodedRawChunk {
        start_ts_ms,
        end_ts_ms,
        sample_count: packets.len() as u32,
        payload_blob: out,
    })
}

/// Decodes a binary chunk blob back into the exact sequence of [`HrPacket`] values.
pub(crate) fn decode_raw_chunk(blob: &[u8]) -> Result<Vec<HrPacket>, StorageError> {
    let mut cursor = 0usize;
    let version = read_u8(blob, &mut cursor)?;
    if version != CHUNK_CODEC_VERSION {
        return Err(StorageError::Validation(format!(
            "unsupported raw sample chunk codec version {version}"
        )));
    }

    let count_u128 = read_uvarint_u128(blob, &mut cursor)?;
    let count = usize::try_from(count_u128)
        .map_err(|_| StorageError::Validation("raw chunk packet count overflow".to_string()))?;
    if count == 0 {
        return Err(StorageError::Validation(
            "raw sample chunk must contain at least one packet".to_string(),
        ));
    }
    if count > blob.len().saturating_sub(cursor) {
        return Err(StorageError::Validation(
            "truncated raw sample chunk: packet count exceeds payload bytes".to_string(),
        ));
    }

    let base_ts_i128 = read_svarint_i128(blob, &mut cursor)?;
    let base_ts_ms = i64::try_from(base_ts_i128).map_err(|_| {
        StorageError::Validation("raw chunk base timestamp out of range".to_string())
    })?;

    let mut packets = Vec::with_capacity(count);
    let mut pred = CodecPredictor::new(base_ts_ms);

    for idx in 0..count {
        let ts_ms = if idx == 0 {
            base_ts_ms
        } else {
            let dt_residual = read_svarint_i128(blob, &mut cursor)?;
            let cur_ts = pred
                .prev_ts
                .checked_add(DEFAULT_TICK_MS)
                .and_then(|v| v.checked_add(dt_residual))
                .ok_or_else(|| {
                    StorageError::Validation("raw chunk timestamp delta overflow".to_string())
                })?;
            pred.prev_ts = cur_ts;
            i64::try_from(cur_ts).map_err(|_| {
                StorageError::Validation("raw chunk decoded timestamp out of i64 range".to_string())
            })?
        };

        let flags = read_u8(blob, &mut cursor)?;
        let contact_status = match flags & 0x03 {
            0 => ContactStatus::Unsupported,
            1 => ContactStatus::NoContact,
            2 => ContactStatus::Contact,
            other => {
                return Err(StorageError::Validation(format!(
                    "invalid contact_status code {other} in raw chunk"
                )));
            }
        };

        let bpm = match BpmMode::try_from((flags >> 2) & 0x03)? {
            BpmMode::Unchanged => f64::from_bits(pred.prev_bpm_bits),
            BpmMode::HalfDelta => {
                let delta = read_svarint_i128(blob, &mut cursor)?;
                let half_i128 = i128::from(pred.prev_bpm_half) + delta;
                let half = i32::try_from(half_i128).map_err(|_| {
                    StorageError::Validation("raw chunk bpm half-step out of range".to_string())
                })?;
                if !(0..=MAX_COMPACT_BPM_HALF).contains(&half) {
                    return Err(StorageError::Validation(format!(
                        "raw chunk half-step bpm {half} out of bounds"
                    )));
                }
                let val = f64::from(half) * 0.5;
                pred.prev_bpm_half = half;
                pred.prev_bpm_bits = val.to_bits();
                val
            }
            BpmMode::ExactF64 => {
                let val = read_f64_le(blob, &mut cursor)?;
                pred.prev_bpm_bits = val.to_bits();
                val
            }
        };

        let rr_count = match (flags >> 4) & 0x03 {
            0 => 0usize,
            1 => 1usize,
            2 => 2usize,
            3 => {
                let n = read_uvarint_u128(blob, &mut cursor)?;
                usize::try_from(n).map_err(|_| {
                    StorageError::Validation("raw chunk rr_ms count overflow".to_string())
                })?
            }
            _ => unreachable!(),
        };

        let rr_enc_mode = RrEncMode::try_from((flags >> 6) & 0x03)?;
        let mut rr_ms = Vec::new();
        if rr_count > 0 {
            let min_bytes_per_rr = if rr_enc_mode == RrEncMode::ExactF64 {
                8
            } else {
                1
            };
            let needed = rr_count.saturating_mul(min_bytes_per_rr);
            if needed > blob.len().saturating_sub(cursor) {
                return Err(StorageError::Validation(
                    "truncated raw sample chunk: rr_ms count exceeds remaining bytes".to_string(),
                ));
            }
            rr_ms.reserve_exact(rr_count);
            match rr_enc_mode {
                RrEncMode::BleTicksDelta => {
                    for _ in 0..rr_count {
                        let delta = read_svarint_i128(blob, &mut cursor)?;
                        let ticks_i128 = i128::from(pred.prev_rr_ticks) + delta;
                        let ticks = u16::try_from(ticks_i128).map_err(|_| {
                            StorageError::Validation(
                                "raw chunk BLE RR ticks out of range".to_string(),
                            )
                        })?;
                        pred.prev_rr_ticks = i32::from(ticks);
                        rr_ms.push(f64::from(ticks) * 1000.0 / 1024.0);
                    }
                }
                RrEncMode::IntegerMsDelta => {
                    for _ in 0..rr_count {
                        let delta = read_svarint_i128(blob, &mut cursor)?;
                        let ms_i128 = i128::from(pred.prev_rr_ms) + delta;
                        let ms = u16::try_from(ms_i128).map_err(|_| {
                            StorageError::Validation(
                                "raw chunk integer RR ms out of range".to_string(),
                            )
                        })?;
                        pred.prev_rr_ms = i32::from(ms);
                        rr_ms.push(f64::from(ms));
                    }
                }
                RrEncMode::ExactF64 => {
                    for _ in 0..rr_count {
                        rr_ms.push(read_f64_le(blob, &mut cursor)?);
                    }
                }
            }
        }

        packets.push(HrPacket {
            ts_ms,
            bpm,
            rr_ms,
            contact_status,
        });
    }

    if cursor != blob.len() {
        return Err(StorageError::Validation(format!(
            "raw sample chunk has {} trailing unconsumed bytes",
            blob.len() - cursor
        )));
    }

    Ok(packets)
}

fn try_exact_half_bpm(bpm: f64) -> Option<i32> {
    if !bpm.is_finite() || !(0.0..=1000.0).contains(&bpm) {
        return None;
    }
    let doubled = (bpm * 2.0).round();
    if !(0.0..=f64::from(MAX_COMPACT_BPM_HALF)).contains(&doubled) {
        return None;
    }
    let half = doubled as i32;
    let reconstructed = f64::from(half) * 0.5;
    (reconstructed.to_bits() == bpm.to_bits()).then_some(half)
}

fn try_exact_ble_ticks(rr_ms: f64) -> Option<u16> {
    if !rr_ms.is_finite() || !(0.0..=64000.0).contains(&rr_ms) {
        return None;
    }
    let rounded = (rr_ms * 1.024).round();
    if !(0.0..=f64::from(u16::MAX)).contains(&rounded) {
        return None;
    }
    let ticks = rounded as u16;
    let reconstructed = f64::from(ticks) * 1000.0 / 1024.0;
    (reconstructed.to_bits() == rr_ms.to_bits()).then_some(ticks)
}

fn try_exact_int_ms(rr_ms: f64) -> Option<u16> {
    if !rr_ms.is_finite() || !(0.0..=f64::from(u16::MAX)).contains(&rr_ms) {
        return None;
    }
    let rounded = rr_ms.round();
    if !(0.0..=f64::from(u16::MAX)).contains(&rounded) {
        return None;
    }
    let ms = rounded as u16;
    (f64::from(ms).to_bits() == rr_ms.to_bits()).then_some(ms)
}

fn write_uvarint_u128(out: &mut Vec<u8>, mut value: u128) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn write_svarint_i128(out: &mut Vec<u8>, value: i128) {
    let zigzag = ((value << 1) ^ (value >> 127)) as u128;
    write_uvarint_u128(out, zigzag);
}

fn read_u8(blob: &[u8], cursor: &mut usize) -> Result<u8, StorageError> {
    let byte = *blob
        .get(*cursor)
        .ok_or_else(|| StorageError::Validation("truncated raw sample chunk".to_string()))?;
    *cursor += 1;
    Ok(byte)
}

fn read_f64_le(blob: &[u8], cursor: &mut usize) -> Result<f64, StorageError> {
    let end = cursor
        .checked_add(8)
        .filter(|&e| e <= blob.len())
        .ok_or_else(|| StorageError::Validation("truncated f64 in raw sample chunk".to_string()))?;
    let bytes: [u8; 8] = blob[*cursor..end]
        .try_into()
        .expect("slice length checked to 8 bytes");
    *cursor = end;
    Ok(f64::from_le_bytes(bytes))
}

fn read_uvarint_u128(blob: &[u8], cursor: &mut usize) -> Result<u128, StorageError> {
    let mut acc = 0_u128;
    let mut shift = 0_u32;
    loop {
        let byte = read_u8(blob, cursor)?;
        let low = u128::from(byte & 0x7f);
        if shift >= 128 || (shift == 126 && low > 3) {
            return Err(StorageError::Validation(
                "varint overflow in raw sample chunk".to_string(),
            ));
        }
        acc |= low << shift;
        if (byte & 0x80) == 0 {
            return Ok(acc);
        }
        shift += 7;
    }
}

fn read_svarint_i128(blob: &[u8], cursor: &mut usize) -> Result<i128, StorageError> {
    let zigzag = read_uvarint_u128(blob, cursor)?;
    let decoded = ((zigzag >> 1) as i128) ^ (-((zigzag & 1) as i128));
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_integer_fractional_ble_rr_and_all_contact_variants_bit_exact() {
        let base = 1_700_000_000_000_i64;
        let packets = vec![
            HrPacket {
                ts_ms: base,
                bpm: 60.0,
                rr_ms: vec![],
                contact_status: ContactStatus::Unsupported,
            },
            HrPacket {
                ts_ms: base + 1000,
                bpm: 60.0,
                rr_ms: vec![f64::from(983_u16) * 1000.0 / 1024.0],
                contact_status: ContactStatus::Contact,
            },
            HrPacket {
                ts_ms: base + 2000,
                bpm: 61.5,
                rr_ms: vec![960.0, 975.0],
                contact_status: ContactStatus::NoContact,
            },
            HrPacket {
                ts_ms: base + 3125,
                bpm: 61.375,
                rr_ms: vec![923.45, 931.125, 940.0],
                contact_status: ContactStatus::Contact,
            },
            HrPacket {
                ts_ms: base - 500,
                bpm: -0.0,
                rr_ms: vec![0.0, 1200.5],
                contact_status: ContactStatus::Unsupported,
            },
        ];
        let encoded = encode_raw_chunk_with_bounds(&packets).unwrap();
        let decoded = decode_raw_chunk(&encoded.payload_blob).unwrap();
        assert_eq!(encoded.start_ts_ms, base - 500);
        assert_eq!(encoded.end_ts_ms, base + 3125);
        assert_eq!(encoded.sample_count, 5);
        assert_eq!(decoded.len(), packets.len());
        for (orig, dec) in packets.iter().zip(decoded.iter()) {
            assert_eq!(dec.ts_ms, orig.ts_ms);
            assert_eq!(dec.bpm.to_bits(), orig.bpm.to_bits());
            assert_eq!(dec.contact_status, orig.contact_status);
            assert_eq!(dec.rr_ms.len(), orig.rr_ms.len());
            for (r0, r1) in orig.rr_ms.iter().zip(dec.rr_ms.iter()) {
                assert_eq!(r1.to_bits(), r0.to_bits());
            }
        }
    }

    #[test]
    fn rejects_empty_truncated_and_malformed_chunks() {
        let empty: &[HrPacket] = &[];
        assert!(encode_raw_chunk_with_bounds(empty).is_err());
        assert!(decode_raw_chunk(&[]).is_err());
        assert!(decode_raw_chunk(&[99, 1, 0]).is_err());

        let valid = encode_raw_chunk_with_bounds(&[HrPacket {
            ts_ms: 1_700_000_000_000,
            bpm: 64.0,
            rr_ms: vec![940.0],
            contact_status: ContactStatus::Contact,
        }])
        .unwrap()
        .payload_blob;
        assert!(decode_raw_chunk(&valid[..valid.len() - 1]).is_err());

        let mut trailing = valid.clone();
        trailing.push(0x00);
        assert!(decode_raw_chunk(&trailing).is_err());

        // Malicious huge count must fail cleanly without OOM.
        let huge_count = [CHUNK_CODEC_VERSION, 0xff, 0xff, 0xff, 0xff, 0x0f, 0x00];
        assert!(decode_raw_chunk(&huge_count).is_err());
    }
}
