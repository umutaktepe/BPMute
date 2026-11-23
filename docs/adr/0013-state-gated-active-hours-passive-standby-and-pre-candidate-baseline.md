---
status: accepted
---

# State-Gated Active Hours, Passive Standby, and Pre-Candidate Sleep Calibration Reference

BPMute unifies `schedule.active_hours_enabled` with session lifecycle and calibration isolation: outside Active Hours, streaming Wearables enter `DetectorState::Standby` (maintaining live TUI/CLI telemetry without opening a SQLite `Monitoring Session`, running HMM candidate evaluation, or storing epochs); at Active Hours window start, the daemon opens a fresh `Monitoring Session` (`Warmup` → `Monitoring`); upon crossing into `Candidate`, the engine snapshots `pre_candidate_mu_hr`, `pre_candidate_sigma_hr`, and `pre_candidate_ln_rmssd` from the active 15-minute `QuietWakeBaseline` as the primary waking reference for calibration recomputation; and at Active Hours window end, an active `Sleeping` state is preserved until natural morning wakefulness confirms closure before transitioning to `Standby`, while an unconfirmed `Monitoring` state closes cleanly as `SessionEndReason::ActiveHoursExpired`—refining ADR-0004, ADR-0005, and ADR-0012.

## Considered Options

- **Naive time guillotine at window end (`08:00`):** Abruptly closes any active session when the clock hits the configured end time, but chops morning sleep short, loses the final sleep cycle, and prevents 10-minute `SustainedWakefulness` from resolving cleanly when a user wakes near `08:00`.
- **Separate opt-in configuration flag (`schedule.standby_outside_active_hours`):** Avoids changing behavior for existing `active_hours_enabled = true` users, but introduces redundant configuration surface when users enabling active hours already expect the daemon not to track sleep or execute desktop automations during waking daytime hours.
- **Retaining `warmup_mu_hr` as the sole waking calibration reference:** Simpler schema and zero migration, but permanently ties personal wake calibration ($\mu_{\text{wake}}$ and $\Delta\text{HR}_{\text{onset}}$) to whichever hour the wearable was first donned or the window opened (e.g., `23:05`), poisoning calibration whenever the user stays up working or gaming until `03:00`.
- **State-gated window + `Standby` mode + `pre_candidate` baseline snapshot (chosen):** Delivers zero-touch 24/7 wearable support, isolates daytime telemetry from SQLite storage and calibration ledger drift, captures the user's authentic quiet-wake reference immediately prior to sleep onset regardless of bedtime hour, and protects morning sleep completion across the window boundary.

## Consequences

- `DetectorState` gains `Standby`, representing an enrolled Wearable that is actively connected and streaming valid heart-rate data while sleep-onset detection and session recording are suspended outside Active Hours.
- `DaemonCore` evaluates local clock transitions against `ScheduleConfig`:
  - When entering Active Hours while an enrolled Wearable is streaming in `Standby`, `DaemonCore` signals `MonitoringContext` to open a fresh `Monitoring Session` starting in `Warmup`.
  - When connecting an enrolled Wearable outside Active Hours, the engine enters `DetectorState::Standby` immediately without creating a row in `sessions` or accumulating `session_epochs`.
  - When exiting Active Hours while in `DetectorState::Monitoring`, the engine closes the session with `SessionEndReason::ActiveHoursExpired` (persisting a neutral zero-sleep summary that applies zero calibration penalty or drift) and enters `Standby`.
  - When exiting Active Hours while in `DetectorState::Sleeping`, the session remains fully active until `SustainedWakefulness`, `DesktopWakeRebound`, or disconnect resolves closure, after which the engine transitions directly to `Standby`.
- `SessionSummary` and SQLite `sessions` gain optional columns `pre_candidate_mu_hr`, `pre_candidate_sigma_hr`, and `pre_candidate_ln_rmssd` (`REAL NULL`). In `recompute_calibration`, `wake_ref` resolves to `session.pre_candidate_mu_hr.or(session.warmup_mu_hr)`, ensuring seamless backwards compatibility with historical ledger sessions.
- TUI Dashboard and CLI `bpmute status` display live streaming heart rate and sparklines during `Standby` while rendering state badge `[STANDBY - Arms at HH:MM]`.
