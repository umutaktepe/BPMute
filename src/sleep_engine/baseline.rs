//! Adaptive `Quiet-Wake Baseline` tracker with spike exclusion and freeze hysteresis.

use std::collections::VecDeque;

use crate::sleep_engine::ppg::{RawEpochMetrics, SIGMA_MIN_BPM, quantile};

const MAX_BASELINE_EPOCHS: usize = 72; // 18 min at 15 s cadence
const MAX_BASELINE_AGE_MS: i64 = 900_000; // 15 min trailing calm-wake window
const WARMUP_RECOVERY_AGE_MS: i64 = 120_000; // 2 min trailing window during steep post-workout recovery
const WARMUP_RECOVERY_SLOPE_THRESHOLD: f64 = -0.75; // bpm/min
const MIN_RETAINED_EPOCHS: usize = 4;
const SPIKE_MULTIPLIER: f64 = 1.15; // Exclude > +15% ActiveWake spikes
const MAX_REANCHOR_NEG_SLOPE: f64 = -0.15; // bpm/min: block N1 deceleration descent
const MAX_REANCHOR_POS_SLOPE: f64 = 0.75; // bpm/min: exclude rising edges of ActiveWake spikes

#[derive(Debug, Clone, Copy)]
struct BaselineEpoch {
    ts_ms: i64,
    hr_bpm_60s: f64,
    local_mad_120s: f64,
    ln_rmssd_60s: Option<f64>,
}

/// Adaptive trailing `Quiet-Wake Baseline` over calm wake `Epochs`.
#[derive(Debug, Clone)]
pub struct QuietWakeBaseline {
    epochs: VecDeque<BaselineEpoch>,
    mu_hr: Option<f64>,
    sigma_hr: f64,
    mu_ln_rmssd: Option<f64>,
    frozen: bool,
}

impl Default for QuietWakeBaseline {
    fn default() -> Self {
        Self::new()
    }
}

impl QuietWakeBaseline {
    /// Creates an empty, unfrozen `Quiet-Wake Baseline`.
    pub fn new() -> Self {
        Self {
            epochs: VecDeque::with_capacity(MAX_BASELINE_EPOCHS),
            mu_hr: None,
            sigma_hr: SIGMA_MIN_BPM,
            mu_ln_rmssd: None,
            frozen: false,
        }
    }

    /// Returns whether baseline adaptation is currently frozen.
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Sets the freeze state directly (e.g., entering `Candidate`/`Confirmed`/`Sleeping`).
    pub fn set_frozen(&mut self, frozen: bool) {
        self.frozen = frozen;
    }

    /// Returns the current baseline HR mean, falling back to `fallback_hr` before first `Epoch`.
    pub fn mu_hr_or(&self, fallback_hr: f64) -> f64 {
        self.mu_hr.unwrap_or(fallback_hr)
    }

    /// Returns the current baseline HR scale, floored at `SIGMA_MIN_BPM` (`2.0 bpm`).
    pub fn sigma_hr(&self) -> f64 {
        self.sigma_hr.max(SIGMA_MIN_BPM)
    }

    /// Returns the baseline median `ln(RMSSD)` if calm-wake HRV `Epochs` have been observed.
    pub fn mu_ln_rmssd(&self) -> Option<f64> {
        self.mu_ln_rmssd
    }

    /// Returns `Some(mu_hr)` if at least one calm-wake `Epoch` has been accepted.
    pub fn mu_hr(&self) -> Option<f64> {
        self.mu_hr
    }

    /// Unfreezes on desktop-activity wake hints so calm waking `Epochs` can re-anchor baseline.
    pub fn on_desktop_wake_hint(&mut self) {
        self.frozen = false;
    }

    /// Unfreezes and trims stale pre-cancel `Epochs` so baseline re-anchors during cooldown.
    pub fn on_cancel_cooldown_start(
        &mut self,
        ts_ms: i64,
        recent_hr: Option<f64>,
        recent_mad: Option<f64>,
        recent_ln_rmssd: Option<f64>,
    ) {
        self.frozen = false;
        self.epochs.clear();
        if let Some(hr) = recent_hr {
            self.push_epoch(
                ts_ms,
                BaselineEpoch {
                    ts_ms,
                    hr_bpm_60s: hr,
                    local_mad_120s: recent_mad.unwrap_or(SIGMA_MIN_BPM),
                    ln_rmssd_60s: recent_ln_rmssd,
                },
            );
            self.recompute();
        } else {
            self.mu_hr = None;
            self.sigma_hr = SIGMA_MIN_BPM;
            self.mu_ln_rmssd = None;
        }
    }

