# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`GLOSSARY.md`** at the repo root, or
- **`GLOSSARY-MAP.md`** at the repo root if it exists: it points at one `GLOSSARY.md` per context. Read each one relevant to the topic.
- **`docs/adr/`**: read ADRs that touch the area you're about to work in. In multi-context repos, also check `src/<context>/docs/adr/` for context-scoped decisions.
  - `0001-rust-static-musl-direct-zbus-stack.md`
  - `0002-unix-socket-ndjson-daemon-ipc.md`
  - `0003-zero-polling-gatt-profile-and-adaptive-bonding.md`
  - `0004-forward-hmm-sleep-onset-engine.md`
  - `0005-recomputable-sqlite-calibration-ledger.md`
  - `0006-layered-idle-probe-and-unprivileged-logind-actions.md`
  - `0007-deterministic-clock-injected-engine-and-replay-harness.md`
  - `0008-local-privacy-rolling-retention-and-calibration-boundaries.md`
  - `0009-declarative-config-semantic-cli-and-biometric-redacted-logging.md`
- **`docs/research/`**: read only when exact numeric constants, protocol details, or empirical justification are needed:
  - `ble-hrs-bluez-dbus.md`
  - `linux-daemon-actions-stack.md`
  - `sleep-onset-hr-hrv-modeling.md`

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `GLOSSARY.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (event-sourced orders), but worth reopening because…_
