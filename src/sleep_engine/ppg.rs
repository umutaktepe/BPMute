//! 4-stage causal PPG signal filtering and 15-second `Epoch` feature extraction.

use std::collections::VecDeque;

use crate::sleep_engine::types::{ContactStatus, HrPacket, WearableFloor};

/// Epoch duration in milliseconds (`15 s`).
pub const EPOCH_DURATION_MS: i64 = 15_000;

/// Minimum physiological baseline scale floor (`2.0 bpm`) per ADR-0004.
pub const SIGMA_MIN_BPM: f64 = 2.0;

const HAMPEL_WINDOW_MS: i64 = 25_000;
const HAMPEL_K: f64 = 3.0;
const HAMPEL_SCALE_FLOOR_BPM: f64 = 1.0;

const HR_MEAN_WINDOW_MS: i64 = 60_000;
const HR_VAR_WINDOW_MS: i64 = 120_000;
const HR_SLOPE_WINDOW_MS: i64 = 240_000;
const RR_WINDOW_MS: i64 = 120_000;
const SQI_WINDOW_MS: i64 = 60_000;

const LT_QD_WINDOW_BEATS: usize = 91;
const LT_MED_WINDOW_BEATS: usize = 11;
const MIN_BEATS_FOR_HRV: usize = 25;
const MAX_RR_ARTIFACT_FRAC: f64 = 0.05;
pub(crate) const MIN_EPOCH_COVERAGE: f64 = 0.80;

const FLATLINE_BPM_RR_MS: i64 = 30_000;
const FLATLINE_BPM_ONLY_MS: i64 = 120_000;

/// Pre-baseline physiological summary emitted every 15-second `Epoch`.
#[derive(Debug, Clone, PartialEq)]
pub struct RawEpochMetrics {
    pub ts_ms: i64,
    pub hr_bpm_60s: f64,
    pub hr_slope_4m: f64,
    pub local_mad_120s: f64,
    pub ln_rmssd_60s: Option<f64>,
    pub rmssd_sdnn_120s: Option<f64>,
    pub hrv_valid: bool,
    pub rr_artifact_frac: f64,
    pub coverage: f64,
    pub flatline_stale: bool,
    pub accumulators_frozen: bool,
}

trait Timestamped {
    fn ts_ms(&self) -> i64;
}

#[derive(Debug, Clone, Copy)]
struct TimedValue {
    ts_ms: i64,
    val: f64,
}

impl Timestamped for TimedValue {
    fn ts_ms(&self) -> i64 {
        self.ts_ms
    }
}

#[derive(Debug, Clone, Copy)]
struct TimedBeat {
    ts_ms: i64,
    rr_ms: f64,
    corrected: bool,
}

impl Timestamped for TimedBeat {
    fn ts_ms(&self) -> i64 {
        self.ts_ms
    }
}

/// Pure-synchronous 4-stage causal PPG filter and `15 s` feature aggregator.
#[derive(Debug, Clone)]
pub struct PpgPipeline {
    epoch_start_ms: Option<i64>,
    last_packet_ts_ms: Option<i64>,
    epoch_total_packets: usize,
    epoch_valid_non_flatline_packets: usize,

    // Stage 2a: Lipponen-Tarvainen causal RR cleaner
    last_clean_rr: Option<f64>,
    last_rr_packet_ts_ms: Option<i64>,
    clean_rr_med11: VecDeque<f64>,
    d_rr_qd91: VecDeque<f64>,
    m_rr_qd91: VecDeque<f64>,
    rr_120s: VecDeque<TimedBeat>,

    // Stage 2b: Causal 25 s Hampel BPM filter & rolling filtered HR buffers
    raw_bpm_25s: VecDeque<TimedValue>,
    filtered_bpm_240s: VecDeque<TimedValue>,

    // Stage 3: Mode-aware flatline detector
    last_raw_bpm: Option<f64>,
    flatline_since_ms: Option<i64>,
    flatline_rr_count: usize,
    last_flatline_rr: Option<f64>,
    rr_flatline_since_ms: Option<i64>,
    flatline_stale: bool,

    // Reusable scratch buffers & cached Lipponen-Tarvainen thresholds
    scratch: Vec<f64>,
    dev_scratch: Vec<f64>,
    cached_th1: f64,
    cached_th2: f64,
    clean_beats_since_th_update: usize,
}

