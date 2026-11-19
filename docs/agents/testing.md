# Async & Integration Testing

Testing conventions, concurrency patterns, and pitfalls for BPMute daemons, worker threads, and CLI integration tests.

## Concurrency & Worker Threads

- **Explicit worker shutdown**: Any OS thread worker (`bpmute-ledger`) communicating via channels must use an explicit shutdown variant (e.g., `DbCommand::Shutdown`) and join its thread. Never rely on dropping sender handles for channel EOF when cloned handles (`PersistenceHandle`) or async tasks may still keep a sender alive.
- **Async handle shutdown**: Provide an async shutdown method (e.g., `shutdown_async`) that sends the shutdown command and awaits thread completion via `tokio::task::spawn_blocking`. Always implement `Drop` on the worker as a fallback.

## Tokio Virtual Time (`start_paused = true`)

- **OS thread I/O vs. virtual time**: In `#[tokio::test(start_paused = true)]`, do NOT wrap IPC or socket calls in `tokio::time::timeout(...)` when the daemon must communicate with an OS worker thread doing SQLite/disk I/O. Tokio will advance virtual time immediately past the timeout before the OS thread can finish. Call `client.next_event().await` without an outer timeout; Tokio will park the test task and wake it when the socket produces bytes.
- **Stream event synchronization**: When feeding simulated wearable streams into `FakeWearableSource`, wait for the expected stream events (e.g., `EpochUpdate`) to arrive before issuing live reconfiguration calls (such as `config.update`). Otherwise, `tokio::select!` in the monitoring loop can interleave the config change before the epoch features are processed and stored.

## Process Boundaries & Logging

- **Daemon in-process singleton**: `bpmute daemon` installs a global `tracing` subscriber on startup. Invoking `run_inproc_cli(&["bpmute", "daemon"], &env)` more than once in the same test process will panic because a global subscriber is already active. For daemon CLI tests (especially startup error checks), execute the built binary via `ProcessCommand::new(env!("CARGO_BIN_EXE_bpmute"))` (`run_real_bin`).

## Protocol & CLI Serde Conventions

- **`PatchField` serialization**: `PatchField::Unchanged` serializes to JSON `null`, but receiving `null` deserializes as `PatchField::Clear`. When constructing JSON payloads for partial mutation ops (like `session.set_feedback`), omit unchanged keys from the JSON map entirely.
- **`clap` `ValueEnum` naming**: Annotate enum types parsed by `clap` with `#[value(rename_all = "snake_case")]` and provide kebab-case aliases (`alias = "..."`) so that both CLI arguments (`--verdict missed-sleep` / `--verdict missed_sleep`) and IPC payloads parse consistently.

## Test Execution

- **Timeout bounds**: Wrap iterative test runs in a timeout (`timeout 30 cargo test ...` or `./scripts/test.sh ...`). If a test hangs, an unbounded run will wait indefinitely.
