# BPMute Sleep-Onset Engine, Calibration Ledger, and Silent `pause_media` Audit

> **Scope:** Primary-source mathematical, behavioral, and test-coverage audit for `umutaktepe/BPMute`, verifying `src/sleep_engine/`, `src/calibration/`, `src/storage/`, `src/daemon/state/`, `src/daemon/monitoring.rs`, `src/cli/commands/sessions.rs`, and `src/tui/` against `GLOSSARY.md` and ADRs `0004`, `0005`, `0007`, `0008`, and `0011`.
>
> **Working-Tree Note (2026-11-22):** At audit time, `src/sleep_engine/types.rs` (lines `77–80`, adding `StreamStatus::is_connected`) and `src/daemon/state/mod.rs` (lines `322–372`, clearing `wearable_batteries` on disconnect) carry uncommitted working-tree edits unrelated to HMM, baseline, or calibration math.

---

## Executive Summary & Risk Register

The core physiological signal processing, forward-filtered HMM math, and SQLite calibration-ledger recomputation formulas are **mathematically sound and faithful to the ADRs**—no formula errors stand in the way of running silent `pause_media` calibration nights (`dry_run = false`) and using CLI next-day feedback (`bpmute sessions feedback ... --avg-sleep-hr ... --onset HH:MM`).

However, the audit uncovered **three concrete bugs/blockers** (affecting `--dry-run` safety and TUI feedback/seed entry) plus **four minor discrepancies / edge-case risks** and **test coverage gaps**:

| # | Area | Finding | Classification | Primary Citation |
|---|---|---|---|---|
| 1 | Silent `pause_media` + `dry_run` | `handle_confirmed_sleep_onset` silent `pause_media` (`0` players) fast-path does **not** check `self.is_dry_run()`, delivering `GraceResult::Expired` and recording live calibration even under `dry_run = true` (violates ADR-0007). | **Bug / Blocker** | `src/daemon/state/grace.rs:297-318` |
| 2 | TUI vs. CLI/Storage Validation & Seed | `SoftRebaselineModal` and `FeedbackModal` clamp sleeping HR to `[30, 120]` BPM vs. `[30, 140]` in `validate_sleep_hr_bpm`/CLI/storage; `TuiCommand::CalibrationSetSeed` is dead/unwired in the UI (`[B]` inserts a `CalibrationBoundary` instead). | **Bug / Blocker** | `src/tui/input/modal_actions.rs:617,779`; `src/calibration/types.rs:331-338`; `src/tui/client_commands.rs:344-359` |
| 3 | TUI `FeedbackModal` `--onset` | Accepts single-digit hours (e.g., `9:30`), which `ClockTime::from_str` rejects via `.ok()`, silently dropping `onset_hhmm` to `PatchField::Unchanged`; also uses `.single()` (fails on fall-back DST ambiguous hours). | **Bug / Blocker** | `src/tui/input/modal_actions.rs:721-758`; `src/tui/client_commands.rs:252-255`; `src/config/schema.rs:316-340` |
| 4 | HMM Student-$t$ Constants | `LOG_NORM_NU3` (`-1.001009...` vs. `-1.000888...`) and `LOG_NORM_NU5` (`-0.963482...` vs. `-0.968619...`) differ slightly from exact analytical values; cancels completely in log-sum-exp normalization across all 3 states. | **Minor Discrepancy** | `src/sleep_engine/hmm.rs:22-24,101-115` |
| 5 | `CalibrationPriors` Unused Fields | `mu_ln_rmssd_sleep` and `sigma_wake` are computed/persisted in `CalibrationState` and passed into `CalibrationPriors`, but never consumed by `HmmFilter` or `SleepEngine`. | **Minor Discrepancy** | `src/sleep_engine/types.rs:192-204`; `src/sleep_engine/hmm.rs:77-81`; `src/sleep_engine/state_machine/lifecycle.rs:244-277` |
| 6 | Silent `pause_media` Post-Trigger Watch & Crash Recovery | Silent `0`-player fast-path skips the 5-min `watch_post_trigger_activity` watch and `mark_session_sleep_trigger`; `MonitoringContext` never emits `EngineEvent::DesktopActivity` (making `DesktopWakeRebound` unreachable in daemon runtime), and `run_monitoring_loop` `select!` is non-biased between `wearable_rx` and `ctrl_rx`. | **Minor Discrepancy / Edge-Case Risk** | `src/daemon/state/grace.rs:297-318,438-495`; `src/daemon/monitoring.rs:107-199,418-434`; `src/storage/store/session_lifecycle.rs:53-70` |
| 7 | `MissedSleep` + `--onset` Double Nudge | Passing `--verdict missed_sleep --onset HH:MM` on an unconfirmed session applies both `ETA_MISSED_TAU` (`-0.010`) and `apply_manual_onset_threshold_nudge` (`-0.005`), totalling `-0.015`. | **Minor Discrepancy / Edge-Case Risk** | `src/calibration/recompute.rs:381-399,761-774` |
| 8 | End-to-End Silent `pause_media` Calibration Test | No integration test drives a full `WearableSource` -> `Confirmed` -> silent `0`-player `pause_media` -> `Sleeping` (`>= 15 min`) -> `SessionClosed` -> next-day `recompute` flow; `test_dry_run_records_zero_calibration_feedback` does not run a live session. | **Untested Gap** | `tests/daemon_actions_e2e.rs:181-220,450-501` |

---

## 1. Sleep Engine HMM & Baseline Math (vs. ADR-0004 & ADR-0007)

