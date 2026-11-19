# Coding Standards

Review-stage standards for BPMute. Mechanical checks (`rustfmt`, `clippy`, `unsafe_code`, `await_holding_*`, ADR-0001 banned dependencies, and 60 s bounded test runs) live in `./scripts/check.sh`, `clippy.toml`, and `Cargo.toml`.

## Judgement Rules

- **Biometric redaction completeness**: Every `tracing` backend (including `tracing-journald`) and both span attributes/records and events must pass through the shared redaction path so raw BPM/RR telemetry and full MAC addresses never appear unless `--log-biometrics` / `BPMUTE_LOG_BIOMETRICS=1` is active. Redaction helpers (`mask_mac`, text scrubbing) must be idempotent so nested layers never corrupt already-masked values.
- **Boundary trait preservation**: Injected environment, clock, and OS-edge boundaries (`EnvSource`, `DaemonClock`, and future `WearableSource` / `IdleProbe` / `ActionExecutor` / `Notifier` traits) must be propagated through spawned tasks and live-reload paths (`SIGHUP`, `config.reload`). Never fall back to concrete `ProcessEnv` or wall-clock globals inside injected paths.
- **Semantic CLI exit codes (`0`–`5`)**: Map `PermissionDenied` and OS/D-Bus backend refusals consistently to `ExitCode::PermissionOrBackendRefusal` (`5`), offline daemon/unreachable socket to `3`, CLI/config/input validation errors to `2`, precondition/state conflicts to `4`, and generic runtime errors to `1`.
- **Runtime state vs. config reloads**: Unrelated `config.update` or `config.reload` operations must not clobber explicit runtime overrides (such as `monitor.set_dry_run`) unless the reloaded setting itself changed.
- **Domain vocabulary (`GLOSSARY.md`)**: Use `Wearable`, `Enrollment`, `Signal Lost`, `Epoch`, `Quiet-Wake Baseline`, `Calibration Maturity`, `Monitoring Session`, `Anomalous Session`, `Grace Period`, `Sleep Action Pipeline`, `Snooze`, `Calibration Boundary`, and `Ledger Bundle` across code, comments, tests, and user-facing strings; keep BlueZ wire identifiers (`Device1`) confined to the BlueZ proxy layer.
