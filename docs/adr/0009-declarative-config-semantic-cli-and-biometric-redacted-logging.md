---
status: accepted
---

# Declarative-Only TOML Configuration, Semantic CLI Exit Codes, and Biometric-Redacted Logging

BPMute v1 keeps `$XDG_CONFIG_HOME/bpmute/config.toml` (`schema_version = 1`, `deny_unknown_fields`, `0600`) strictly limited to declarative user preferences—leaving enrolled `Wearables` and Day-1 sleeping-HR seeds solely in SQLite—reloads only via IPC or `SIGHUP`, standardizes semantic CLI exit codes (`0`–`5`), and writes structured daemon logs via pure-Rust `tracing-journald` (auto-detected from `JOURNAL_STREAM`) with biometric telemetry and full MAC addresses redacted even at `DEBUG`/`TRACE` unless explicitly unlocked via `--log-biometrics` / `BPMUTE_LOG_BIOMETRICS=1`.

## Considered Options

- **Mirroring enrolled `Wearables` or Day-1 sleeping-HR seeds in `config.toml`:** Lets users hand-edit device lists or initial calibration seeds in TOML, but splits state across TOML and SQLite, creating drift during `Ledger Bundle` imports, `Calibration Boundary` recomputation, and TUI edits.
- **Warn-and-ignore unknown TOML keys with an `inotify` live file watcher:** Tolerates version downgrades and editor saves without explicit reload, but silently hides typos in safety-critical keys (`power_action`, `grace_period_sec`, `ignore_players`) and introduces partial-write races and extra watcher dependencies.
- **Linking `libsystemd` for journald logging or unlocking raw BPM/RR automatically at `DEBUG`/`TRACE`:** Linking `libsystemd` breaks the pure-Rust static `musl` binary guarantee (ADR-0001), while unlocking raw HR/MAC logging automatically at `DEBUG` risks accidentally persisting sensitive biometric streams into `journald` whenever a user debugs BlueZ or Wayland idle issues.
- **Generic POSIX exit codes (`0`/`1`/`2`):** Simpler to document, but forces Waybar/Polybar scripts and shell hooks to parse stderr to distinguish an offline daemon (`3`) or an inactive `Grace Period` (`4`) from genuine configuration or runtime failures.

## Consequences

- `config.toml` is not auto-created on first daemon startup; BPMute runs on built-in defaults and writes `config.toml` (`0600`) lazily on first TUI `Settings` edit (via `toml_edit`) or explicit `bpmute config init`.
- Invalid configuration or invalid `BPMUTE_LOG` `EnvFilter` expressions fail fast (`exit 2`) at startup, while `config.reload` / `SIGHUP` rejects invalid edits and preserves the last valid in-memory configuration.
- `bpmute.service` includes `ExecReload=/bin/kill -HUP $MAINPID`, and precedence follows `CLI flags > BPMUTE_* env vars > config.toml > defaults` with global `--config`, `--data-dir`, and `--socket` overrides.
- Enabling `--log-biometrics` / `BPMUTE_LOG_BIOMETRICS=1` emits a prominent startup `WARN` banner so accidental persistent biometric logging in `systemd` units is immediately surfaced.
