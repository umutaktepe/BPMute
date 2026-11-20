## Language

Everything outside direct chat with the user must be written in English: all source code, comments, tests, documentation (`README.md`, `GLOSSARY.md`, `docs/adr/`, research notes), commit messages, GitHub issues/PRs, and user-facing application strings (TUI, CLI, notifications).

## Agent skills

### Issue tracker

Issues and specs are tracked in GitHub Issues (`umutaktepe/BPMute`) via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context layout (`GLOSSARY.md` and `docs/adr/` at the repo root). See `docs/agents/domain.md`.

### Testing

Testing conventions, concurrency pitfalls, and virtual-time execution. See `docs/agents/testing.md`.

### Architecture

Runtime actor ownership (`DaemonCore`, `MonitoringContext`, `GraceCoordinator`, `LedgerStore`) and cross-actor reset boundaries. See `docs/agents/architecture.md`.

## Verification

Run iterative tests with `./scripts/test.sh <args>` (enforces 30 s timeout and hermetic D-Bus isolation); run `./scripts/check.sh` before finishing any change.
