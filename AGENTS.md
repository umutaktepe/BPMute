## Language

Everything outside direct chat with the user must be written in English: all source code, comments, tests, documentation (`README.md`, `GLOSSARY.md`, `docs/adr/`, research notes), commit messages, GitHub issues/PRs, and user-facing application strings (TUI, CLI, notifications).

## Agent skills

### Issue tracker

Issues and specs are tracked in GitHub Issues (`umutaktepe/BPMute`) via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Domain docs

Single-context layout (`GLOSSARY.md` and `docs/adr/` at the repo root). See `docs/agents/domain.md`.

### Testing

Testing conventions, concurrency pitfalls, and virtual-time execution. See `docs/agents/testing.md`.

## Verification

Wrap iterative test runs in `timeout 30` (or `./scripts/test.sh`); run `./scripts/check.sh` and `/code-review` before finishing any change or closing an issue.
