# Runtime Architecture & State Ownership

Authoritative state boundaries across TUI, daemon IPC, sleep-onset monitoring, and SQLite persistence.

## Actor & Ownership Map

```text
TuiApp (src/tui/app.rs)
  └─ TuiIpcBridge (src/tui/client.rs, client_commands.rs)
       └─ Unix NDJSON IPC (src/ipc/server.rs)
            └─ DaemonCore (src/daemon/state/mod.rs, ipc.rs, schedule.rs)
                 ├─ MonitoringContext (src/daemon/monitoring.rs) + SleepEngine (src/sleep_engine/state_machine/)
                 ├─ GraceCoordinator (src/actions/grace.rs)
                 └─ PersistenceWorker -> LedgerStore (src/storage/worker.rs, store/)
```

- **`MonitoringContext` owns live session execution**: holds the local `SleepEngine`, `active_session_id`, `active_wearable_id`, `wearable_cache`, and `pending_raw` packet buffer. Syncs `DaemonCore` via `sync_detector_and_deadline` and `set_session_close_deadline` on `SessionStarted`, `StateChanged`, `SessionEnded`, and gap deadline changes.
- **`DaemonCore` owns IPC snapshots, alerts, and broadcast channels**: serves `daemon.status` (`monitoring_state` is PascalCase `MonitoringState`), `calibration.status`, `wearable.list`, and `EventBroker` push streams; notifies `MonitoringContext` via `calibration_tx`, `config_tx`, and `engine_ctrl_tx` (`EngineControlEvent`).
- **`PersistenceWorker` / `LedgerStore` owns SQLite WAL durability**: runs on the dedicated `bpmute-ledger` OS thread under `flock`. Every discrete lifecycle, calibration, import, or storage-reclaiming mutation (`calibration.rs`, `maintenance.rs`, `session.rs`, `session_lifecycle.rs`, `wearable.rs`, `mod.rs`) and `PersistenceWorker` shutdown must flush/truncate WAL via `write_tx_and_flush` / `flush_wal` (or `VACUUM` + `wal_checkpoint(TRUNCATE)`), while only `15 s` streaming (`append_epoch_and_raw`) stays un-flushed under `WAL_RUNTIME_PRAGMAS` (`PRAGMA journal_size_limit = 1048576`). Because `raw_sample_chunks` omits per-packet `seq` inside `payload_blob`, every stored chunk must span a contiguous `[seq, seq + sample_count)` range (splitting on `RAW_CHUNK_MAX_SAMPLES`, `seq` gaps, and existing chunk boundaries during `Ledger Bundle` import/merge). `wearables` enforces `UNIQUE (mac, address_type)` with `ON CONFLICT(mac, address_type) DO UPDATE`, so bundle imports must resolve each wearable's `(mac, address_type)` to its persisted `wearables.id` and remap `sessions.wearable_id` on cross-machine merges.
- **Cross-actor resets**: Any IPC operation mutating or wiping persisted session/calibration state while monitoring may be active must abort active grace (`signal_arousal_abort`), synchronize `MonitoringContext` via `EngineControlEvent`, and propagate the SQLite-recomputed `CalibrationState` through `apply_calibration_update`.
- **Multi-channel event loop prioritization**: Actor loops consuming multiple channels (e.g., `run_monitoring_loop` with `shutdown_rx`, `ctrl_rx`, `cal_rx`, `cfg_rx`, and `wearable_rx`) must use `biased;` in `tokio::select!` with shutdown, control, calibration, and configuration branches prioritized ahead of high-throughput data streams (`wearable_rx`). This ensures that administrative commands, session resets, and live config/retention updates take immediate effect and are never starved or delayed behind incoming telemetry packets.

## TUI Submodule Layout

- **`src/tui/views/`**: Tab and chrome rendering (`dashboard.rs`, `history.rs`, `history_chart.rs`, `history_modals.rs`, `wearables.rs`, `settings.rs`, `footer.rs`, `wearables_modals.rs`).
- **`src/tui/input/`**: Keyboard and mouse event routers (`mod.rs`, `mouse.rs`).
- **`src/tui/app_ipc.rs`**: Incoming daemon IPC event handling and `TuiApp` state synchronization.
- **`src/tui/client_commands.rs`**: Outgoing `TuiCommand` translation into socket RPC requests.

## File Size & Submodule Boundaries

All Rust source files in `src/` must remain strictly under 1000 lines (enforced deterministically by `./scripts/check.sh`). When a module approaches 900+ lines, decompose its internal responsibilities into sibling submodules (for example, extracting IPC dispatch from `src/tui/app.rs` into `src/tui/app_ipc.rs`).

## Canonical Schemas & Contracts

- **IPC wire requests, responses, and push events**: See `src/ipc/protocol.rs` for authoritative schemas (`DaemonStatus`, `SessionListPage`, `SessionEpochsParams`, `MAX_IPC_EPOCHS_PER_FRAME`, `SnapshotEvent`, `EpochUpdateEvent`, `HrSampleEvent`) and `src/ipc/client.rs` (`EpochPaginator`) for transparent multi-page epoch assembly.
- **Persisted database models and descriptors**: See `src/storage/mod.rs` for entity structures (`WearableDescriptor`, `SessionSummary`, `SessionFullDetail`, `SessionEpochsPage`, `ImportSummary`).
- **Shared test fixtures and doubles**: See `tests/common/` (`common::daemon::TestWearableSource`, `common::tui::make_daemon_status`).