### 1.1 `Warmup` Durations & `warmup_mu_hr` Snapshot Timing
- **Tiered durations (`src/sleep_engine/state_machine/mod.rs:15-16`, `src/sleep_engine/state_machine/lifecycle.rs:237-242`):**
  - `ColdStart` and `Learning`: `WARMUP_EPOCHS_COLD_OR_LEARNING = 32` clean (`!accumulators_frozen`) `15 s` Epochs (`8 min`).
  - `Calibrated`: `WARMUP_EPOCHS_CALIBRATED = 20` clean `15 s` Epochs (`5 min`).
  - Frozen epochs (`flatline_stale || coverage < 0.80`) do not increment `warmup_clean_epochs` (`src/sleep_engine/state_machine/mod.rs:329-337`).
- **Snapshot timing (`src/sleep_engine/state_machine/mod.rs:234-246,332-347`, `lifecycle.rs:279-283`):**
  - For raw packets (`process_raw_epoch`), `self.baseline.accept_warmup(raw)` executes *before* `normalize_features` and `process_features_step`.
  - When `warmup_clean_epochs >= required_warmup_epochs()` (on the 32nd or 20th clean epoch), `snapshot_warmup_baseline()` captures `warmup_mu_hr = baseline.mu_hr()`, `warmup_sigma_hr = Some(baseline.sigma_hr())`, and `warmup_ln_rmssd = baseline.mu_ln_rmssd()`.
  - `evaluate_warmup_anomaly()` (`lifecycle.rs:244-254`) immediately checks `|(mu - ref_hr)| > 2.5 * mad.max(SIGMA_MIN_BPM)` using `p.wake_median.or(p.mu_wake)` and `p.wake_mad_s` (`wake_mad_s` is `Some` once `>= 5` clean sessions exist in calibration).
  - The engine transitions to `DetectorState::Monitoring` on that same 32nd/20th epoch (`state_machine/mod.rs:340-346`), while `HmmFilter::step` is bypassed (`state_machine/mod.rs:281-286`), so the transition epoch carries `state: Monitoring` and `p_sleep = 0.01`.
  - **Edge-case note:** In `close_session` (`lifecycle.rs:405-409`), `warmup_mu_hr` falls back to `self.baseline.mu_hr()` if a session closes before completing warmup. In `feed_epoch_features` (`state_machine/mod.rs:333-335,553-589`), `accept_warmup` runs inside `process_features_step` without re-normalizing pre-extracted `EpochFeatures`.

### 1.2 `QuietWakeBaseline` Sliding Window, Spike/N1 Exclusion, & `P(SleepNREM) >= 0.30` Freeze
- **Window bounds (`src/sleep_engine/baseline.rs:7-14,183-203`):**
  - `MAX_BASELINE_AGE_MS = 900_000` (`15 min`), `MAX_BASELINE_EPOCHS = 72` (`18 min` at `15 s`), `MIN_RETAINED_EPOCHS = 4`.
  - `evict_older_than` retains at least `MIN_RETAINED_EPOCHS = 4` epochs even if older than `15 min` during sparse updates/gaps.
  - During `Warmup`, if `raw.hr_slope_4m < -0.75 bpm/min` (`WARMUP_RECOVERY_SLOPE_THRESHOLD`), `accept_warmup` (`baseline.rs:121-123`) evicts epochs older than `WARMUP_RECOVERY_AGE_MS = 120_000` (`2 min`) down to `MIN_RETAINED_EPOCHS` to shed elevated post-workout HR.
- **Spike exclusion (`baseline.rs:164-170,205-240`):**
  - Admission guard (`try_record_epoch`): once `epochs.len() >= 4`, rejects any epoch with `hr_bpm_60s > 1.15 * mu_hr` (`SPIKE_MULTIPLIER = 1.15`).
  - Recomputation (`recompute`): computes preliminary median `prelim_med` across retained epochs, filters `ep.hr_bpm_60s <= 1.15 * prelim_med` (when `len >= 4`, falling back to all if empty), then sets:
    - $\mu_{\text{HR}} = \text{median}(\text{kept\_hrs})$
    - $\sigma_{\text{HR}} = \max(\text{median}(\text{kept\_mads}), 2.0)$
    - $\mu_{\ln\text{RMSSD}} = \text{median}(\text{kept\_hrvs})$ when non-empty.
  - **Minor edge case (`baseline.rs:237-239`):** If all HRV-bearing epochs later age out of `epochs`, `self.mu_ln_rmssd` retains its last median rather than clearing to `None`.
- **Freeze & N1 exclusion (`baseline.rs:128-162`, `state_machine/mod.rs:363,367,390`):**
  - If `in_candidate_or_later || p_sleep >= 0.30`, `update_post_warmup` sets `self.frozen = true` and returns immediately.
  - Excludes rising edges and MAD bursts: returns if `hr_slope_4m > 0.75` (`MAX_REANCHOR_POS_SLOPE`) or `local_mad_120s / self.sigma_hr() > 1.80`.
  - N1 deceleration guard: when `hr_slope_4m < -0.15` (`MAX_REANCHOR_NEG_SLOPE`), blocks re-anchoring *unless* `hr_bpm_60s <= mu_hr` AND `local_var_ratio >= 0.80` (collapsed-variance N1 descent is blocked; high-variance post-activity recovery purges stale epochs `> hr_bpm_60s + 1.5 * sigma_hr`).
  - `Candidate`, `Confirmed`, and `Sleeping` explicitly force `baseline.set_frozen(true)`. On cancel/snooze/arousal, `on_cancel_cooldown_start` (`baseline.rs:88-113`) or `on_desktop_wake_hint` (`baseline.rs:83-85`) unfreezes the baseline.

