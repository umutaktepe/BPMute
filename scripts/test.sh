#!/usr/bin/env bash
set -euo pipefail
exec timeout 30 cargo test "$@"