impl Default for PpgPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl PpgPipeline {
    /// Creates an empty PPG filtering pipeline.
    pub fn new() -> Self {
        Self {
            epoch_start_ms: None,
            last_packet_ts_ms: None,
            epoch_total_packets: 0,
            epoch_valid_non_flatline_packets: 0,
            last_clean_rr: None,
            last_rr_packet_ts_ms: None,
            clean_rr_med11: VecDeque::with_capacity(LT_MED_WINDOW_BEATS),
            d_rr_qd91: VecDeque::with_capacity(LT_QD_WINDOW_BEATS),
            m_rr_qd91: VecDeque::with_capacity(LT_QD_WINDOW_BEATS),
            rr_120s: VecDeque::new(),
            raw_bpm_25s: VecDeque::new(),
            filtered_bpm_240s: VecDeque::new(),
            last_raw_bpm: None,
            flatline_since_ms: None,
            flatline_rr_count: 0,
            last_flatline_rr: None,
            rr_flatline_since_ms: None,
            flatline_stale: false,
            scratch: Vec::with_capacity(LT_QD_WINDOW_BEATS),
            dev_scratch: Vec::with_capacity(LT_QD_WINDOW_BEATS),
            cached_th1: 150.0,
            cached_th2: 150.0,
            clean_beats_since_th_update: 0,
        }
    }

    /// Sets the start anchor for the next 15-second `Epoch`.
    pub fn set_epoch_anchor(&mut self, ts_ms: i64) {
        self.epoch_start_ms = Some(ts_ms);
        self.epoch_total_packets = 0;
        self.epoch_valid_non_flatline_packets = 0;
    }

    /// Resets flatline state and short-term epoch packet counters across a pause/gap.
    pub fn on_short_gap_resume(&mut self, ts_ms: i64) {
        self.set_epoch_anchor(ts_ms);
        self.last_raw_bpm = None;
        self.flatline_since_ms = None;
        self.flatline_rr_count = 0;
        self.last_flatline_rr = None;
        self.rr_flatline_since_ms = None;
        self.flatline_stale = false;
    }

    /// Clears rolling temporal windows after a Tier-2 (`2-15 min`) gap so pre-gap samples do not
    /// skew post-gap slopes or SQI windows.
    pub fn clear_rolling_windows(&mut self, ts_ms: i64) {
        self.on_short_gap_resume(ts_ms);
        self.last_clean_rr = None;
        self.last_rr_packet_ts_ms = None;
        self.clean_rr_med11.clear();
        self.d_rr_qd91.clear();
        self.m_rr_qd91.clear();
        self.rr_120s.clear();
        self.raw_bpm_25s.clear();
        self.filtered_bpm_240s.clear();
        self.cached_th1 = 150.0;
        self.cached_th2 = 150.0;
        self.clean_beats_since_th_update = 0;
    }

    /// Ingests one `HrPacket`, running Stages 1-3 and emitting a `RawEpochMetrics` whenever a
    /// `15 s` bucket completes.
    pub fn ingest_packet(&mut self, packet: &HrPacket) -> Option<RawEpochMetrics> {
        if let Some(prev_ts) = self.last_packet_ts_ms
            && packet.ts_ms < prev_ts
        {
            return None;
        }
        self.last_packet_ts_ms = Some(packet.ts_ms);

        let epoch_start = *self.epoch_start_ms.get_or_insert(packet.ts_ms);
        self.epoch_total_packets += 1;

        if Self::is_packet_valid_stage1(packet) {
            self.update_flatline_tracker(packet);
            self.ingest_rr_beats(packet);
            let filtered_bpm = self.apply_hampel_25s(packet.ts_ms, packet.bpm);
            if !self.flatline_stale {
                self.epoch_valid_non_flatline_packets += 1;
                push_timed_evicting(
                    &mut self.filtered_bpm_240s,
                    packet.ts_ms,
                    filtered_bpm,
                    HR_SLOPE_WINDOW_MS,
                );
            }
        }

        if packet.ts_ms - epoch_start < EPOCH_DURATION_MS {
            return None;
        }

        let metrics = self.build_epoch_metrics(epoch_start, packet.ts_ms);
        let next_anchor = if packet.ts_ms - epoch_start < 2 * EPOCH_DURATION_MS {
            epoch_start + EPOCH_DURATION_MS
        } else {
            packet.ts_ms
        };
        self.set_epoch_anchor(next_anchor);
        Some(metrics)
    }

