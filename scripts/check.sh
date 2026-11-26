#!/usr/bin/env bash
set -euo pipefail

# Hermetic test isolation: isolate test processes from host desktop D-Bus, Wayland, and X11
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/dev/null}"
export DBUS_SYSTEM_BUS_ADDRESS="${DBUS_SYSTEM_BUS_ADDRESS:-unix:path=/dev/null}"
unset WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY

fix_fmt=0
lint_only=0
staged_only=0
for arg in "$@"; do
  case "$arg" in
    --fix) fix_fmt=1 ;;
    --lint) lint_only=1 ;;
    --staged) staged_only=1 ;;
    *)
      echo "usage: $0 [--fix] [--lint] [--staged]" >&2
      exit 2
      ;;
  esac
done

if [ "$fix_fmt" -eq 1 ] && [ "$staged_only" -eq 1 ]; then
  echo "error: --fix cannot be combined with --staged" >&2
  exit 2
fi

if [ "$staged_only" -eq 1 ]; then
  repo_root="$(git rev-parse --show-toplevel)"
  tmp_dir="$(mktemp -d)"
  trap 'rm -rf "$tmp_dir"' EXIT
  git -C "$repo_root" checkout-index -a --prefix="$tmp_dir/"
  cd "$tmp_dir"
  export CARGO_TARGET_DIR="$repo_root/target/staged"
fi

if [ "$fix_fmt" -eq 1 ]; then
  cargo fmt
fi

cargo fmt --check

while IFS= read -r -d '' file; do
  lines=$(wc -l < "$file")
  limit=1000
  if [ "$lines" -gt "$limit" ]; then
    echo "error: $file has $lines lines (limit: $limit); split into sibling submodules" >&2
    exit 1
  elif [ "$lines" -gt 900 ]; then
    echo "warning: $file has $lines lines (>900); decompose into sibling submodules before hitting $limit" >&2
  fi
done < <(find ./src -name '*.rs' -print0)

