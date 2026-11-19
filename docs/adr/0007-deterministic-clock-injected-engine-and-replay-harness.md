---
status: accepted
---

# Pure-Sync Clock-Injected Sleep Engine with Dev-Only Synthetic Harness and Read-Only CLI Replay

BPMute v1 isolates the entire 4-stage PPG filter, `15 s` Epoch aggregator, 3-state Forward-Filtered HMM, deterministic sleep-onset state machine, and calibration-ledger recomputation inside a pure-synchronous, zero-`tokio`, zero-I/O Rust core driven strictly by explicit timestamped input events (`HrPacket`, `StreamState`, `DesktopActivity`, `GraceOutcome`), while wrapping OS edges (`WearableSource`, `IdleProbe`, `ActionExecutor`, `Notifier`) behind narrow async traits tested in `tokio::time::pause()` virtual time. The release `bpmute` binary ships only a strictly read-only, dual-fidelity `bpmute replay` command and zero-side-effect dry-run trigger verification, whereas the parametric synthetic HR/RR generator, seeded Monte Carlo sweeps, and multi-night calibration-ledger fixtures live exclusively in the dev/CI workspace test harness.

## Considered Options

- **Tokio-coupled sleep engine with wall-clock `Instant::now()` and inline SQLite queries:** Marginally reduces adapter boilerplate in the daemon, but forces slow or flaky time-mocking for multi-hour sleep/gap scenarios and prevents exact bit-for-bit replay of recorded sessions through the production code path.
- **Bundling the synthetic HR/RR simulator inside the release `bpmute` binary:** Enables users to run live fake-Wearable sessions directly against the TUI, but bloats the static `musl` binary with RNG/physiological-synthesis code that most users never invoke when read-only `bpmute replay` and `--dry-run` already cover troubleshooting and safe `Grace Period` verification.
- **Committing public polysomnography/wearable datasets (PhysioNet, Walch) into the repository and CI:** Adds real-world physiological recordings directly to CI, but introduces dataset licensing/redistribution overhead and multi-hundred-megabyte repository bloat compared to a deterministic self-contained synthetic corpus paired with optional local offline adapters and tiny anonymized real-capture fixtures.

## Consequences

- Production daemon execution, `cargo test` regression suites, and `bpmute replay` execute the exact same pure-Rust state machine and calibration math with zero clock drift or environment nondeterminism.
- `bpmute replay` transparently replays stored `Monitoring Sessions` from `1 Hz` `raw_samples` (full filter + HMM pipeline) when retained or falls back to permanent `15 s` `session_epochs` (post-filter HMM + state machine) with an explicit fidelity indicator, and accepts standalone `CSV`/`JSONL` traces without ever mutating SQLite or `calibration_state`.
- Dry-run trigger verification exercises the real desktop notification (`[Dry Run]`) and active idle probe during the `Grace Period`, while suppressing all `Sleep Action Pipeline` side effects and recording zero calibration feedback.
- CI stays fast (`< 5 s` deterministic Tier 1 suite) and self-contained, while heavier `~200`-night seeded Monte Carlo phenotype sweeps run as a separate Tier 2 benchmark job (`#[ignore]`) with quantitative specificity, recall, latency, and quarantine-stability gates.
