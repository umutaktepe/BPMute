---
status: accepted
---

# Disconnect-Reason Session Closure Window, Countdown Visibility, and Manual End

BPMute retains the `15 min` (`MEDIUM_GAP_MAX_MS`) `Monitoring Session` closure window for RF dropouts (`Timeout`), `Signal Lost`, and legacy BlueZ hosts (`< 5.79`), while applying a shortened `2 min` (`SHORT_GAP_MAX_MS`) grace window when BlueZ reports an explicit remote termination (`org.bluez.Device1.Disconnected` with `org.bluez.Reason.Remote`), exposing the active close deadline (`session_close_deadline_ms`) in `DaemonStatus` for TUI/CLI countdown visibility, and permitting explicit manual closure (`session.end` / `bpmute sessions end` / TUI `[E] End Session`) strictly while disconnected or in `Signal Lost`—refining ADR-0003 and ADR-0004.

## Considered Options

- **Immediate (`0 s`) closure on `org.bluez.Reason.Remote`:** Makes morning shutdown instant, but risks splitting an overnight `Monitoring Session` (and forcing a fresh `5–8 min` Warmup) on an accidental watch button press or firmware reset/battery-drop HCI `0x13` termination.
- **Shortening the global gap timeout for all disconnects (e.g., `5 min` instead of `15 min`):** Simplifies the state machine and avoids BlueZ signal handling, but splits overnight sessions when a user briefly leaves Bluetooth range (bathroom/kitchen) for 5–15 minutes.
- **Allowing manual `session.end` while a Wearable is still `Streaming`:** Lets users force-split a live stream, but conflicts with `SleepEngine`'s auto-open invariant where the very next 1 Hz HR packet immediately opens a new `Monitoring Session` after `Idle`, producing confusing UX.
- **Tiered `2 min` remote-disconnect window + `15 min` fallback + countdown visibility + gap-gated manual close (chosen):** Preserves overnight room-exit resilience, tolerates brief accidental watch exits within the existing `SHORT_GAP_MAX_MS` (`2 min`) tier, degrades cleanly on BlueZ `< 5.79` where `Device1.Disconnected` is unavailable, and gives users transparent countdown feedback plus immediate one-key manual closure (`SessionEndReason::ManualClose`).

## Consequences

- `BluezWearableSource` subscribes to `org.bluez.Device1.Disconnected(reason, message)` as a progressive enhancement alongside `PropertiesChanged(Connected = false)` (reconciling D-Bus signal arrival order so a `Remote` reason received immediately before or after `Connected = false` tightens the active gap deadline to `min(existing_deadline, disconnect_ts + 2 min)` and dispatches `NotifyDisconnected` without the `15 s` RF debounce, while also capturing `Reason.Authentication`).
- `DaemonStatus` carries an optional `session_close_deadline_ms: Option<i64>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`) updated on gap start, deadline tightening, resume, and session close so TUI (`Sleep State` card and History `[CLOSING]` status) and `bpmute status` render a live countdown without per-second IPC traffic.
- Manual closure (`session.end`, `bpmute sessions end`, and confirmation-free TUI `[E] End Session` on Dashboard and History) is permitted only while a session has an active disconnect/`Signal Lost` gap, persists `end_ts_ms = gap_started_ms` with `SessionEndReason::ManualClose` (`"manual_close"`), and immediately recomputes calibration; automatic `2 min` remote-disconnect and `15 min` timeout closures continue to record `SessionEndReason::GapExceeded`.