if grep -Eq 'DaemonOptions\s*\{' tests/*.rs; then
  echo "error: direct DaemonOptions struct literal forbidden in tests; use DaemonOptions::for_testing(...) (docs/agents/testing.md)" >&2
  exit 1
fi

if grep -Eq 'DaemonStatus\s*\{' tests/*.rs; then
  echo "error: direct DaemonStatus struct literal forbidden in tests; use tests/common/tui.rs::make_daemon_status(...) (docs/agents/testing.md)" >&2
  exit 1
fi

if grep -Eq 'SessionListEntry\s*\{' tests/*.rs; then
  echo "error: direct SessionListEntry struct literal forbidden in tests; use tests/common/tui.rs::make_history_session(...) (docs/agents/testing.md)" >&2
  exit 1
fi

if awk '/SessionSummary\s*\{/ && !/->/ && !/fn / { print FILENAME ":" FNR ":" $0; found=1 } END { exit !found }' tests/*.rs; then
  echo "error: direct SessionSummary struct literal forbidden in tests; use tests/common/tui.rs::make_session_summary(...) (docs/agents/testing.md)" >&2
  exit 1
fi

if grep -En '#!/usr/bin/env[[:space:]]' tests/*.rs; then
  echo "error: #!/usr/bin/env shebang forbidden in fake test scripts (stripped MapEnv PATH omits /usr/bin); use POSIX #!/bin/sh" >&2
  exit 1
fi

if grep -REn '^[[:space:]]*(let[[:space:]]+_[[:space:]]*=[[:space:]]*)?(std::)?thread::spawn[[:space:]]*\(' src tests; then
  echo "error: unjoined thread::spawn with discarded JoinHandle is forbidden; bind the handle and join on explicit shutdown (docs/agents/testing.md)" >&2
  exit 1
fi

if grep -En 'Command::new\("dbus-daemon"\)' tests/*.rs; then
  echo "error: raw Command::new(\"dbus-daemon\") forbidden in tests/*.rs; reuse tests/common/dbus.rs (docs/agents/testing.md)" >&2
  exit 1
fi

if grep -En 'let[[:space:]]+_[[:space:]]*=[[:space:]]*(adapter|device|mgr|proxy)\.' src/bluetooth/*.rs; then
  echo "error: silent let _ = on BlueZ D-Bus proxy calls is forbidden in src/bluetooth/; log or propagate errors" >&2
  exit 1
fi

if grep -REn 'let[[:space:]]+_[[:space:]]*=[[:space:]]*[[:alnum:]_]*(proxy|conn|manager)\.' src/actions/ src/idle/ || grep -REn 'let[[:space:]]+Ok\([^)]+\)[[:space:]]*=.*(Proxy::|session_bus\(|session_conn\(|receive_).*else[[:space:]]*\{[[:space:]]*return' src/actions/ src/idle/; then
  echo "error: silent D-Bus error discarding is forbidden in src/actions/ and src/idle/; log or propagate errors" >&2
  exit 1
fi

if grep -REn '\.receive_[a-z0-9_]+_changed\(' src/ | grep -Ev '\.receive_(properties|owner)_changed\('; then
  echo "error: zbus PropertyStream (.receive_*_changed()) is forbidden in src/ due to initial cached-value/coalescing races; subscribe via PropertiesProxy::receive_properties_changed() + CacheProperties::No before reading initial state" >&2
  exit 1
fi

if ! grep -REq 'parse_hr_measurement\(' src/bluetooth; then
  echo "error: parse_hr_measurement must be wired into production src/bluetooth/" >&2
  exit 1
fi

python3 - <<'PY'
import glob, re, sys

daemon_events = set()
for path in sorted(glob.glob("src/daemon/**/*.rs", recursive=True)):
    stripped = "\n".join(
        l.split("//", 1)[0] for l in open(path, encoding="utf-8").read().splitlines()
    )
    for m in re.finditer(r'\bpublish_(?:control|stream)\(\s*"([^"]+)"', stripped):
        daemon_events.add(m.group(1))

tui_client = "\n".join(
    l.split("//", 1)[0] for l in open("src/tui/client.rs", encoding="utf-8").read().splitlines()
)
tui_events = set(re.findall(r'^\s*"([A-Z][A-Za-z0-9]+)"\s*=>', tui_client, flags=re.M))
allowlist = set()
missing_tui = sorted(daemon_events - tui_events - allowlist)
if missing_tui:
    sys.exit(
        f"error: daemon push event(s) {missing_tui} missing match arm in src/tui/client.rs"
    )

ipc_mod = "\n".join(
    l.split("//", 1)[0] for l in open("src/ipc/mod.rs", encoding="utf-8").read().splitlines()
)
other_ipc_consumers = {}
for path in sorted(glob.glob("src/**/*.rs", recursive=True) + glob.glob("tests/**/*.rs", recursive=True)):
    if path.startswith("src/ipc/"):
        continue
    other_ipc_consumers[path] = open(path, encoding="utf-8").read()

for m in re.finditer(r'(?m)^pub use\s+([^;]+);', ipc_mod):
    stmt = m.group(1)
    inner = stmt.split("{", 1)[1].rsplit("}", 1)[0] if "{" in stmt else stmt.rsplit("::", 1)[-1]
    syms = [s.strip().split(" as ")[-1] for s in inner.split(",") if s.strip()]
    for sym in syms:
        if sym.endswith("Params") or sym.endswith("Response"):
            sys.exit(
                f"error: request/response DTO {sym} must stay in src/ipc/protocol.rs rather than re-exporting from src/ipc/mod.rs (CODING_STANDARDS.md)"
            )
        pat = re.compile(rf'(?:crate|bpmute)::ipc::(?:[^{{;]*\{{[^}}]*\b{re.escape(sym)}\b|\b{re.escape(sym)}\b)')
        if not any(pat.search(txt) for txt in other_ipc_consumers.values()):
            sys.exit(
                f"error: unused crate-root re-export {sym} in src/ipc/mod.rs; remove or import via ipc::protocol (CODING_STANDARDS.md)"
            )
