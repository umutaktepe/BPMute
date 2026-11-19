---
status: accepted
---

# Unix Domain Socket with NDJSON for Daemon–TUI IPC

Primary IPC between `bpmute daemon` and the `bpmute` TUI/CLI uses a user-private Unix domain socket at `$XDG_RUNTIME_DIR/bpmute/daemon.sock` carrying newline-delimited JSON (NDJSON), rather than exposing a Session D-Bus service in v1. This keeps high-frequency 1 Hz heart-rate telemetry, rolling sparkline snapshots, and state transitions lightweight and decoupled from session D-Bus restarts, while relying on `$XDG_RUNTIME_DIR`'s `0700` per-user isolation.

## Considered Options

- **Session D-Bus (`org.bpmute.Daemon`):** Directly inspectable via `busctl`/`gdbus` for desktop bar scripts, but adds D-Bus variant/struct marshaling boilerplate for nested 1 Hz telemetry streams and fails when the session bus is absent or restarted.
- **Both Unix socket and read-only D-Bus in v1:** Doubles the IPC surface before the telemetry and state schemas stabilize.

## Consequences

- External bar integrations (e.g., Waybar, Polybar) consume `bpmute status --json` in v1.
- A secondary read-only Session D-Bus interface can be added post-v1 without breaking the Unix socket protocol.
