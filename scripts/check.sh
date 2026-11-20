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

if find src/sleep_engine src/calibration -name '*.rs' -print0 | xargs -0 awk '{ sub(/\/\/.*/, ""); if ($0 ~ /(^|[^[:alnum:]_])(tokio|std::fs|std::io|rusqlite|rand|rand_chacha)([^[:alnum:]_]|$)|Instant::now|SystemTime::now|Local::now|Utc::now/) { print FILENAME ":" FNR ":" $0; found=1 } } END { exit !found }'; then
  echo "error: forbidden async, wall-clock, I/O, SQLite, or RNG usage inside pure-sync core (ADR-0007)" >&2
  exit 1
fi

if grep -Eq 'DaemonOptions\s*\{' tests/*.rs; then
  echo "error: direct DaemonOptions struct literal forbidden in tests; use DaemonOptions::for_testing(...) (docs/agents/testing.md:27)" >&2
  exit 1
fi

while IFS= read -r -d '' file; do
  lines=$(wc -l < "$file")
  limit=1000
  case "$file" in
    ./src/storage/store/mod.rs) limit=1294 ;;
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
