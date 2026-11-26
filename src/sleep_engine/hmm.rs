//! 3-state online Forward-Filtered Hidden Markov Model with Student-$t$ emissions
//! and native HRV marginalization.

use crate::sleep_engine::types::EpochFeatures;

/// Latent physiological state indices in the 3-state HMM.
pub const STATE_ACTIVE_WAKE: usize = 0;
pub const STATE_QUIET_WAKE: usize = 1;
pub const STATE_SLEEP_NREM: usize = 2;

const INITIAL_PROBS: [f64; 3] = [0.15, 0.84, 0.01];

/// Sticky per-`15 s` Epoch transition probabilities `[from][to]`.
const TRANSITION: [[f64; 3]; 3] = [
    [0.92, 0.079, 0.001],
    [0.03, 0.95, 0.02],
    [0.015, 0.025, 0.96],
];

/// Precomputed exact Student-$t$ log normalizing constants `ln(Gamma((nu+1)/2)) - ln(Gamma(nu/2)) - 0.5*ln(nu*PI)`.
/// Exact for `nu = 3`: `ln(2 / (PI * sqrt(3)))`.
const LOG_NORM_NU3: f64 = -1.001_009_092_458_936;
/// Exact for `nu = 5`: `ln(8 / (3 * PI * sqrt(5)))`.
const LOG_NORM_NU5: f64 = -0.963_482_274_142_251;
/// Tightened negative-slope scale for `ActiveWake` so steep recovery drops cannot beat `QuietWake` or `SleepNREM`.
const ACTIVE_WAKE_NEG_SLOPE_SIGMA: f64 = 0.25;

/// 3-state Forward-Filtered HMM (`ActiveWake`, `QuietWake`, `SleepNREM`).
#[derive(Debug, Clone)]
pub struct HmmFilter {
    probs: [f64; 3],
}

impl Default for HmmFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl HmmFilter {
    /// Initializes the forward filter at the waking prior `[0.15, 0.84, 0.01]`.
    pub fn new() -> Self {
        Self {
            probs: INITIAL_PROBS,
        }
    }

    /// Returns current filtered state probabilities `[P(ActiveWake), P(QuietWake), P(SleepNREM)]`.
    pub fn probs(&self) -> [f64; 3] {
        self.probs
    }

    /// Resets the posterior distribution back to the initial waking prior.
    pub fn reset(&mut self) {
        self.probs = INITIAL_PROBS;
    }

    /// Executes one online forward-filtering step over an `EpochFeatures` vector.
    ///
    /// When `hrv_valid` is `false` or HRV features are `None`, HRV dimensions are omitted from
    /// the emission log-likelihood sum (exact marginalization).
    pub fn step(
        &mut self,
        features: &EpochFeatures,
        hrv_valid: bool,
        expected_z_sleep: f64,
    ) -> [f64; 3] {
        // 1. Chapman-Kolmogorov prediction step: p_pred[j] = sum_i p_prev[i] * A[i][j]
        let mut pred = [0.0_f64; 3];
        for (j, pred_j) in pred.iter_mut().enumerate() {
            let mut sum = 0.0;
            for (i, trans_row) in TRANSITION.iter().enumerate() {
                sum += self.probs[i] * trans_row[j];
            }
            *pred_j = sum.max(1e-15);
        }

        // 2. Emission log-likelihoods in log space
        let z_params = [(2.2, 1.6), (0.0, 1.0), (expected_z_sleep, 1.0)];
        let slope_params = [(0.0, 1.60), (0.0, 0.55), (-0.20, 0.28)];
        let rvar_params = [(1.75, 0.55), (0.85, 0.30), (0.40, 0.14)];
        let d_rmssd_params = [(-0.25, 0.55), (0.0, 0.55), (0.20, 0.55)];
        let ratio_params = [(0.45, 0.50), (0.62, 0.50), (1.05, 0.52)];

        let mut log_post = [0.0_f64; 3];
        for k in 0..3 {
            let mut ll = pred[k].ln();
            ll += student_t_log_pdf_nu3(features.z_hr, z_params[k].0, z_params[k].1);
            // ActiveWake models upward surges/oscillations (sigma_pos = 1.60), not steep
            // downward recovery/sleep re-entry; tighten its negative slope tail so rapid
            // deceleration cannot beat QuietWake or SleepNREM.
            ll += if k == STATE_ACTIVE_WAKE {
                student_t_log_pdf_nu5_asymmetric(
                    features.hr_slope_4m,
                    slope_params[k].0,
                    ACTIVE_WAKE_NEG_SLOPE_SIGMA,
                    slope_params[k].1,
                )
            } else {
                student_t_log_pdf_nu5(features.hr_slope_4m, slope_params[k].0, slope_params[k].1)
            };
            ll += student_t_log_pdf_nu5(features.r_var_120s, rvar_params[k].0, rvar_params[k].1);

            if hrv_valid
                && let (Some(d_ln), Some(ratio)) =
                    (features.delta_ln_rmssd_60s, features.rmssd_sdnn_120s)
            {
                ll += student_t_log_pdf_nu5(d_ln, d_rmssd_params[k].0, d_rmssd_params[k].1);
                ll += student_t_log_pdf_nu5(ratio, ratio_params[k].0, ratio_params[k].1);
            }
            log_post[k] = ll;
        }

        // 3. Log-sum-exp normalization
        let max_ll = log_post.into_iter().fold(f64::NEG_INFINITY, f64::max);
        let mut sum_exp = 0.0;
        let mut next = [0.0_f64; 3];
        for k in 0..3 {
            let w = (log_post[k] - max_ll).exp();
            next[k] = w;
            sum_exp += w;
        }
        if sum_exp > 0.0 && sum_exp.is_finite() {
            for p in &mut next {
                *p /= sum_exp;
            }
            self.probs = next;
        }
        self.probs
    }
}