    /// Seeds or updates the baseline during `Warmup` from a non-frozen `Epoch`.
    pub fn accept_warmup(&mut self, raw: &RawEpochMetrics) {
        if raw.accumulators_frozen {
            return;
        }
        self.frozen = false;
        if raw.hr_slope_4m < WARMUP_RECOVERY_SLOPE_THRESHOLD {
            self.evict_older_than(raw.ts_ms, WARMUP_RECOVERY_AGE_MS);
        }
        self.try_record_epoch(raw);
    }

    /// Applies post-warmup adaptive re-anchoring and freeze hysteresis rules (ADR-0004).
    pub fn update_post_warmup(
        &mut self,
        raw: &RawEpochMetrics,
        p_sleep: f64,
        in_candidate_or_later: bool,
    ) {
        if in_candidate_or_later || p_sleep >= 0.30 {
            self.frozen = true;
            return;
        }

        self.frozen = false;
        if raw.accumulators_frozen {
            return;
        }

        let local_var_ratio = raw.local_mad_120s / self.sigma_hr();
        // Exclude rising edges and step-edge MAD bursts of transient ActiveWake spikes.
        if raw.hr_slope_4m > MAX_REANCHOR_POS_SLOPE || local_var_ratio > 1.80 {
            return;
        }

        if raw.hr_slope_4m < MAX_REANCHOR_NEG_SLOPE {
            let below_current_baseline = self.mu_hr.is_none_or(|mu| raw.hr_bpm_60s <= mu);
            if !below_current_baseline || local_var_ratio < 0.80 {
                return;
            }
            // Waking post-activity recovery: purge only stale elevated epochs above current calm HR,
            // preserving any older low-HR quiet-wake baseline history.
            let upper_bound = raw.hr_bpm_60s + 1.5 * self.sigma_hr();
            self.epochs.retain(|ep| ep.hr_bpm_60s <= upper_bound);
        }

        self.try_record_epoch(raw);
    }

    fn try_record_epoch(&mut self, raw: &RawEpochMetrics) {
        if self.epochs.len() >= MIN_RETAINED_EPOCHS
            && let Some(mu) = self.mu_hr
            && raw.hr_bpm_60s > SPIKE_MULTIPLIER * mu
        {
            return;
        }
        self.push_epoch(
            raw.ts_ms,
            BaselineEpoch {
                ts_ms: raw.ts_ms,
                hr_bpm_60s: raw.hr_bpm_60s,
                local_mad_120s: raw.local_mad_120s,
                ln_rmssd_60s: raw.ln_rmssd_60s,
            },
        );
        self.recompute();
    }

    fn evict_older_than(&mut self, now_ms: i64, max_age_ms: i64) {
        while self.epochs.len() > MIN_RETAINED_EPOCHS {
            let expired = self
                .epochs
                .front()
                .is_some_and(|ep| now_ms - ep.ts_ms > max_age_ms);
            if expired {
                self.epochs.pop_front();
            } else {
                break;
            }
        }
    }

    fn push_epoch(&mut self, now_ms: i64, epoch: BaselineEpoch) {
        self.evict_older_than(now_ms, MAX_BASELINE_AGE_MS);
        if self.epochs.len() == MAX_BASELINE_EPOCHS {
            self.epochs.pop_front();
        }
        self.epochs.push_back(epoch);
    }

    fn recompute(&mut self) {
        if self.epochs.is_empty() {
            return;
        }
        let hrs: Vec<f64> = self.epochs.iter().map(|ep| ep.hr_bpm_60s).collect();
        let prelim_med = quantile(&hrs, 0.5).unwrap_or(65.0);

        // Exclude > +15% spikes above preliminary quiet median when >= 4 epochs exist.
        let filtered: Vec<&BaselineEpoch> = if self.epochs.len() >= MIN_RETAINED_EPOCHS {
            let kept: Vec<&BaselineEpoch> = self
                .epochs
                .iter()
                .filter(|ep| ep.hr_bpm_60s <= SPIKE_MULTIPLIER * prelim_med)
                .collect();
            if kept.is_empty() {
                self.epochs.iter().collect()
            } else {
                kept
            }
        } else {
            self.epochs.iter().collect()
        };

        let kept_hrs: Vec<f64> = filtered.iter().map(|ep| ep.hr_bpm_60s).collect();
        let kept_hrvs: Vec<f64> = filtered.iter().filter_map(|ep| ep.ln_rmssd_60s).collect();

        self.mu_hr = quantile(&kept_hrs, 0.5);
        // Median of detrended local 120 s MAD values prevents transient step-edge MAD spikes
        // from inflating baseline sigma_hr.
        let kept_mads: Vec<f64> = filtered.iter().map(|ep| ep.local_mad_120s).collect();
        let median_mad = quantile(&kept_mads, 0.5).unwrap_or(SIGMA_MIN_BPM);
        self.sigma_hr = median_mad.max(SIGMA_MIN_BPM);
        if !kept_hrvs.is_empty() {
            self.mu_ln_rmssd = quantile(&kept_hrvs, 0.5);
        }
    }
}
