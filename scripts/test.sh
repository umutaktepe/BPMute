#!/usr/bin/env bash
set -euo pipefail

# Hermetic test isolation: isolate test processes from host desktop D-Bus
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/dev/null}"
export DBUS_SYSTEM_BUS_ADDRESS="${DBUS_SYSTEM_BUS_ADDRESS:-unix:path=/dev/null}"

build_args=()
for arg in "$@"; do
  if [ "$arg" = "--" ]; then
    break
  fi
  build_args+=("$arg")
done

# Pre-compile before arming the 30 s hang-detection timeout so build time never triggers false 124s.
cargo test --no-run "${build_args[@]}"

exec timeout 30 cargo test "$@"