### 1.3 PPG Pipeline & Feature Formulas (`src/sleep_engine/ppg.rs`)
- **Stage 1 validation (`ppg.rs:226-231`):** `contact_status != NoContact && bpm.is_finite() && (30.0..=200.0).contains(&bpm)`. Non-finite/out-of-range BPMs are excluded from filtering and reduce epoch `coverage`.
- **Stage 2a Lipponen-Tarvainen causal RR cleaner (`ppg.rs:290-371`):**
  - Valid RR range: `[300.0, 2000.0]` ms. Invalid/non-finite RR is replaced with `med11().unwrap_or((60_000.0 / bpm).clamp(300.0, 2000.0))` and flagged `corrected = true`.
  - Thresholds (`lt_thresholds`, `ppg.rs:347-371`): activates at `LT_MED_WINDOW_BEATS = 11` clean beats (`d_rr_qd91`, `m_rr_qd91` capped at `LT_QD_WINDOW_BEATS = 91`), using quartile deviation $\text{QD} = 0.5(Q_{0.75} - Q_{0.25})$:
    $$\theta_1 = \max(5.2 \cdot \text{QD}(d_{\text{RR}}),\ 35.0\text{ ms}), \qquad \theta_2 = \max(5.2 \cdot \text{QD}(m_{\text{RR}}),\ 45.0\text{ ms})$$
    Fallback before 11 clean beats: $\max(0.20 \cdot \text{med}_{11},\ 150.0\text{ ms})$.
  - Missed beat (`rr > 1.75 * med11 && |0.5*rr - med11| < 2*th2`): splits into two beats of `0.5 * rr` (first `corrected = true`, second `corrected = false`). Ectopic/artifact (`rr < 0.65 * med11 || |d_rr| > th1 || |m_rr| > th2`): replaces with `med11`, `corrected = true`.
- **Stage 2b 25 s Hampel BPM filter (`ppg.rs:393-413`):** Window `25_000 ms`, min `5` samples; scale $s = \max(1.4826 \cdot \text{MAD},\ 1.0)$; replaces raw BPM with window median if $|\text{bpm} - \text{med}| > 3.0 \cdot s$.
- **Stage 3 mode-aware flatline (`ppg.rs:233-288`):**
  - `has_active_rr` is true if `!rr_ms.is_empty()` or `last_rr_packet_ts_ms` within `3_000 ms`.
  - `BPM+RR`: `flatline_stale` iff `run_ms >= 30_000` (`|bpm - prev| < 1e-9`), `flatline_rr_count >= 5`, AND `rr_flatline_since_ms` elapsed `>= 30_000` (`|rr - prev| <= 1.0` ms).
  - `BPM-only`: `flatline_stale` iff `run_ms >= 120_000`.
  - **Edge case:** If a `BPM+RR` wearable outputs constant integer BPM for `> 120 s` while RR jitters by `> 1 ms`, `has_active_rr == true` bypasses the `120 s` BPM-only flatline check.
- **Stage 4 Epoch features (`ppg.rs:415-680`):**
  - `coverage` (`ppg.rs:419-424`): `valid_non_flatline_packets / max(round((ts - epoch_start)/1000), 15.0, total_packets)` clamped to `[0.0, 1.0]`. Assumes `>= 0.8 Hz` nominal delivery (`MIN_EPOCH_COVERAGE = 0.80`).
  - `hr_bpm_60s` (`ppg.rs:426-429,489-499`): arithmetic mean of Hampel-filtered BPM over trailing `60_000 ms` (fallback `last_raw_bpm`, then `65.0`).
  - `z_hr` (`ppg.rs:586-587`): `(hr_bpm_60s - baseline_mu_hr) / baseline_sigma_hr.max(2.0)`. Denominator `>= 2.0` prevents division by zero.
  - `hr_slope_4m` (`ppg.rs:501-549`): OLS slope over `filtered_bpm_240s` (`240_000 ms`), requiring `len >= 4` and span `>= 60_000 ms` (else `0.0`); time axis is `(ts_ms - first_ts) / 60_000.0`, so units are **`bpm/min`**, with `den <= 1e-12 -> 0.0`.
  - `local_mad_120s` & `r_var_120s` (`ppg.rs:433-439,551-588`): applies a 5-point moving average (if `len >= 5`), fits OLS over `120_000 ms`, computes `1.4826 * MAD(residuals)` (fallback `2.0`). Normalized as `r_var_120s = (local_mad_120s / denom).max(wearable_floor.clamped())` where `wearable_floor` is clamped to `[0.05, 0.80]` (`src/sleep_engine/types.rs:241-246`). Note: `r_var_120s` is a dimensionless **scale ratio** (not squared variance ratio), consistent with `observed_hr_var_ratio` in `state_machine/mod.rs:303-305`.
  - SQI & HRV (`ppg.rs:441-472,649-680`): `rr_artifact_frac = sqi_corrected / sqi_total` over `60_000 ms`. `hrv_valid = !flatline_stale && !accumulators_frozen && sqi_total >= 25 && rr_artifact_frac <= 0.05`.
    - `ln_rmssd_60s = rmssd_60.max(1.0).ln()` (natural log $\ln$, floored at `1.0 ms` $\Rightarrow \ge 0.0$).
    - `delta_ln_rmssd_60s = ln_rmssd_60s - baseline_mu_ln_rmssd`.
    - `rmssd_sdnn_120s = (rmssd_120 / sdnn_120.max(1.0)).clamp(0.0, 2.5)`, pairing consecutive beats in `compute_rmssd` only when `dt <= 3_000 ms` (intra-packet beats have `dt = 0`, so multi-beat BLE packets are handled properly) and using unbiased sample SDNN (`n - 1.0`, `n >= 4`).