    pub(crate) fn is_packet_valid_stage1(packet: &HrPacket) -> bool {
        packet.contact_status != ContactStatus::NoContact
            && packet.bpm.is_finite()
            && packet.bpm > 0.0
            && (30.0..=200.0).contains(&packet.bpm)
    }

    fn update_flatline_tracker(&mut self, packet: &HrPacket) {
        if !packet.rr_ms.is_empty() {
            self.last_rr_packet_ts_ms = Some(packet.ts_ms);
        }
        let has_active_rr = !packet.rr_ms.is_empty()
            || self
                .last_rr_packet_ts_ms
                .is_some_and(|t| packet.ts_ms - t <= 3_000);
        if !has_active_rr {
            self.last_flatline_rr = None;
            self.rr_flatline_since_ms = None;
            self.flatline_rr_count = 0;
        }

        let same_bpm = self
            .last_raw_bpm
            .is_some_and(|prev| (packet.bpm - prev).abs() < 1e-9);

        if !same_bpm {
            self.last_raw_bpm = Some(packet.bpm);
            self.flatline_since_ms = Some(packet.ts_ms);
            self.flatline_rr_count = 0;
            self.last_flatline_rr = None;
            self.rr_flatline_since_ms = None;
            self.flatline_stale = false;
        }

        for &rr in &packet.rr_ms {
            if rr.is_finite() && (300.0..=2000.0).contains(&rr) {
                if self
                    .last_flatline_rr
                    .is_some_and(|prev| (rr - prev).abs() <= 1.0)
                {
                    self.flatline_rr_count += 1;
                } else {
                    self.last_flatline_rr = Some(rr);
                    self.rr_flatline_since_ms = Some(packet.ts_ms);
                    self.flatline_rr_count = 1;
                }
            }
        }

        let run_ms = self
            .flatline_since_ms
            .map_or(0, |since| packet.ts_ms - since);

        self.flatline_stale = if has_active_rr {
            run_ms >= FLATLINE_BPM_RR_MS
                && self.flatline_rr_count >= 5
                && self
                    .rr_flatline_since_ms
                    .is_some_and(|t| packet.ts_ms - t >= FLATLINE_BPM_RR_MS)
        } else {
            run_ms >= FLATLINE_BPM_ONLY_MS
        };
    }

    fn ingest_rr_beats(&mut self, packet: &HrPacket) {
        if packet.rr_ms.is_empty() {
            evict_older_than(&mut self.rr_120s, packet.ts_ms, RR_WINDOW_MS);
            return;
        }

        for &rr in &packet.rr_ms {
            if !rr.is_finite() || !(300.0..=2000.0).contains(&rr) {
                let replacement = self
                    .med11()
                    .unwrap_or_else(|| (60_000.0 / packet.bpm).clamp(300.0, 2000.0));
                self.push_beat(packet.ts_ms, replacement, true);
                continue;
            }

            let Some(prev_clean) = self.last_clean_rr else {
                self.accept_clean_rr(rr, None, None);
                self.push_beat(packet.ts_ms, rr, false);
                continue;
            };

            let med11 = self.med11().unwrap_or(prev_clean);
            let d_rr = rr - prev_clean;
            let m_rr = rr - med11;
            let (th1, th2) = self.lt_thresholds(med11);

            if rr > 1.75 * med11 && (rr * 0.5 - med11).abs() < 2.0 * th2 {
                let half = rr * 0.5;
                self.push_beat(packet.ts_ms, half, true);
                self.push_beat(packet.ts_ms, half, false);
                self.last_clean_rr = Some(half);
            } else if rr < 0.65 * med11 || d_rr.abs() > th1 || m_rr.abs() > th2 {
                self.push_beat(packet.ts_ms, med11, true);
                self.last_clean_rr = Some(med11);
            } else {
                self.accept_clean_rr(rr, Some(d_rr), Some(m_rr));
                self.push_beat(packet.ts_ms, rr, false);
            }
        }

        evict_older_than(&mut self.rr_120s, packet.ts_ms, RR_WINDOW_MS);
    }

    fn med11(&mut self) -> Option<f64> {
        if self.clean_rr_med11.is_empty() {
            return None;
        }
        self.scratch.clear();
        self.scratch.extend(
            self.clean_rr_med11
                .iter()
                .copied()
                .filter(|v| v.is_finite()),
        );
        median_in_place(&mut self.scratch)
    }

