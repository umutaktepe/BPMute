#!/usr/bin/env bash
set -euo pipefail

# Hermetic test isolation: isolate test processes from host desktop D-Bus, Wayland, and X11
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/dev/null}"
export DBUS_SYSTEM_BUS_ADDRESS="${DBUS_SYSTEM_BUS_ADDRESS:-unix:path=/dev/null}"
unset WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY

build_args=()
for arg in "$@"; do
  if [ "$arg" = "--" ]; then
    break
  fi
  build_args+=("$arg")
done

timeout_sec="${BPMUTE_TEST_TIMEOUT:-30}"
if ! [[ "$timeout_sec" =~ ^[1-9][0-9]*$ ]]; then
  echo "error: BPMUTE_TEST_TIMEOUT must be a positive integer (got: '$timeout_sec')" >&2
  exit 2
fi

# Pre-compile before arming the hang-detection timeout so build time never triggers false 124s.
cargo test --no-run "${build_args[@]}"

exec timeout "$timeout_sec" cargo test "$@"