### 1.4 HMM Emissions, Transitions, Hysteresis, & `CalibrationPriors` Blending
- **States & Markov chain (`src/sleep_engine/hmm.rs:7-18`):**
  - `INITIAL_PROBS = [0.15, 0.84, 0.01]` for `[ActiveWake, QuietWake, SleepNREM]`.
  - Sticky `15 s` row-stochastic transition matrix `TRANSITION` (each row sums to `1.000`):
    - `ActiveWake -> [0.92, 0.079, 0.001]`
    - `QuietWake  -> [0.03, 0.95, 0.02]`
    - `SleepNREM  -> [0.015, 0.025, 0.96]`
  - Prediction floored at `1e-15` (`hmm.rs:73`).
- **Student-$t$ emissions & normalization (`hmm.rs:20-25,77-131`):**
  - Core features: `z_hr` ($\nu = 3$: `[(2.2, 1.6), (0.0, 1.0), (expected_z_sleep, 1.0)]`), `hr_slope_4m` ($\nu = 5$: `[(0.0, 1.60), (0.0, 0.55), (-0.20, 0.28)]`), `r_var_120s` ($\nu = 5$: `[(1.75, 0.55), (0.95, 0.22), (0.40, 0.14)]`).
  - Optional HRV features (included iff `hrv_valid` and both `Some`): `delta_ln_rmssd_60s` ($\nu = 5$: `[(-0.25, 0.55), (0.0, 0.55), (0.20, 0.55)]`), `rmssd_sdnn_120s` ($\nu = 5$: `[(0.45, 0.50), (0.62, 0.50), (1.05, 0.52)]`). When absent/gated, HRV terms are omitted in log space (exact marginalization).
  - **Constant precision note:** `LOG_NORM_NU3 = -1.001_009_092_458_936` (exact $\ln(2 / (\pi \sqrt{3})) \approx -1.000_888_85$) and `LOG_NORM_NU5 = -0.963_482_274_142_251` (exact $\ln(8 / (3\pi\sqrt{5})) \approx -0.968_619_59$). Because all 3 states share the same $\nu$ per feature, log-sum-exp normalization cancels the additive constants identically.
  - Log-sum-exp (`hmm.rs:101-115`) guards `sum_exp > 0.0 && sum_exp.is_finite()`, retaining prior `self.probs` if non-finite.
- **Thresholds, confirmation dwell, & hysteresis (`src/sleep_engine/types.rs:270-313`, `state_machine/mod.rs:354-388,444-521`):**
  - Presets (`Balanced` / `Conservative` / `Responsive`): base `p_sleep_threshold = 0.85 / 0.90 / 0.78` (`+ priors.tau_p_sleep_offset`, clamped `[0.50, 0.99]`), `dwell_bpm_rr_sec = 180 / 300 / 120` (`+ dwell_adjust_sec`, `[60, 900]`), `dwell_bpm_only_sec = 300 / 420 / 240` (`+ dwell_adjust_sec`, `[60, 900]`), `reset_threshold = 0.50`.
  - Candidate entry requires `cooldown_clear` (`>= 15 min` after FP/snooze) and `stabilization_remaining_epochs == 0` (`8` epochs = `2 min` after Tier-2 gap), with `p_sleep >= p_sleep_threshold`.
  - Candidate dwell selects `dwell_bpm_rr_sec` iff `candidate_hrv_valid_epochs / candidate_epochs >= 0.80`, else `dwell_bpm_only_sec`.
  - Candidate reset hysteresis: `p_sleep < 0.50` immediately, OR `candidate_arousal_streak >= 2` where arousal is `p_active_wake > 0.50 || (z_hr > -0.5 && r_var_120s > 1.1)`.
  - Confirmed arousal abort: `p_sleep < 0.50` for `2` consecutive epochs emits `GraceAbortRequested(PhysiologicalArousal)`.
- **Personal priors blending (`src/sleep_engine/state_machine/lifecycle.rs:256-277`):**
  - `DEFAULT_EXPECTED_Z_SLEEP = -2.4`. If `self.anomalous` OR (`ColdStart && mu_sleep.is_none()`), returns `-2.4`.
  - Otherwise computes `from_delta = delta_hr_onset.map(|d| (-d.abs()) / denom)` and `from_sleep_mu = mu_sleep.map(|ms| (ms - baseline_mu_hr) / denom)`, averages if both present, and blends:
    $$\text{expected\_z\_sleep} = \operatorname{clamp}\big((1 - w)\cdot(-2.4) + w \cdot z_{\text{personal}},\ -3.5,\ -1.6\big)$$
    with $w = 0.25$ (`ColdStart` with Day-1 seed), $w = 0.50$ (`Learning`), $w = 0.80$ (`Calibrated`).
  - **Unused fields:** `CalibrationPriors::mu_ln_rmssd_sleep` and `CalibrationPriors::sigma_wake` (`types.rs:197,200`) are populated from `CalibrationState` but are not read by `SleepEngine` or `HmmFilter`.

---

## 2. Calibration Ledger Recomputation & Feedback Math (vs. ADR-0005 & ADR-0008)

### 2.1 Deterministic Fold, Boundaries, Weights, EB $\to$ Capped-EWMA (`src/calibration/recompute.rs`)
- **Chronological determinism (`recompute.rs:634-684`):**
  - `recompute_calibration` sorts sessions strictly by `(start_ts_ms, id)` ascending; `CalibrationFolder` matches bit-for-bit when folded chronologically.
  - Latest active boundary (`reverted_at_ms.is_none()`) is selected by `(effective_ts_ms, created_at_ms, id)`.
  - `Calibration Boundary` isolation (`recompute.rs:254-267,310-337`):
    - Personal physiological state skips sessions with `session.start_ts_ms < active_boundary_ts_ms`.
    - `effective_seed_sleep_hr` takes `active_boundary.seed_sleep_hr` when an active boundary exists (else `day1_seed_sleep_hr`).
    - Per-Wearable accumulators (`hr_var_floor`, `typical_sqi`, `supports_rr`, `supports_contact`, `sessions_count`) process all sessions `>= floor_reset_ts_ms` (latest active boundary with `reset_floors = true`), preserving floors across normal boundaries.
