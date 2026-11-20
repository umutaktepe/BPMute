#!/usr/bin/env bash
set -euo pipefail

# Hermetic test isolation: isolate test processes from host desktop D-Bus
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/dev/null}"
export DBUS_SYSTEM_BUS_ADDRESS="${DBUS_SYSTEM_BUS_ADDRESS:-unix:path=/dev/null}"

cargo fmt --check
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

if grep -Eq 'DaemonOptions\s*\{' tests/*.rs; then
  echo "error: direct DaemonOptions struct literal forbidden in tests; use DaemonOptions::for_testing(...) (docs/agents/testing.md:27)" >&2
  exit 1
fi

if grep -En '#!/usr/bin/env[[:space:]]' tests/*.rs; then
  echo "error: #!/usr/bin/env shebang forbidden in fake test scripts (stripped MapEnv PATH omits /usr/bin); use POSIX #!/bin/sh" >&2
  exit 1
fi

if grep -En 'let[[:space:]]+_[[:space:]]*=[[:space:]]*(adapter|device|mgr|proxy)\.' src/bluetooth/*.rs; then
  echo "error: silent let _ = on BlueZ D-Bus proxy calls is forbidden in src/bluetooth/; log or propagate errors" >&2
  exit 1
fi

if ! grep -Eq 'parse_hr_measurement\(' src/bluetooth/service.rs; then
  echo "error: parse_hr_measurement must be wired into production src/bluetooth/service.rs" >&2
  exit 1
fi

if ! grep -Eq '"WearablesUpdated"' src/daemon/state/mod.rs || ! grep -Eq '"WearablesUpdated"' src/tui/client.rs; then
  echo "error: WearablesUpdated IPC control event must stay wired between daemon and TUI" >&2
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

while IFS= read -r -d '' file; do
  lines=$(wc -l < "$file")
  limit=1000
  case "$file" in
    ./src/storage/store/mod.rs) limit=1061 ;;
    ./src/sleep_engine/state_machine.rs) limit=1041 ;;
  esac
  if [ "$lines" -gt "$limit" ]; then
    echo "error: $file has $lines lines (limit: $limit); split into sibling submodules" >&2
    exit 1
  fi
done < <(find ./src -name '*.rs' -print0)

if awk '
  FNR == 1 { in_test = 0; paused = 0 }
  /^[[:space:]]*#\[tokio::test/ {
    in_test = 1
    paused = ($0 ~ /start_paused[[:space:]]*=[[:space:]]*true/)
  }
  /^\}/ { in_test = 0; paused = 0 }
  /tokio::time::pause\(\)/ { paused = 1 }
  {
    line = $0
    sub(/\/\/.*/, "", line)
    if (in_test && !paused && line ~ /tokio::time::sleep/) {
      print FILENAME ":" FNR ":" $0
      found = 1
    }
  }
  END { exit !found }
' tests/*.rs; then
  echo "error: tokio::time::sleep inside unpaused #[tokio::test]; use #[tokio::test(start_paused = true)] or event-driven awaits (docs/agents/testing.md)" >&2
  exit 1
fi

timeout 60 cargo test
timeout 5 cargo test --test sleep_engine
timeout 5 cargo test --test calibration_ledger
