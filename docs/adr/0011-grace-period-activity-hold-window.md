---
status: accepted
---

# Fixed Activity Hold Window Before Desktop-Activity Grace Period Cancellation

Desktop user activity detected during a `Grace Period` does not immediately close the notification or cancel with `GraceResult::Cancelled`. Instead, `GraceCoordinator` transitions from the `Sleep Action Pipeline` countdown into a fixed `10 s` non-resetting `Activity Hold` window (pausing sleep action execution and giving the user time to click `Snooze`, press `Cancel`, click the notification body, or run CLI/TUI `snooze`), and automatically resolves to `GraceResult::Cancelled` (`strong_fp` calibration penalty + `15 min` cooldown) if the window expires without a user choice—refining the Grace Period activity-cancellation flow in ADR-0006.

## Considered Options

- **Immediate `GraceResult::Cancelled` on desktop input (previous behavior):** Prevents sleepy users from moving the pointer to click `Snooze` or typing `bpmute snooze` without triggering an immediate false-positive cancellation and closing the notification under their cursor.
- **Disable passive desktop-activity cancellation during `Grace Period`:** Relies strictly on the `2 min` pre-trigger gate and explicit button/TUI/CLI actions, risking unintended `suspend`/`poweroff` execution if an awake user ignores or misses the notification.
- **Resuming the `Grace Period` countdown if no further input arrives during `Activity Hold`:** Tolerates accidental pointer bumps, but leaves a user who pressed a single key (e.g., Space or Esc) expecting cancellation vulnerable to sleep execution `10 s` later, and complicates `ext-idle-notify-v1` handling across compositors.
- **Activity-triggered `AbortedNoPenalty` instead of `GraceResult::Cancelled`:** Avoids penalizing calibration when desktop input occurs, but hides genuine waking false positives when an awake user dismisses or ignores the prompt and resumes working.
- **Fixed `10 s` `Activity Hold` window with unconditional auto-cancel (chosen):** Preserves `Snooze` accessibility across notifications, TUI, and CLI, bounds notification lifetime to at most `10 s`, prevents late-countdown (`< 10 s` remaining) races, and preserves false-positive learning when the user does not choose `Snooze`.

## Consequences

- `GraceCoordinator` tracks a two-phase Grace Period state (`Countdown` vs. `ActivityHold`), resetting the active remaining timer to `10 s` on the first post-settle desktop activity event and ignoring subsequent idle-probe triggers so continuous pointer/keyboard use cannot extend the hold indefinitely.
- `Notifier::show_grace_notice`, `GraceTickEvent`, and TUI banner rendering distinguish the two phases explicitly (correcting the normal countdown body from `"Cancelling in ..."` to `"Sleep action in ..."` and showing `"Activity detected — cancelling in ..."` / `ACTIVITY HOLD: Xs` during `Activity Hold`), using `#[serde(default)]` on new IPC fields for backward compatibility.
- `NotificationClosed(reason = 2)` keeps its ADR-0006 non-cancelling semantics (`notice_dismissed = true`), and the notification `"default"` action remains mapped to `Cancel`; `ArousalAbort`, `SignalLost`, and `PrepareForSleep` continue to preempt immediately in both phases.