- **Day-1 seed & `NADIR_OFFSET_BPM` (`recompute.rs:14-15,262-267,421-440`):**
  - `NADIR_OFFSET_BPM = +2.5 bpm` is added to both `day1_seed_sleep_hr` (`DAY1_SEED_WEIGHT = 1.5`) and `manual_avg_sleep_hr` (`WEIGHT_MANUAL_AVG_SLEEP_HR = 2.0`), converting overnight average/nadir HR up to onset-HR space.
  - `delta_hr_onset` with `manual_avg_sleep_hr` (when `onset_evidence` is `None`) is `(avg_hr + 2.5) - warmup_mu_hr`, negative sign (`sleep - wake`), clamped to `DELTA_HR_RANGE = (-20.0, -1.5)`.
- **Session eligibility & weights (`recompute.rs:367-458`):**
  - `has_implicit_sleep = !is_fp && post_trigger_sleep_sec >= 900` (`MIN_IMPLICIT_SLEEP_SEC = 900`, weight `WEIGHT_IMPLICIT_SLEEP = 0.5`).
  - `has_suspend_poweroff = !is_fp && !has_implicit_sleep && matches!(trigger_power_action, Suspend | Poweroff) && sleeping_ts_ms.is_some()` (weight `WEIGHT_SUSPEND_POWEROFF = 0.35`; updates `mu_wake`, `sigma_wake`, and `delta_hr_onset` **only**, leaving `mu_sleep` and `mu_ln_rmssd_sleep` untouched).
  - `has_manual_sleep = manual_avg_sleep_hr.is_some() || manual_onset_ts_ms.is_some()`.
  - Precedence for `mu_sleep`: `manual_avg_sleep_hr` (`w = 2.0`) $>$ `onset_evidence` (`w = 1.75`) $>$ `implicit_sleep` (`w = 0.5`).
  - Precedence for `delta_hr_onset`: `onset_evidence` (`w = 1.75`) $>$ `manual_avg_sleep_hr` (`w = 2.0`) $>$ `implicit_sleep` (`w = 0.5`) $>$ `suspend_poweroff` (`w = 0.35`).
- **Empirical Bayes $\to$ Capped-EWMA (`recompute.rs:56-117,464-508`):**
  - Sessions with `valid_sessions < 6` (`CALIBRATED_MIN_SESSIONS - 1 = 6`) use `update_eb`:
    - `mu_wake`, `sigma_wake`, `mu_sleep`, `mu_ln_rmssd_sleep` use `prior_kappa = 0.0` (exact weighted mean $\sum w_i x_i / \sum w_i$, including Day-1 seed at `w = 1.5`).
    - `delta_hr_onset` uses `EB_KAPPA_0 = 4.0` shrunk toward `prior_delta = (-0.08 * mu_wake).clamp(-20.0, -1.5)` (fallback `-5.5`).
  - Starting at the 7th valid session (`valid_sessions >= 6`, moving to `7` / `Calibrated`), `update_ewma` activates:
    - Huber clipping: $\text{clip}(x_t - \hat{x}_{t-1},\ -\Delta,\ +\Delta)$ where $\Delta = 2.0 \cdot S_{\text{wake}}$ for HR params (`SIGMA_MIN_BPM = 2.0` before 5 clean sessions), `3.0` for `sigma_wake`, `0.70` for `mu_ln_rmssd_sleep`.
    - Weight-scaled step size: $\alpha = \min(0.08 \cdot w / 0.5,\ 0.08)$ for $w \le 0.5$ (`0.08` implicit, `0.056` suspend/poweroff) and $\min(0.08 \cdot w / 0.5,\ 0.25)$ for $w > 0.5$ (`0.25` capped for manual onset `1.75` and manual HR `2.0`).
    - If `self.current` is `None` upon reaching EWMA (e.g., first `mu_sleep` after 6 `Suspend` sessions without Day-1 seed), `update_ewma` cleanly initializes via `self.seed(obs, weight)`.
  - **Tier thresholds (`recompute.rs:16-17,655-663`):** `0..=2` $\to$ `ColdStart`, `3..=6` $\to$ `Learning`, `7+` $\to$ `Calibrated`.
- **Per-Wearable floor (`recompute.rs:151-189`):**
  - Clamps `sleep_hr_var_ratio` to `[0.05, 0.80]`. First `6` valid floor observations use EB toward `0.25` with `FLOOR_EB_KAPPA = 2.0`; subsequent use EWMA with `FLOOR_EWMA_ALPHA = 0.15`.
  - **Subtlety:** `WearableAccumulator::observe` excludes `FeedbackVerdict::AnomalousExclude` (`manually_excluded`), but runs *before* MAD auto-quarantine (line 324 vs. 347), so an auto-anomalous session with `sleep_hr_var_ratio` still updates `hr_var_floor`.

