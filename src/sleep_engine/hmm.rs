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
        let rvar_params = [(1.75, 0.55), (0.95, 0.22), (0.40, 0.14)];
        let d_rmssd_params = [(-0.25, 0.55), (0.0, 0.55), (0.20, 0.55)];
        let ratio_params = [(0.45, 0.50), (0.62, 0.50), (1.05, 0.52)];

        let mut log_post = [0.0_f64; 3];
        for k in 0..3 {
            let mut ll = pred[k].ln();
            ll += student_t_log_pdf_nu3(features.z_hr, z_params[k].0, z_params[k].1);
            ll += student_t_log_pdf_nu5(features.hr_slope_4m, slope_params[k].0, slope_params[k].1);
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
    let s = sigma.max(1e-6);
    let r = (x - mu) / s;
    LOG_NORM_NU5 - s.ln() - 3.0 * (1.0 + (r * r) / 5.0).ln()
}
