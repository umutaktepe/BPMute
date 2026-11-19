---
status: accepted
---

# Local Privacy Isolation, Tiered Rolling Retention, and Reversible Calibration Boundaries

BPMute v1 stores all biometric history and calibration state locally in `$XDG_DATA_HOME/bpmute/bpmute.db` (SQLite WAL) using strict `0700`/`0600` XDG permissions, `PRAGMA secure_delete = ON`, and `PRAGMA auto_vacuum = INCREMENTAL` rather than database-level encryption (SQLCipher), relying on Linux full-disk encryption (LUKS) and BPMute's 100% network-free binary (ADR-0001). Storage follows a three-tier retention model (`14`-day default rolling TTL for `1 Hz` `raw_samples`, configurable to `0`/disabled, `N` days, or `unlimited`; indefinite default for `15 s` `session_epochs` with optional TTL; indefinite for `sessions` and `session_feedback`), assigns UUIDv7 identifiers to all ledger entities for idempotent cross-machine `Ledger Bundle` merges, and implements non-destructive re-baselining via reversible `Calibration Boundary` rows rather than mutating or deleting historical `Monitoring Sessions`.

## Considered Options

- **SQLCipher / keyring-unlocked database encryption:** Protects biometric HR rows at rest even on unencrypted filesystems, but introduces native OpenSSL/crypto C dependencies that break the pure-Rust static `musl` toolchain and requires interactive Secret Service / passphrase unlocking that fails during headless `systemd --user` startup.
- **Destructive calibration reset (clearing or deleting prior sessions):** Simplifies ledger replay by removing historical rows when resting HR shifts, but destroys user sleep history, makes accidental resets irreversible, and prevents seamlessly re-baselining from the start of a `5`-session persistent physiological shift without returning to `Cold Start`.
- **SQLite `INTEGER PRIMARY KEY AUTOINCREMENT` with replace-only restore:** Slightly smaller primary keys, but causes ID collisions when merging `Ledger Bundles` across multiple Linux machines (e.g., laptop and desktop) and forces all-or-nothing overwrites.

## Consequences

- `PRAGMA auto_vacuum = INCREMENTAL` and `PRAGMA secure_delete = ON` are enabled at database initialization so pruned or deleted biometric rows are zero-overwritten and reclaimed in bounded non-blocking steps at daemon startup and `Monitoring Session` close without stalling the BLE event loop.
- Deleting a single `Monitoring Session`, creating or undoing a `Calibration Boundary`, or merging an imported `Ledger Bundle` deterministically recomputes `calibration_state` from the ledger; a `Calibration Boundary` resets personal physiology to `Cold Start` (with an optional Day-1 sleeping-HR seed) while preserving per-Wearable signal floors unless explicitly cleared.
- When `bpmute daemon` is active, all ledger mutations route through the `0600` Unix socket IPC to keep in-memory and on-disk state strictly synchronized; offline CLI mutations acquire an exclusive `flock` on `$XDG_DATA_HOME/bpmute/bpmute.lock` before opening a SQLite transaction and write an automatic `0600` pre-replace/pre-migration backup before destructive schema or restore operations.
- All exported files (`Ledger Bundle` JSON, per-session canonical `CSV`/`JSONL` traces with optional `--anonymize`, and WAL-safe SQLite hot backups) are created with `0600` permissions, while default `INFO` logs and desktop notifications remain biometric-minimal (no `1 Hz` HR logging, MAC addresses redacted outside `DEBUG`/`TRACE`, and no raw BPM/history on lock-screen notifications).
