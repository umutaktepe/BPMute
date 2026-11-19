#!/usr/bin/env bash
set -euo pipefail

# Hermetic test isolation: isolate test processes from host desktop D-Bus
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=/dev/null}"
export DBUS_SYSTEM_BUS_ADDRESS="${DBUS_SYSTEM_BUS_ADDRESS:-unix:path=/dev/null}"

exec timeout 30 cargo test "$@"