### 2.2 Verdicts, Quarantine, & Threshold Adjustments
- **`AnomalousExclude` (`recompute.rs:340-344`):** Increments `quarantined_sessions`, resets `consecutive_anomaly_count`, and returns immediately (`0.0` drift).
- **MAD quarantine (`recompute.rs:347-365,560-570,585-630`):** Activates once `clean_wake_hrs.len() >= 5` (rolling window `30`). Uses `scaled_mad` floored at `2.0 bpm`. If `|warmup_mu_hr - median| > 2.5 * mad_s`, increments `quarantined_sessions`, tracks same-direction `ShiftDirection::{Higher, Lower}` streak (`PERSISTENT_SHIFT_STREAK = 5`), and returns with `0.0` personal drift. `recompute.rs` ignores the stored `auto_anomalous` column and recomputes MAD quarantine purely from the ledger, guaranteeing edit/delete invariance.
- **Verdicts & FP caps (`recompute.rs:367-413,460-462,761-774`):**
  - `is_fp = verdict == FalsePositive || grace_cancel_count > 0 || rapid_wake_fp_count > 0 || rapid_resume_within_5m`. Capped at 1 FP update per session.
  - `strong_fp` (`FalsePositive` or `grace_cancel_count > 0`): `tau += +0.015`, `dwell += +30 s`; `rapid_wake` only: `tau += +0.01125`, `dwell += +20 s`. Clamped to `tau in [-0.03, +0.10]`, `dwell in [-60, 300]`.
  - `Snooze` (`snooze_count`): zero FP penalty.
  - `MissedSleep`: `tau += -0.010`, `dwell += -15 s`.
  - True-positive decay (`recompute.rs:460-462`): if `has_implicit_sleep && !has_manual_sleep && tau_p_sleep_offset > 0.0`, decays by `ETA_TP_TAU = -0.003` down to `0.0`.
  - **Discrepancy (double nudge):** When `verdict == MissedSleep` AND `manual_onset_ts_ms.is_some()` on a missed session (`onset_confirmed_ts_ms == None`), both `ETA_MISSED_TAU` (`-0.010`, line 382) and `apply_manual_onset_threshold_nudge` (`-0.005`, lines 394 & 768) apply (`-0.015` total).
  - **Omitted verdict (`None`):** Wake-only sessions (`post_trigger_sleep_sec < 900`, no suspend/poweroff, no manual overrides) return early at line 412 with zero effect on `valid_sessions` or thresholds.

### 2.3 `--onset HH:MM` Parsing, Midnight-Crossing, & `[onset, onset + 15 min]` Extraction
- **Backend/CLI (`src/config/schema.rs:316-340`, `src/storage/store/session.rs:747-806`):**
  - `ClockTime::from_str` enforces strict 5-char `HH:MM` (`00:00..=23:59`).
  - `resolve_onset_hhmm_ms` scans local dates `[start_date - 1, end_date + 1]`, handles `LocalResult::Ambiguous` (fall-back DST) and `LocalResult::None` (spring-forward gap), and selects the first timestamp inside `[start_ts_ms, end_ts_ms]`.
- **TUI `FeedbackModal` discrepancy (`src/tui/input/modal_actions.rs:719-773`, `src/tui/client_commands.rs:252-255`):**
  1. `modal_actions.rs:721-730` accepts single-digit hours like `9:30`, which then fails `ClockTime::from_str` in `client_commands.rs:254` (`and_then(|s| s.parse::<ClockTime>().ok())`), **silently dropping `onset_hhmm` to `PatchField::Unchanged`** instead of surfacing an error or zero-padding.
  2. `modal_actions.rs:732-757` uses `.single()` on `Local`, which rejects valid timestamps during fall-back DST ambiguous hours.
- **Epoch extraction (`extract_onset_evidence`, `src/calibration/recompute.rs:699-759`):**
  - Selects `!ep.accumulators_frozen` epochs in `[onset_ts_ms, onset_ts_ms + 900_000]` (`15 min`), falling back to `[onset_ts_ms - 120_000, onset_ts_ms + 900_000]` if empty.
  - Missing/frozen window returns `None` (no `sleep_hr`/`delta_hr`/`ln_rmssd` extracted; `apply_manual_onset_threshold_nudge` still executes).
  - Low-SQI (`hrv_valid == false`) epochs inside a valid BPM window (`!accumulators_frozen`) still yield median `sleep_hr` (`hr_bpm_60s`) and `delta_hr` (`sleep_hr - wake_ref`), while `ln_rmssd` filters on `ep.hrv_valid` (`recompute.rs:748`) and safely yields `None`.

---

## 3. Silent `pause_media` Calibration-Night End-to-End Flow (vs. ADR-0011 & Daemon/Storage)

Tracing the calibration-night configuration (`actions.pause_media = true`, `0` active MPRIS players, `lock_screen = false`, `power_action = PowerAction::None`, `custom_command = []`):

1. **Confirmation & `DaemonCore::handle_confirmed_sleep_onset` (`src/daemon/state/grace.rs:233-318`):**
   - `SleepEngine::step_candidate` sets `confirmed_ts_ms` and `onset_hr_bpm`, emitting `StateChanged { to: Confirmed }`.
   - `MonitoringContext::process_outputs` (`src/daemon/monitoring.rs:309-317`) updates `DaemonCore` and awaits `handle_confirmed_sleep_onset(ts_ms)`.
   - After verifying `!is_paused`, `is_within_active_hours` (if enabled), and trailing `120 s` desktop inactivity (`AbortedNoPenalty` if active), lines `297–318` check:
     `pause_media && custom_command.is_empty() && !lock_screen && power_action == PowerAction::None`
     and query `action_executor.has_playing_players(&ignore_players)`.
   - When `has_playing == false` (`0` players), it skips `GraceCoordinator::run` (no desktop notification, no `GraceTick`) and calls `deliver_grace_outcome(GraceOutcome { ts_ms, outcome: GraceResult::Expired })`, returning at line `316`.
