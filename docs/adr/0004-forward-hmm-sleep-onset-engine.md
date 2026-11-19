---
status: accepted
---

# 3-State Forward-Filtered HMM Inside a Deterministic Sleep-Onset State Machine

BPMute v1 detects sleep onset using a 3-state online Forward-Filtered Hidden Markov Model (`ActiveWake`, `QuietWake`, `SleepNREM`) with sticky self-transitions and robust Student-$t$ emissions wrapped inside a deterministic state machine (`Warmup` → `Monitoring` → `Candidate` → `Confirmed` → `Sleeping`). When a Wearable streams BPM-only data (such as the Huawei Watch GT5) or beat-to-beat RR intervals fail the 5% Signal Quality Index artifact gate, the HMM natively marginalizes out the HRV dimensions with zero mode-switching discontinuity, while the outer state machine enforces asymmetric specificity-first confirmation dwell (`3 min` for `BPM+RR`, `5 min` for `BPM-only`) and mode-aware flatline gating (`30 s` on `BPM+RR`, `120 s` on `BPM-only`).

## Considered Options

- **Composite one-sided Lower CUSUM:** Minimax optimal for univariate downward shifts with $O(1)$ arithmetic, but requires manual re-weighting and re-scaling whenever RR intervals drop in or out and yields an unbounded score rather than a calibrated `P(SleepNREM)` probability for TUI display and baseline gating.
- **Pre-trained neural sequence model (LSTM / CNN):** Achieves strong multi-stage sleep staging on large ECG/PPG datasets, but adds opaque weights, heavy inference/runtime complexity, and brittle personalization from sparse per-user labels compared to an interpretable parametric HMM.
- **Uniform 30 s flatline PPG gate across all Wearables:** Protects against frozen optical firmware on `BPM+RR` streams, but risks falsely freezing detection during legitimate steady N2 sleep on 1 Hz integer-quantized `BPM-only` Wearables where identical integer BPM values can persist for 30–60 seconds.

## Consequences

- `BPM+RR` and `BPM-only` Wearables share one unified `15 s` Epoch feature pipeline (`z_HR`, `HR_slope`, `R_var`, plus optional `Δln(RMSSD)` and `RMSSD/SDNN`) and one probability threshold across sensitivity presets (`Conservative` / `Balanced` / `Responsive`).
- The Quiet-Wake Baseline re-anchors continuously during calm wakefulness (`P(SleepNREM) < 0.30` and non-negative/flat 4-minute HR slope) and freezes immediately once `P(SleepNREM) ≥ 0.30` so evening desk activity does not poison bedtime detection and slow N1 deceleration is never chased downward.
- Sustained post-sleep wakefulness (`≥ 10 min` with `P(SleepNREM) < 0.30` or desktop input plus HR rebound) closes the completed Monitoring Session and opens a new one, preserving a strict 1:1 relationship between Monitoring Sessions and sleep onsets.