#[inline]
fn student_t_log_pdf_nu3(x: f64, mu: f64, sigma: f64) -> f64 {
    let s = sigma.max(1e-6);
    let r = (x - mu) / s;
    LOG_NORM_NU3 - s.ln() - 2.0 * (1.0 + (r * r) / 3.0).ln()
}

#[inline]
fn student_t_log_pdf_nu5(x: f64, mu: f64, sigma: f64) -> f64 {
    student_t_log_pdf_nu5_asymmetric(x, mu, sigma, sigma)
}

#[inline]
fn student_t_log_pdf_nu5_asymmetric(x: f64, mu: f64, sigma_neg: f64, sigma_pos: f64) -> f64 {
    let peak_s = sigma_pos.max(1e-6);
    let tail_s = if x < mu { sigma_neg } else { sigma_pos }.max(1e-6);
    let r = (x - mu) / tail_s;
    LOG_NORM_NU5 - peak_s.ln() - 3.0 * (1.0 + (r * r) / 5.0).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmm_step_matches_analytical_student_t_worked_example() {
        let features = EpochFeatures {
            hr_bpm_60s: 55.0,
            z_hr: -2.5,
            hr_slope_4m: -0.25,
            r_var_120s: 0.35,
            ln_rmssd_60s: Some(3.8),
            delta_ln_rmssd_60s: Some(0.22),
            rmssd_sdnn_120s: Some(1.10),
        };

        let mut filter_bpm_only = HmmFilter::new();
        let p_bpm_only = filter_bpm_only.step(&features, false, -2.4);
        let expected_bpm_only = [0.000_503_090_205, 0.165_558_158_444, 0.833_938_751_352];
        for (got, exp) in p_bpm_only.iter().zip(expected_bpm_only) {
            assert!((got - exp).abs() < 1e-11, "got {got}, expected {exp}");
        }

        let mut filter_bpm_rr = HmmFilter::new();
        let p_bpm_rr = filter_bpm_rr.step(&features, true, -2.4);
        let expected_bpm_rr = [0.000_157_195_882, 0.102_159_362_773, 0.897_683_441_345];
        for (got, exp) in p_bpm_rr.iter().zip(expected_bpm_rr) {
            assert!((got - exp).abs() < 1e-11, "got {got}, expected {exp}");
        }
    }

    #[test]
    fn steep_negative_slope_does_not_classify_as_active_wake() {
        // Rapid sleep re-entry after a brief nocturnal rise (e.g. 75 -> 60 bpm over 4 min)
        // produces a steep negative 4-min slope alongside transient step-edge variability.
        let mut filter = HmmFilter::new();
        let peak_wake = EpochFeatures {
            hr_bpm_60s: 75.0,
            z_hr: 2.2,
            hr_slope_4m: 1.5,
            r_var_120s: 1.50,
            ln_rmssd_60s: None,
            delta_ln_rmssd_60s: None,
            rmssd_sdnn_120s: None,
        };
        for _ in 0..8 {
            filter.step(&peak_wake, false, -2.4);
        }
        assert!(
            filter.probs()[STATE_ACTIVE_WAKE] > 0.70,
            "precondition: peak arousal enters ActiveWake, got {:?}",
            filter.probs()
        );

        let sleep_reentry = EpochFeatures {
            hr_bpm_60s: 64.0,
            z_hr: -0.5,
            hr_slope_4m: -3.2,
            r_var_120s: 1.35,
            ln_rmssd_60s: None,
            delta_ln_rmssd_60s: None,
            rmssd_sdnn_120s: None,
        };
        for _ in 0..4 {
            filter.step(&sleep_reentry, false, -2.4);
        }
        let p = filter.probs();
        assert!(
            p[STATE_ACTIVE_WAKE] < 0.25,
            "steep negative deceleration must drop out of ActiveWake, got probs={p:?}"
        );

        // Positive arousal slope still favors ActiveWake over QuietWake/SleepNREM.
        let mut rising_filter = HmmFilter::new();
        let active_rise = EpochFeatures {
            hr_bpm_60s: 74.0,
            z_hr: 2.0,
            hr_slope_4m: 2.5,
            r_var_120s: 1.45,
            ln_rmssd_60s: None,
            delta_ln_rmssd_60s: None,
            rmssd_sdnn_120s: None,
        };
        for _ in 0..6 {
            rising_filter.step(&active_rise, false, -2.4);
        }
        assert!(
            rising_filter.probs()[STATE_ACTIVE_WAKE] > 0.70,
            "steep positive slope must remain ActiveWake, got {:?}",
            rising_filter.probs()
        );
    }
}
