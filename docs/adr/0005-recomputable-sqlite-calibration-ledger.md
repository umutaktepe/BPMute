---
status: accepted
---

# Deterministic Recomputable Calibration Ledger Over SQLite

BPMute v1 persists immutable per-session extracted summaries and `15 s` Epochs alongside editable `session_feedback` rows in SQLite (WAL mode) and deterministically recomputes the user's Empirical Bayes → capped-EWMA calibration snapshot (`calibration_state`) chronologically whenever a Monitoring Session closes or a user edits, excludes, or resets past session feedback. Personal physiological parameters (`mu_wake`, `sigma_wake`, `mu_sleep`, `delta_hr_onset`, `mu_ln_rmssd_sleep`, decision threshold/dwell adjustments) are shared across Wearables, while sensor-specific noise floors (`hr_var_floor`, capabilities, typical SQI) are isolated per Wearable.

## Considered Options

- **In-place incremental EWMA / Bayesian state updates:** Mutates running posterior columns directly on disk after each night, which is marginally simpler to write initially but makes retroactive manual sleep-onset replay, next-morning sick-night quarantines, typo corrections, and algorithm version upgrades approximate or irreversible.
- **Strictly per-Wearable calibration profiles:** Prevents cross-sensor contamination between an optical wrist Wearable and an ECG chest strap, but forces users to re-complete `Cold Start` from scratch whenever they switch Wearables even though their underlying resting and sleeping heart rate belongs to the same person.
- **Single global profile with no per-Wearable signal floor:** Pools everything without isolating optical vs. chest-strap variance floors, letting noisier wrist PPG distort a chest strap's `R_var` baseline (and vice versa).

## Consequences

- Marking a past Monitoring Session as an `Anomalous Session`, entering or editing a Wearable companion app's sleep onset timestamp or average sleeping HR, or accepting a 5-session consecutive-anomaly re-baseline prompt produces an exact, auditable, drift-free `calibration_state` in under a millisecond.
- Uncancelled host `Suspend` / `PowerOff` triggers (where no 15-minute post-trigger stream exists and the host does not resume within 5 minutes) contribute a lower-weight implicit positive ($w = 0.35$) to waking baseline and onset drop only, preventing suspending users from stalling in `Cold Start`.
- Raw 1 Hz BLE packets (`raw_samples`) are stored separately under rolling retention for diagnostics and filter replay without affecting long-term ledger recomputation, which depends only on permanent session summaries, `15 s` Epochs, and `session_feedback`.