PY

python3 - <<'PY'
import glob, re, sys

proto_lines = open("src/ipc/protocol.rs", encoding="utf-8").read().splitlines()
for idx, line in enumerate(proto_lines):
    if "deny_unknown_fields" in line:
        window = "\n".join(proto_lines[max(0, idx - 3) : idx + 2])
        if "derive(" not in window or "Serialize" not in window:
            sys.exit(
                f"error: #[serde(deny_unknown_fields)] type at src/ipc/protocol.rs:{idx + 1} must also derive Serialize"
            )

files = sorted(set(glob.glob("src/tui/**/*.rs", recursive=True) + glob.glob("src/cli/commands/*.rs")))
for path in files:
    stripped = "\n".join(
        l.split("//", 1)[0] for l in open(path, encoding="utf-8").read().splitlines()
    )
    if re.search(r'json!\(\s*\{[^}]*"', stripped):
        sys.exit(
            f"error: keyed serde_json::json!({{ ... }}) literal forbidden in {path}; use typed structs from src/ipc/protocol.rs"
        )
    if re.search(r'\["[a-z0-9_]+"\]', stripped):
        sys.exit(
            f"error: raw JSON string-key indexing forbidden in {path}; deserialize typed response structs from src/ipc/protocol.rs"
        )

tui_client = "\n".join(
    l.split("//", 1)[0] for l in open("src/tui/client.rs", encoding="utf-8").read().splitlines()
)
if re.search(r'push\.data\.get\(\s*"', tui_client):
    sys.exit(
        "error: raw push.data.get(...) forbidden in src/tui/client.rs; deserialize typed event structs from src/ipc/protocol.rs"
    )

for path in sorted(glob.glob("src/daemon/state/**/*.rs", recursive=True)):
    stripped = "\n".join(
        l.split("//", 1)[0] for l in open(path, encoding="utf-8").read().splitlines()
    )
    if re.search(
        r'\b(?:make_event|publish_control|publish_stream)\(\s*"[^"]+"\s*,\s*json!\(\s*\{',
        stripped,
        flags=re.S,
    ):
        sys.exit(
            f"error: untyped json!({{ ... }}) push event payload forbidden in {path}; serialize a typed struct from src/ipc/protocol.rs"
        )

for path in sorted(glob.glob("src/storage/**/*.rs", recursive=True)):
    content = open(path, encoding="utf-8").read()
    has_vacuum = bool(re.search(r'\bVACUUM;|PRAGMA\s+incremental_vacuum\b', content))
    if has_vacuum and not re.search(r'wal_checkpoint\(TRUNCATE\)|flush_wal\(|write_tx_and_flush\(|vacuum_and_truncate_wal\(', content):
        sys.exit(
            f"error: {path} runs VACUUM or PRAGMA incremental_vacuum without wal_checkpoint(TRUNCATE) / flush_wal() / write_tx_and_flush()"
        )
    if "VACUUM;" in content and "PRAGMA auto_vacuum" in content:
        if content.index("VACUUM;") < content.index("PRAGMA auto_vacuum"):
            sys.exit(
                f"error: {path} sets PRAGMA auto_vacuum after VACUUM;; set it before VACUUM"
            )
    for fn_block in re.split(r'(?=^\s*(?:pub(?:\([^)]*\))?\s+)?fn\s+)', content, flags=re.M)[1:]:
        fn_sig = fn_block.splitlines()[0].strip()
        if fn_sig.startswith("fn migrate_") and ("VACUUM;" in fn_block or "vacuum_and_truncate_wal(" in fn_block):
            if not re.search(r'WAL_RUNTIME_PRAGMAS|PRAGMA\s+journal_mode\s*=\s*WAL', fn_block):
                sys.exit(
                    f"error: {path} ({fn_sig}) runs VACUUM / vacuum_and_truncate_wal without switching to WAL mode first"
                )
        if not fn_sig.startswith("pub fn "):
            continue
        needs_flush = bool(re.search(r'\bVACUUM;|PRAGMA\s+incremental_vacuum\b|recompute_and_persist_in_tx\(', fn_block))
        if needs_flush and not re.search(r'wal_checkpoint\(TRUNCATE\)|flush_wal\(|write_tx_and_flush\(|vacuum_and_truncate_wal\(', fn_block):
            sys.exit(
                f"error: {path} ({fn_sig}) mutates/recomputes ledger without wal_checkpoint(TRUNCATE) / flush_wal() / write_tx_and_flush()"
            )

