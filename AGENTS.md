## Language

Everything outside direct chat with the user must be written in English: all source code, comments, tests, documentation (`README.md`, `GLOSSARY.md`, `docs/adr/`, research notes), commit messages, GitHub issues/PRs, and user-facing application strings (TUI, CLI, notifications).

## Agent skills

### Issue tracker

Read `docs/agents/issue-tracker.md` before running `gh` (enforces `GH_PAGER=cat` and `--json`). Issues and specs are tracked in GitHub Issues (`umutaktepe/BPMute`).

### Domain docs

Single-context layout (`GLOSSARY.md` and `docs/adr/` at the repo root). See `docs/agents/domain.md`.

### Testing

Testing conventions, concurrency pitfalls, and virtual-time execution. See `docs/agents/testing.md`.

### Architecture

Runtime actor ownership (`DaemonCore`, `MonitoringContext`, `GraceCoordinator`, `LedgerStore`) and cross-actor reset boundaries. See `docs/agents/architecture.md`.

## Verification

Run targeted iterative tests with `./scripts/test.sh --test <target> [filter]` (compiles first, then enforces 30 s execution timeout and hermetic D-Bus isolation), fast format/static/clippy checks with `./scripts/check.sh --lint` (or `--fix`), and `./scripts/check.sh` (or `./scripts/check.sh --staged` when unrelated dirty files remain in the working tree) before finishing any change.
