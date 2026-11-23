# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`GLOSSARY.md`** at the repo root for domain vocabulary.
- **`docs/adr/`**: read ADRs that touch the area you're about to work in:
  - `0001-rust-static-musl-direct-zbus-stack.md`
  - `0002-unix-socket-ndjson-daemon-ipc.md`
  - `0003-zero-polling-gatt-profile-and-adaptive-bonding.md`
  - `0004-forward-hmm-sleep-onset-engine.md`
  - `0005-recomputable-sqlite-calibration-ledger.md`
  - `0006-layered-idle-probe-and-unprivileged-logind-actions.md`
  - `0007-deterministic-clock-injected-engine-and-replay-harness.md`
  - `0008-local-privacy-rolling-retention-and-calibration-boundaries.md`
  - `0009-declarative-config-semantic-cli-and-biometric-redacted-logging.md`
  - `0010-render-emitted-tui-hit-map.md`
  - `0011-grace-period-activity-hold-window.md`
  - `0012-disconnect-session-closure-and-manual-end.md`
  - `0013-state-gated-active-hours-passive-standby-and-pre-candidate-baseline.md`
- **`docs/research/`**: read only when exact numeric constants, protocol details, or empirical justification are needed:
  - `ble-hrs-bluez-dbus.md`
  - `linux-daemon-actions-stack.md`
  - `sleep-onset-hr-hrv-modeling.md`

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `GLOSSARY.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (deterministic clock-injected replay), but worth reopening because…_