ipc_server = "\n".join(
    l.split("//", 1)[0] for l in open("src/ipc/server.rs", encoding="utf-8").read().splitlines()
)
if re.search(r'\bwrite_ndjson_line\(', ipc_server):
    sys.exit(
        "error: unbounded write_ndjson_line(...) forbidden in src/ipc/server.rs; use write_bounded_ndjson_line(..., MAX_FRAME_BYTES)"
    )

for path in sorted(glob.glob("src/**/*.rs", recursive=True)):
    stripped = "\n".join(
        l.split("//", 1)[0] for l in open(path, encoding="utf-8").read().splitlines()
    )
    if re.search(r'\.timestamp_millis_opt\([^)]*\)\s*\.single\s*\(', stripped, flags=re.S):
        sys.exit(
            f"error: .timestamp_millis_opt(...).single() forbidden in {path}; use .earliest() or match LocalResult so DST fall-back hours never drop timestamps"
        )

for path in sorted(glob.glob("src/daemon/**/*.rs", recursive=True)):
    stripped = "\n".join(
        l.split("//", 1)[0] for l in open(path, encoding="utf-8").read().splitlines()
    )
    if re.search(
        r'\bpersistence\.(?:get_session|close_session_and_recompute|set_feedback_and_recompute)\(',
        stripped,
    ):
        sys.exit(
            f"error: unbounded persistence session detail call forbidden in {path}; use get_session_with_epoch_limit / get_session_epochs / *_slim_and_recompute (ADR-0002)"
        )
PY

# shellcheck disable=SC2016
if find src tests -name '*.rs' -print0 | xargs -0 awk '
  FNR == 1 { in_test = 0; paused = 0; test_indent = 0 }
  /^[[:space:]]*#\[tokio::test/ {
    match($0, /^[[:space:]]*/)
    test_indent = RLENGTH
    in_test = 1
    paused = ($0 ~ /start_paused[[:space:]]*=[[:space:]]*true/)
  }
  in_test && $0 ~ ("^ {" test_indent "}\\}$") { in_test = 0; paused = 0 }
  /tokio::time::pause\(\)/ { paused = 1 }
  {
    line = $0
    sub(/\/\/.*/, "", line)
    if (in_test && !paused && line ~ /tokio::time::(sleep|timeout|interval)/) {
      print FILENAME ":" FNR ":" $0
      found = 1
    }
  }
  END { exit !found }
'; then
  echo "error: tokio::time::{sleep,timeout,interval} inside unpaused #[tokio::test]; use #[tokio::test(start_paused = true)] or event-driven awaits (docs/agents/testing.md)" >&2
  exit 1
fi

if python3 - <<'PY'
import glob, re, sys