    fn lt_thresholds(&mut self, med11: f64) -> (f64, f64) {
        if self.d_rr_qd91.len() >= LT_MED_WINDOW_BEATS
            && self.m_rr_qd91.len() >= LT_MED_WINDOW_BEATS
        {
            if self.d_rr_qd91.len() <= 15 || self.clean_beats_since_th_update >= 10 {
                self.scratch.clear();
                self.scratch.extend(self.d_rr_qd91.iter().copied());
                let qd_drr = (quantile(&self.scratch, 0.75).unwrap_or(0.0)
                    - quantile(&self.scratch, 0.25).unwrap_or(0.0))
                    * 0.5;
                self.scratch.clear();
                self.scratch.extend(self.m_rr_qd91.iter().copied());
                let qd_mrr = (quantile(&self.scratch, 0.75).unwrap_or(0.0)
                    - quantile(&self.scratch, 0.25).unwrap_or(0.0))
                    * 0.5;
                self.cached_th1 = (5.2 * qd_drr).max(35.0);
                self.cached_th2 = (5.2 * qd_mrr).max(45.0);
                self.clean_beats_since_th_update = 0;
            }
            (self.cached_th1, self.cached_th2)
        } else {
            let fallback = (0.20 * med11).max(150.0);
            (fallback, fallback)
        }
    }

    fn accept_clean_rr(&mut self, rr: f64, d_rr: Option<f64>, m_rr: Option<f64>) {
        self.last_clean_rr = Some(rr);
        push_bounded(&mut self.clean_rr_med11, rr, LT_MED_WINDOW_BEATS);
        if let Some(d) = d_rr {
            push_bounded(&mut self.d_rr_qd91, d, LT_QD_WINDOW_BEATS);
        }
        if let Some(m) = m_rr {
            push_bounded(&mut self.m_rr_qd91, m, LT_QD_WINDOW_BEATS);
        }
        self.clean_beats_since_th_update += 1;
    }

    fn push_beat(&mut self, ts_ms: i64, rr_ms: f64, corrected: bool) {
        self.rr_120s.push_back(TimedBeat {
            ts_ms,
            rr_ms,
            corrected,
        });
    }

    fn apply_hampel_25s(&mut self, ts_ms: i64, raw_bpm: f64) -> f64 {
        push_timed_evicting(&mut self.raw_bpm_25s, ts_ms, raw_bpm, HAMPEL_WINDOW_MS);
        if self.raw_bpm_25s.len() < 5 {
            return raw_bpm;
        }
        self.scratch.clear();
        self.scratch.extend(self.raw_bpm_25s.iter().map(|v| v.val));
        let Some(med) = median_in_place(&mut self.scratch) else {
            return raw_bpm;
        };
        self.dev_scratch.clear();
        self.dev_scratch
            .extend(self.scratch.iter().map(|&v| (v - med).abs()));
        let s_mad = median_in_place(&mut self.dev_scratch).map_or(0.0, |m| 1.4826 * m);
        let scale = s_mad.max(HAMPEL_SCALE_FLOOR_BPM);
        if (raw_bpm - med).abs() > HAMPEL_K * scale {
            med
        } else {
            raw_bpm
        }
    }