2. **`dry_run = true` vs. `dry_run = false` Bug (`grace.rs:297-318` vs. `385,509`):**
   - In the normal grace branch (`grace.rs:385,509`), `deliver_grace_outcome` is skipped whenever `core.is_dry_run()` is `true`, preserving ADR-0007's zero-calibration-feedback guarantee.
   - In the silent `pause_media` (`0` players) fast-path (`grace.rs:297-318`)—as well as the `is_paused` (`250-260`) and `outside active_hours` (`262-276`) early exits—`self.is_dry_run()` is not checked. Running `bpmute daemon --dry-run` with `pause_media = true` and `0` active players silently transitions `SleepEngine` to `Sleeping`, accumulates `post_trigger_sleep_sec`, and records live calibration on session close.
3. **Sleeping Accumulation, Persistence, & Next-Day Recompute (`dry_run = false`):**
   - `MonitoringContext::handle_control_event` (`src/daemon/monitoring.rs:186-193`) feeds `GraceOutcome::Expired` to `SleepEngine::handle_grace_outcome` (`lifecycle.rs:110-123`), transitioning `Confirmed -> Sleeping` and setting `sleeping_ts_ms = Some(ts_ms)`.
   - **Async/Persistence nuances:**
     - `run_monitoring_loop` (`src/daemon/monitoring.rs:418`) uses non-biased `tokio::select!`; under batched packet bursts, `wearable_rx` could drain extra packets in `Confirmed` before `ctrl_rx` delivers `GraceOutcome::Expired`.
     - `mark_session_sleep_trigger` (`grace.rs:391-401`) only runs for `Suspend | Poweroff`, so an ungraceful daemon crash (`SIGKILL`) overnight with `PowerAction::None` leaves `sleeping_ts_ms` and overnight summaries `NULL` upon `recover_stale_sessions` (`src/storage/store/session_lifecycle.rs:12-49`).
     - The 5-minute post-trigger desktop activity watch (`grace.rs:438-495`) is not spawned on the early return at line `316`, and `src/daemon/` never injects `EngineEvent::DesktopActivity` (`DesktopWakeRebound` only triggers in replay/tests).
   - Under normal streaming in `Sleeping` (`state_machine/mod.rs:389-415`), every clean epoch appends `hr_bpm_60s`, `ln_rmssd_60s` (when `hrv_valid`), and `unclamped_var_ratio` (`local_mad_120s / sigma_hr`), and increments `post_trigger_sleep_sec += 15` whenever `p_sleep >= 0.30`. Sustained wakefulness (`post_sleep_wake_sec >= 600`), `> 15 min` gap, wearable switch, or daemon shutdown closes the session.
   - `close_session` (`lifecycle.rs:382-435`) computes medians (`sleep_mu_hr`, `sleep_ln_rmssd`, `observed_hr_var_ratio`) and mean `typical_sqi`; `MonitoringContext` calls `close_session_and_recompute` (`src/storage/store/session.rs:192-286`), which persists all session summary columns and updates `calibration_state` and `wearables`. Next-day `recompute_from_scratch` / `set_feedback_and_recompute` loads these exact persisted rows and `session_epochs` cleanly.
4. **`CalibrationSetSeed` TUI vs. CLI Gap:**
   - **Action semantics:** CLI `bpmute calibration seed <BPM>` (`src/cli/commands/sessions.rs:580-643`) calls `calibration.set_seed`, updating the Day-1 seed (or active boundary seed) without inserting a `CalibrationBoundary`. TUI `TuiCommand::CalibrationSetSeed` (`src/tui/event.rs:123`, `src/tui/client_commands.rs:344-359`) is unreachable from any UI view/keybinding; TUI Settings `[B]` (`SoftRebaselineModal`, `src/tui/input/mod.rs:338-349`, `src/tui/input/modal_actions.rs:603-630`) always emits `TuiCommand::CalibrationSetBoundary` (`calibration.set_boundary`), inserting a new `ManualRebaseline` boundary that resets personal calibration to `Cold Start`.
   - **Validation range:** Both `SoftRebaselineModal` (`modal_actions.rs:617-619`) and `FeedbackModal` (`modal_actions.rs:779-780`, `src/tui/views/history_modals.rs:191`) enforce `30.0..=120.0` BPM, whereas `validate_sleep_hr_bpm` (`src/calibration/types.rs:331-338`), CLI (`sessions.rs:309,582`), and `LedgerStore` (`storage/store/calibration.rs:44,170`, `storage/store/session.rs:394`) enforce `30.0..=140.0` BPM.

---

## 4. Test Coverage & Audit Classification Matrix

### 4.1 Classification & Existing Test Mapping

