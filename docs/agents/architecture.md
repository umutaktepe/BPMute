# Runtime Architecture & State Ownership

Authoritative state boundaries across TUI, daemon IPC, sleep-onset monitoring, and SQLite persistence.

## Actor & Ownership Map

```text
TuiApp (src/tui/app.rs)
  └─ TuiIpcBridge (src/tui/client.rs, client_commands.rs)
       └─ Unix NDJSON IPC (src/ipc/server.rs)
            └─ DaemonCore (src/daemon/state/mod.rs, ipc.rs)
                 ├─ MonitoringContext + SleepEngine (src/daemon/mod.rs)
                 ├─ GraceCoordinator (src/actions/grace.rs)
                 └─ PersistenceWorker -> LedgerStore (src/storage/worker.rs, store/)
```

- **`MonitoringContext` owns live session execution**: holds the local `SleepEngine`, `active_session_id`, `active_wearable_id`, `wearable_cache`, and `pending_raw` packet buffer. Syncs `DaemonCore` via `set_detector_phase` on `SessionStarted`, `StateChanged`, and `SessionEnded`.
- **`DaemonCore` owns IPC snapshots, alerts, and broadcast channels**: serves `daemon.status` (`monitoring_state` is PascalCase `MonitoringState`), `calibration.status`, `wearable.list`, and `EventBroker` push streams; notifies `MonitoringContext` via `calibration_tx`, `config_tx`, and `engine_ctrl_tx` (`EngineControlEvent`).
- **`PersistenceWorker` / `LedgerStore` owns SQLite WAL durability**: runs on the dedicated `bpmute-ledger` OS thread under `flock`. Destructive or storage-reclaiming operations (`hard_wipe`, `purge_raw_samples`, `delete_session_and_recompute`, `unenroll_wearable` with `delete_sessions`) live in `src/storage/store/` (`maintenance.rs`, `session.rs`, `wearable.rs`) and must flush/truncate WAL (`flush_wal` / `PRAGMA wal_checkpoint(TRUNCATE)`) after `VACUUM` or `PRAGMA incremental_vacuum`.
- **Cross-actor resets**: Any IPC operation mutating or wiping persisted session/calibration state while monitoring may be active must abort active grace (`signal_arousal_abort`), synchronize `MonitoringContext` via `EngineControlEvent`, and propagate the SQLite-recomputed `CalibrationState` through `apply_calibration_update`.