    fn build_epoch_metrics(&mut self, epoch_start: i64, ts_ms: i64) -> RawEpochMetrics {
        evict_older_than(&mut self.rr_120s, ts_ms, RR_WINDOW_MS);
        evict_older_than(&mut self.filtered_bpm_240s, ts_ms, HR_SLOPE_WINDOW_MS);

        let expected_packets = (((ts_ms - epoch_start) as f64 / 1000.0).round())
            .max(15.0)
            .max(self.epoch_total_packets as f64);
        let coverage =
            (self.epoch_valid_non_flatline_packets as f64 / expected_packets).clamp(0.0, 1.0);
        let accumulators_frozen = self.flatline_stale || coverage < MIN_EPOCH_COVERAGE;

        let hr_bpm_60s = self
            .mean_filtered_hr(ts_ms, HR_MEAN_WINDOW_MS)
            .or(self.last_raw_bpm)
            .unwrap_or(65.0);

        let hr_slope_4m = self.compute_hr_slope_4m();

        let vals_120s: Vec<TimedValue> = self
            .filtered_bpm_240s
            .iter()
            .copied()
            .filter(|v| ts_ms - v.ts_ms <= HR_VAR_WINDOW_MS)
            .collect();
        let local_mad_120s = detrended_scaled_mad(&vals_120s).unwrap_or(SIGMA_MIN_BPM);

        let sqi_beats: Vec<TimedBeat> = self
            .rr_120s
            .iter()
            .copied()
            .filter(|b| ts_ms - b.ts_ms <= SQI_WINDOW_MS)
            .collect();
        let sqi_total = sqi_beats.len();
        let sqi_corrected = sqi_beats.iter().filter(|b| b.corrected).count();
        let rr_artifact_frac = if sqi_total > 0 {
            sqi_corrected as f64 / sqi_total as f64
        } else {
            0.0
        };

        let hrv_valid = !self.flatline_stale
            && !accumulators_frozen
            && sqi_total >= MIN_BEATS_FOR_HRV
            && rr_artifact_frac <= MAX_RR_ARTIFACT_FRAC;

        let (ln_rmssd_60s, rmssd_sdnn_120s) = if hrv_valid {
            let rmssd_60 = compute_rmssd(&sqi_beats);
            let beats_120: Vec<TimedBeat> = self.rr_120s.iter().copied().collect();
            let rmssd_120 = compute_rmssd(&beats_120);
            let sdnn_120 = compute_sdnn(&beats_120);
            let ratio = match (rmssd_120, sdnn_120) {
                (Some(r), Some(s)) => Some((r / s.max(1.0)).clamp(0.0, 2.5)),
                _ => None,
            };
            (rmssd_60.map(|r| r.max(1.0).ln()), ratio)
        } else {
            (None, None)
        };

        RawEpochMetrics {
            ts_ms,
            hr_bpm_60s,
            hr_slope_4m,
            local_mad_120s,
            ln_rmssd_60s,
            rmssd_sdnn_120s,
            hrv_valid: hrv_valid && ln_rmssd_60s.is_some() && rmssd_sdnn_120s.is_some(),
            rr_artifact_frac,
            coverage,
            flatline_stale: self.flatline_stale,
            accumulators_frozen,
        }
    }

    fn mean_filtered_hr(&self, ts_ms: i64, window_ms: i64) -> Option<f64> {
        let mut sum = 0.0;
        let mut count = 0_usize;
        for sample in &self.filtered_bpm_240s {
            if ts_ms - sample.ts_ms <= window_ms {
                sum += sample.val;
                count += 1;
            }
        }
        (count > 0).then(|| sum / count as f64)
    }

    fn compute_hr_slope_4m(&self) -> f64 {
        if self.filtered_bpm_240s.len() < 4 {
            return 0.0;
        }
        let first_ts = self.filtered_bpm_240s.front().map_or(0, |s| s.ts_ms);
        let last_ts = self.filtered_bpm_240s.back().map_or(0, |s| s.ts_ms);
        if last_ts - first_ts < 60_000 {
            return 0.0;
        }
        let points: Vec<TimedValue> = self.filtered_bpm_240s.iter().copied().collect();
        ols_trend(&points).map_or(0.0, |trend| trend.slope)
    }
}

struct OlsTrend {
    first_ts: i64,
    t_mean: f64,
    y_mean: f64,
    slope: f64,
}

fn ols_trend(series: &[TimedValue]) -> Option<OlsTrend> {
    let first_ts = series.first()?.ts_ms;
    let n = series.len() as f64;
    let mut t_sum = 0.0;
    let mut y_sum = 0.0;
    for sample in series {
        t_sum += (sample.ts_ms - first_ts) as f64 / 60_000.0;
        y_sum += sample.val;
    }
    let t_mean = t_sum / n;
    let y_mean = y_sum / n;

    let mut num = 0.0;
    let mut den = 0.0;
    for sample in series {
        let dt = (sample.ts_ms - first_ts) as f64 / 60_000.0 - t_mean;
        let dy = sample.val - y_mean;
        num += dt * dy;
        den += dt * dt;
    }
    let slope = if den <= 1e-12 { 0.0 } else { num / den };
    Some(OlsTrend {
        first_ts,
        t_mean,
        y_mean,
        slope,
    })
}

fn detrended_scaled_mad(points: &[TimedValue]) -> Option<f64> {
    let smoothed;
    let series: &[TimedValue] = if points.len() >= 5 {
        smoothed = points
            .windows(5)
            .map(|w| TimedValue {
                ts_ms: w[2].ts_ms,
                val: w.iter().map(|p| p.val).sum::<f64>() * 0.2,
            })
            .collect::<Vec<_>>();
        &smoothed
    } else if points.len() >= 3 {
        points
    } else {
        return None;
    };
    let trend = ols_trend(series)?;
    let residuals: Vec<f64> = series
        .iter()
        .map(|sample| {
            let t = (sample.ts_ms - trend.first_ts) as f64 / 60_000.0;
            sample.val - (trend.y_mean + trend.slope * (t - trend.t_mean))
        })
        .collect();
    scaled_mad(&residuals)
}

