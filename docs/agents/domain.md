# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`GLOSSARY.md`** at the repo root for domain vocabulary.
- **`docs/adr/`**: list `docs/adr/*.md` and read ADRs touching the area you're about to work in (locate by numeric prefix with `docs/adr/00NN-*.md`, e.g., `docs/adr/0014-*.md` for `ADR-0014`, rather than guessing descriptive slugs).
- **`docs/research/`**: read only when exact numeric constants, protocol details, or empirical justification are needed:
  - `ble-hrs-bluez-dbus.md`
  - `linux-daemon-actions-stack.md`
  - `sleep-onset-hr-hrv-modeling.md`

## Use the glossary's vocabulary

When your output names a domain concept (in an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `GLOSSARY.md`. Don't drift to synonyms the glossary explicitly avoids; when an existing issue or spec uses a colloquial synonym (such as `archive` or `dump` for `Ledger Bundle`), normalize it to the `GLOSSARY.md` term in new code, comments, and test identifiers.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0007 (deterministic clock-injected replay), but worth reopening because…_
