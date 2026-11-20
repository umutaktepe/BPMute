# Async & Integration Testing

Testing conventions, concurrency patterns, and pitfalls for BPMute daemons, worker threads, and CLI integration tests.

## Concurrency & Worker Threads

- **Explicit worker shutdown**: Any OS thread worker (`bpmute-ledger`) communicating via channels must use an explicit shutdown variant (e.g., `DbCommand::Shutdown`) and join its thread. Never rely on dropping sender handles for channel EOF when cloned handles (`PersistenceHandle`) or async tasks may still keep a sender alive.
- **Async handle shutdown**: Provide an async shutdown method (e.g., `shutdown_async`) that sends the shutdown command and awaits thread completion via `tokio::task::spawn_blocking`. Always implement `Drop` on the worker as a fallback.

## Tokio Virtual Time (`start_paused = true`)

- **Virtual-time sleeps**: Any async test exercising countdowns, settle delays, or timeouts must use `#[tokio::test(start_paused = true)]` (or explicit `tokio::time::pause()`). Never use `tokio::time::sleep`, `tokio::time::timeout`, or `tokio::time::interval` inside unpaused `#[tokio::test]` blocks (`./scripts/check.sh` enforces this); virtual time advances instantaneously, keeping the test suite runtime under target bounds (ADR-0007).
- **OS thread I/O and `UnixStream` auto-advance under virtual time**: In `#[tokio::test(start_paused = true)]`, Tokio auto-advances virtual time whenever all tasks park waiting for a real `UnixStream` or the OS SQLite worker thread. Never wrap IPC/socket calls in `tokio::time::timeout(...)`, and when testing cancel/snooze flows before expiry over a real socket, configure a generous deadline (e.g., `grace_sec: 300`) and advance virtual time in small increments rather than using a tight `15s` timer that can expire mid-IPC under parallel test load.
- **Stream event synchronization**: When feeding simulated wearable streams into `FakeWearableSource`, wait for the expected stream events (e.g., `EpochUpdate`) to arrive before issuing live reconfiguration calls (such as `config.update`). Otherwise, `tokio::select!` in the monitoring loop can interleave the config change before the epoch features are processed and stored.
- **In-process mock servers with child processes**: Tests hosting an in-process async server (such as a mock `org.bluez` D-Bus server) while spawning child CLI processes that call it must use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]` and `tokio::process::Command`; blocking `std::process::Command::output()` on a `current_thread` runtime deadlocks the shared executor.

## Process Boundaries & Logging

- **Daemon in-process singleton**: `bpmute daemon` installs a global `tracing` subscriber on startup. Invoking `run_inproc_cli(&["bpmute", "daemon"], &env)` more than once in the same test process will panic because a global subscriber is already active. For daemon CLI tests (especially startup error checks), execute the built binary via `ProcessCommand::new(env!("CARGO_BIN_EXE_bpmute"))` (`run_real_bin`).

## Protocol & CLI Serde Conventions

- **`PatchField` serialization**: `PatchField::Unchanged` serializes to JSON `null`, but receiving `null` deserializes as `PatchField::Clear`. When constructing JSON payloads for partial mutation ops (like `session.set_feedback`), omit unchanged keys from the JSON map entirely.
- **`clap` `ValueEnum` naming**: Annotate enum types shared with serialized IPC or config fields with `#[value(rename_all = "snake_case")]` and provide kebab-case aliases (`alias = "..."`) so CLI flags and JSON/TOML payloads match. Pure CLI-only option enums (such as `--target graphical-session`) follow idiomatic `kebab-case` with optional `snake_case` aliases when specified by the CLI contract.

## D-Bus Hermeticism & Test Doubles

- **Host D-Bus isolation**: Test processes must never connect to the developer's live system or session D-Bus. `./scripts/test.sh` and `./scripts/check.sh` enforce `DBUS_SESSION_BUS_ADDRESS="unix:path=/dev/null"` and `DBUS_SYSTEM_BUS_ADDRESS="unix:path=/dev/null"`. Any test requiring OS or notification interactions must use in-memory doubles (`FakeActionExecutor`, `FakeNotifier`).
- **Fake binary scripts under stripped `PATH`**: Write fake executable test scripts with POSIX `#!/bin/sh` (`./scripts/check.sh` rejects `#!/usr/bin/env`), because hermetic `MapEnv` tests replace `PATH` with only the temporary script directory.
- **Daemon options in tests**: Use `DaemonOptions::for_testing(...)` rather than constructing raw struct literals. This defaults OS-touching handles (`action_executor`, `notifier`, `wearable_source`) safely to in-memory test doubles and insulates tests from future struct field additions.

## TUI Rendering & Interactive IPC

- **Populated-state rendering**: Assert `--ascii`, `NO_COLOR`, and `80×24` layout behavior on populated app state (enrolled wearables, live telemetry, alerts, tables/modals) rather than empty-state CTAs, which hide Unicode leaks and column clipping.
- **ASCII progress bars**: `ratatui::widgets::Gauge` renders Unicode `'█'` even when `use_unicode(false)` is set; render explicit ASCII progress bars (`[#---]`) whenever `theme.ascii` is enabled.
- **Non-blocking interactive IPC ops**: Operations that wait on follow-up requests over the same socket (such as `wearable.enroll_select` awaiting `wearable.enroll_auth_response`) must run as spawned tasks on the connection handler rather than blocking the socket read loop.

## Test Execution

- **Canonical runner**: Run iterative test subsets with `./scripts/test.sh <args>` (enforces the `timeout 30` bound and hermetic `DBUS_*` isolation) so hung tests fail fast without touching host D-Bus.
- **Integration-test trace diagnostics**: Enable tracing inside an integration test temporarily with `let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();` and run `RUST_LOG=bpmute=trace ./scripts/test.sh <args> -- --nocapture` instead of adding ad-hoc `eprintln!` instrumentation across source files.
- **Foreground diagnostics under desktop sessions**: Desktop environments (such as KDE Plasma Wayland) set `JOURNAL_STREAM`, causing `bpmute daemon` with default `--log-format auto` to log to `journald` instead of terminal stderr; pass `--log-format pretty --log-level bpmute=trace` when inspecting live terminal output.