/// Computes `z_hr` and `r_var_120s` relative to `Quiet-Wake Baseline` and per-Wearable floor.
pub fn normalize_features(
    raw: &RawEpochMetrics,
    baseline_mu_hr: f64,
    baseline_sigma_hr: f64,
    baseline_mu_ln_rmssd: Option<f64>,
    wearable_floor: WearableFloor,
) -> crate::sleep_engine::types::EpochFeatures {
    let denom = baseline_sigma_hr.max(SIGMA_MIN_BPM);
    let z_hr = (raw.hr_bpm_60s - baseline_mu_hr) / denom;
    let r_var_120s = (raw.local_mad_120s / denom).max(wearable_floor.clamped());
    let delta_ln_rmssd_60s = match (raw.ln_rmssd_60s, baseline_mu_ln_rmssd) {
        (Some(cur), Some(base)) => Some(cur - base),
        _ => None,
    };
    crate::sleep_engine::types::EpochFeatures {
        hr_bpm_60s: raw.hr_bpm_60s,
        z_hr,
        hr_slope_4m: raw.hr_slope_4m,
        r_var_120s,
        ln_rmssd_60s: raw.ln_rmssd_60s,
        delta_ln_rmssd_60s,
        rmssd_sdnn_120s: raw.rmssd_sdnn_120s,
    }
}

fn median_in_place(slice: &mut [f64]) -> Option<f64> {
    let n = slice.len();
    if n == 0 {
        return None;
    }
    let mid = n / 2;
    let (left, hi, _) = slice.select_nth_unstable_by(mid, f64::total_cmp);
    if n % 2 == 1 {
        Some(*hi)
    } else {
        let lo = left.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        Some(0.5 * (lo + *hi))
    }
}

/// Computes empirical quantile `q in [0.0, 1.0]` on a finite slice.
pub fn quantile(values: &[f64], q: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    let q_clamped = q.clamp(0.0, 1.0);
    let pos = q_clamped * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    Some(sorted[lo] * (1.0 - frac) + sorted[hi] * frac)
}

/// Computes normalized median absolute deviation (`1.4826 * MAD`).
pub fn scaled_mad(values: &[f64]) -> Option<f64> {
    let med = quantile(values, 0.5)?;
    let devs: Vec<f64> = values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .map(|v| (v - med).abs())
        .collect();
    quantile(&devs, 0.5).map(|mad| 1.4826 * mad)
}

fn compute_rmssd(beats: &[TimedBeat]) -> Option<f64> {
    if beats.len() < 4 {
        return None;
    }
    let mut sum_sq = 0.0;
    let mut count = 0_usize;
    for pair in beats.windows(2) {
        if pair[1].ts_ms - pair[0].ts_ms <= 3_000 {
            let diff = pair[1].rr_ms - pair[0].rr_ms;
            sum_sq += diff * diff;
            count += 1;
        }
    }
    (count >= 3).then(|| (sum_sq / count as f64).sqrt())
}

fn compute_sdnn(beats: &[TimedBeat]) -> Option<f64> {
    if beats.len() < 4 {
        return None;
    }
    let n = beats.len() as f64;
    let mean = beats.iter().map(|b| b.rr_ms).sum::<f64>() / n;
    let var = beats
        .iter()
        .map(|b| {
            let d = b.rr_ms - mean;
            d * d
        })
        .sum::<f64>()
        / (n - 1.0);
    Some(var.sqrt())
}

fn push_bounded(buf: &mut VecDeque<f64>, val: f64, max_len: usize) {
    if buf.len() == max_len {
        buf.pop_front();
    }
    buf.push_back(val);
}

fn push_timed_evicting(buf: &mut VecDeque<TimedValue>, ts_ms: i64, val: f64, window_ms: i64) {
    buf.push_back(TimedValue { ts_ms, val });
    evict_older_than(buf, ts_ms, window_ms);
}

fn evict_older_than<T: Timestamped>(buf: &mut VecDeque<T>, now_ms: i64, window_ms: i64) {
    while let Some(front) = buf.front() {
        if now_ms - front.ts_ms() > window_ms {
            buf.pop_front();
        } else {
            break;
        }
    }
}
