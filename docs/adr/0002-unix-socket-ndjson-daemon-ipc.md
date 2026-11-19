---
status: accepted
---

# Unix Domain Socket with NDJSON for Daemon–TUI IPC

Primary IPC between `bpmute daemon` and the `bpmute` TUI/CLI uses a user-private Unix domain socket at `$XDG_RUNTIME_DIR/bpmute/daemon.sock` carrying newline-delimited JSON (NDJSON), rather than exposing a Session D-Bus service in v1. This keeps high-frequency 1 Hz heart-rate telemetry, rolling sparkline snapshots, and state transitions lightweight and decoupled from session D-Bus restarts, while relying on `$XDG_RUNTIME_DIR`'s `0700` per-user isolation.

## Considered Options

- **Session D-Bus (`org.bpmute.Daemon`):** Directly inspectable via `busctl`/`gdbus` for desktop bar scripts, but adds D-Bus variant/struct marshaling boilerplate for nested 1 Hz telemetry streams and fails when the session bus is absent or restarted.
- **Both Unix socket and read-only D-Bus in v1:** Doubles the IPC surface before the telemetry and state schemas stabilize.
- **Separate control and telemetry sockets, or poll-only HTTP/JSON:** Adds lifecycle and reconnect coordination or loses instant reactivity for `Grace Period` banners and interactive Bluetooth `Agent1` pairing prompts.

## Consequences

- A single multiplexed connection supports both one-shot CLI request/response calls (`{"v":1,"id":...,"op":...,"params":...}`) and long-lived TUI `subscribe` push streams (`{"v":1,"event":...,"seq":...,"data":...}`), starting with an immediate `Snapshot` (full status + rolling `30 min` HR ring buffer + active alerts) and dropping stale `1 Hz` `HrSample` ticks on slow clients rather than blocking the daemon.
- Frames are capped at `1 MiB` per NDJSON line; bulk trace, `Ledger Bundle`, and SQLite backup operations exchange filesystem paths (`0600`) rather than streaming multi-MB payloads inline.
- Because `bpmute daemon` owns BlueZ and the temporary `Enrollment`-scoped `org.bluez.Agent1`, interactive SMP passkey/PIN prompts are bridged over IPC under a single-owner enrollment lock (`ENROLLMENT_IN_PROGRESS` for concurrent callers) that auto-cancels on client disconnect or a `30 s` timeout.
- External bar integrations (e.g., Waybar, Polybar) consume `bpmute status --json`, which emits the exact `daemon.status` schema when connected and `{"v":1,"daemon_running":false}` with a non-zero exit code when offline.
- A secondary read-only Session D-Bus interface can be added post-v1 without breaking the Unix socket protocol.