bad = []
for path in sorted(glob.glob("tests/**/*.rs", recursive=True)):
    text = open(path, encoding="utf-8").read()
    if "multi_thread" not in text:
        continue
    in_paused_test = False
    test_indent = 0
    for idx, raw in enumerate(text.splitlines(), 1):
        m = re.match(r"^(\s*)#\[tokio::test", raw)
        if m:
            in_paused_test = "start_paused" in raw and "true" in raw
            test_indent = len(m.group(1))
        elif in_paused_test and re.match(rf"^ {{{test_indent}}}\}}$", raw):
            in_paused_test = False
        elif in_paused_test and "tokio::time::pause()" in raw:
            in_paused_test = True
        code = raw.split("//", 1)[0]
        if not in_paused_test and re.search(r"\btokio::time::sleep\(", code):
            bad.append(f"{path}:{idx}: {raw.strip()}")

if bad:
    print("\n".join(bad), file=sys.stderr)
    sys.exit(1)
PY
then
  :
else
  echo "error: real-time tokio::time::sleep(...) in multi_thread test helpers is forbidden; synchronize via watch/oneshot channels (docs/agents/testing.md)" >&2
  exit 1
fi

for script in scripts/*.sh .githooks/*; do
  [ -f "$script" ] || continue
  if [ ! -x "$script" ]; then
    echo "error: script is not executable: $script" >&2
    exit 1
  fi
  bash -n "$script"
  if command -v shellcheck >/dev/null 2>&1; then
    shellcheck "$script"
  fi
done

python3 - <<'PY'
import glob, sys
try:
    import yaml
except ImportError:
    yaml = None

if yaml is not None:
    for path in sorted(glob.glob(".github/**/*.yml", recursive=True) + glob.glob(".github/**/*.yaml", recursive=True)):
        with open(path, encoding="utf-8") as f:
            data = yaml.safe_load(f)
        if path == ".github/build-setup.yml" and not isinstance(data, list):
            sys.exit(f"error: {path} must be a top-level YAML sequence of steps for cargo-dist")
else:
    with open(".github/build-setup.yml", encoding="utf-8") as f:
        for line in f:
            stripped = line.strip()
            if stripped and not stripped.startswith("#"):
                if not stripped.startswith("- "):
                    sys.exit("error: .github/build-setup.yml must be a top-level YAML sequence of steps for cargo-dist")
                break
PY
if command -v actionlint >/dev/null 2>&1; then
  actionlint
fi
if command -v dist >/dev/null 2>&1; then
  dist plan --output-format=json >/dev/null
  dist generate --check
fi

cargo clippy --all-targets -- -D warnings

if cargo tree -e normal --prefix none | grep -Eq '^(reqwest|hyper|rustls|native-tls|openssl|openssl-sys|ureq|curl|curl-sys|h2|tokio-rustls|webpki|rustls-webpki|libsystemd|libsystemd-sys|libdbus-sys|dbus|bluer|btleplug) '; then
  echo "error: forbidden outbound HTTP/TLS or C system dependency detected (ADR-0001)" >&2
  exit 1
fi

if cargo tree --depth 1 -e normal --prefix none | grep -Eq '^(rand|rand_chacha|rand_core) '; then
  echo "error: dev-only synthetic harness dependency leaked into normal dependencies (ADR-0007)" >&2
  exit 1
fi

mapfile -d '' sync_files < <(find src/sleep_engine src/calibration -name '*.rs' -print0)
if [ "${#sync_files[@]}" -gt 0 ] && awk '{ sub(/\/\/.*/, ""); if ($0 ~ /(^|[^[:alnum:]_])(tokio|std::fs|std::io|rusqlite|rand|rand_chacha)([^[:alnum:]_]|$)|Instant::now|SystemTime::now|Local::now|Utc::now/) { print FILENAME ":" FNR ":" $0; found=1 } } END { exit !found }' "${sync_files[@]}"; then
  echo "error: forbidden async, wall-clock, I/O, SQLite, or RNG usage inside pure-sync core (ADR-0007)" >&2
  exit 1
fi

if [ "$lint_only" -eq 1 ]; then
  exit 0
fi

cargo test --no-run
timeout 60 cargo test
timeout 5 cargo test --test sleep_engine
timeout 5 cargo test --test calibration_ledger