| Component / Requirement | Classification | Existing Coverage |
|---|---|---|
| Stage 1–3 PPG filters (bounds, Hampel, LT RR, SQI, mode-aware flatline, coverage freeze) | **Verified Sound** | `tests/sleep_engine.rs`: `slice1_stage1_bounds_contact_gate_and_coverage_freeze`, `slice2_hampel_filter_lt_rr_sqi_and_mode_aware_flatlines`, `tier1_gt5_optical_lock_ramp_flatlines_hampel_and_silent_gaps`, `review_regression_desktop_candidate_cancel_grace_outcomes_single_gap_and_rr_drop_flatline` |
| Warmup durations (`32` vs. `20`), `QuietWakeBaseline` spike exclusion & `P >= 0.30` freeze | **Verified Sound** | `tests/sleep_engine.rs`: `slice3_warmup_durations_hmm_baseline_freeze_and_hrv_marginalization`, `tier1_sleep_positives_paired_parity_and_slow_gradual_descent`, `tier1_hard_wake_negatives_zero_false_positives` |
| HMM transitions, HRV marginalization, presets, hysteresis, tiered gaps, 1:1 split | **Verified Sound** | `tests/sleep_engine.rs`: `slice4_presets_dwell_grace_outcomes_cooldown_and_anomalous_detection`, `slice4_tiered_gaps_and_post_sleep_1_to_1_session_splitting`, `tier1_calibrated_and_cold_start_post_workout_recovery_zero_false_positives`; `tests/monte_carlo.rs`: `tier2_seeded_monte_carlo_sweep` (`#[ignore]`) |
| HMM Student-$t$ log-norm constants (`LOG_NORM_NU3`, `LOG_NORM_NU5`) & unused `CalibrationPriors` fields | **Minor Discrepancy** | Constants cancel identically in normalization; `mu_ln_rmssd_sleep`/`sigma_wake` unused in `SleepEngine` |
| Calibration EB $\to$ EWMA, `NADIR_OFFSET_BPM`, weights, MAD quarantine, boundaries, wearable floor isolation | **Verified Sound** | `tests/calibration_ledger.rs`: `maturity_progression_day1_seed_and_10_session_convergence`, `feedback_weight_matrix_suspend_cancel_snooze_and_manual_overrides`, `mad_quarantine_zero_drift_persistent_shift_boundary_and_dismissal`, `hybrid_wearable_scoping_and_bitwise_round_trip_determinism`; `tests/privacy_retention_portability.rs`: `test_reversible_calibration_boundary_rebaseline_and_undo` |
| `MissedSleep` + `--onset` double tau nudge (`-0.015` when `onset_confirmed_ts_ms` is `None`) | **Minor Discrepancy / Edge-Case Risk** | Partially exercised in `tests/calibration_ledger.rs:367-400` (asserts `< 0.0`, not exact `-0.015` vs. `-0.010`) |
| `--onset HH:MM` midnight-crossing & `session.set_feedback` persistence | **Verified Sound** (backend/CLI) / **Bug** (TUI) | `tests/ledger_daemon_cli.rs`: `cli_sessions_calibration_online_offline_and_dual_fidelity_replay` (`--onset 23:32` wraparound) |
| Grace Period `ActivityHold` & action pipeline | **Verified Sound** | `tests/grace_period.rs`; `tests/daemon_idle_e2e.rs`; `tests/daemon_actions_e2e.rs` |
| Silent `pause_media` (`0` players) under `dry_run = true` | **Bug / Blocker** | `tests/daemon_actions_e2e.rs:450` (`test_dry_run_records_zero_calibration_feedback`) does not use a live wearable session |
| Silent `pause_media` (`0` players) calibration-night full E2E | **Untested Gap** | `tests/daemon_actions_e2e.rs:181` only checks `notifier.events().is_empty()` without a live session or DB check |
| TUI `SoftRebaselineModal` / `FeedbackModal` (`30..=120` vs. `30..=140`), dead `CalibrationSetSeed`, TUI `9:30` onset drop | **Bug / Blocker** | `tests/tui_settings_e2e.rs:418,666`; `tests/tui_history_e2e.rs:234,743` only test valid `54.5`/`57.5` and `HH:MM` |

### 4.2 Concrete Missing Test Cases to Add
1. **Worked-example numerical unit test for `HmmFilter::step` (`src/sleep_engine/hmm.rs`):** Pin exact single-step posterior outputs `[p_active, p_quiet, p_sleep]` for known `(z_hr, hr_slope_4m, r_var_120s)` with and without HRV (`BPM+RR` vs. `BPM-only`), verifying non-tautological Student-$t$ log-PDF math and normalization.
2. **Worked-example numerical unit test for `recompute_calibration` (`tests/calibration_ledger.rs`):** Assert exact values across:
   - Day-1 seed (`55.0` -> `57.5`, `w = 1.5`) + 6 `HostSuspend` sessions (`w = 0.35`, `mu_sleep` unchanged) + 7th EWMA transition session (`mu_sleep` initializes via `seed` in `update_ewma`),
   - Exact `delta_hr_onset` EB shrinkage with `EB_KAPPA_0 = 4.0`,
   - Exact `tau_p_sleep_offset` under `MissedSleep` with and without `--onset`,
   - `extract_onset_evidence` with `hrv_valid = false` (low SQI) in `[onset, onset + 15 min]` and with all epochs frozen (`accumulators_frozen = true`) returning `None`.
3. **End-to-end silent `pause_media` (`0` active players) calibration-night E2E (`tests/daemon_actions_e2e.rs`):** Drive a full `TestWearableSource` sleep trace through `Warmup` -> `Confirmed` -> silent `Sleeping` (`>= 15 min`) -> `SustainedWakefulness` / `StreamEnded`, verifying `notifier.events().is_empty()`, `sleeping_ts_ms.is_some()`, `post_trigger_sleep_sec >= 900`, `sleep_mu_hr.is_some()`, `cal.valid_sessions == 1`, and next-day `recompute_from_scratch()` parity.
4. **Live-session `dry_run = true` + `pause_media = true` (`0` players) regression test (`tests/daemon_actions_e2e.rs`):** Feed a real `TestWearableSource` sleep trace under `actions.dry_run = true` and `0` active players and assert `cal.valid_sessions == 0` (currently fails due to Bug #1).
5. **TUI boundary & `HH:MM` regression tests (`tests/tui_settings_e2e.rs`, `tests/tui_history_e2e.rs`):** Verify `135.0` BPM acceptance (once aligned to `30..=140`), rejection/zero-padding of `9:30` onset in `FeedbackModal`, and DST ambiguous local-time handling.
