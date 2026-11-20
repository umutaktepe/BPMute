#!/usr/bin/env bash
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <binary_path>" >&2
  exit 2
fi

bin="$1"
if [ ! -f "$bin" ] || [ ! -x "$bin" ]; then
  echo "error: binary not found or not executable: $bin" >&2
  exit 1
fi

file_out="$(file -L "$bin")"
echo "file: $file_out"

if echo "$file_out" | grep -Eqi '(dynamically linked|interpreter)'; then
  echo "error: binary is dynamically linked or specifies a dynamic ELF interpreter" >&2
  exit 1
fi
if ! echo "$file_out" | grep -Eqi '(statically linked|static-pie linked)'; then
  echo "error: binary does not report static linkage in \`file\` output" >&2
  exit 1
fi

ldd_out="$(ldd "$bin" 2>&1 || true)"
echo "ldd: $ldd_out"

if echo "$ldd_out" | grep -Eq '(\.so|=>)'; then
  echo "error: dynamic shared-library dependencies detected by \`ldd\`" >&2
  exit 1
fi
if ! echo "$ldd_out" | grep -Eqi '(not a dynamic executable|statically linked|not a valid dynamic program)'; then
  echo "error: unexpected \`ldd\` output for static binary" >&2
  exit 1
fi

if command -v readelf >/dev/null 2>&1; then
  needed_out="$(readelf -d "$bin" 2>&1 || true)"
  if echo "$needed_out" | grep -q '(NEEDED)'; then
    echo "error: ELF DT_NEEDED entries detected by \`readelf -d\`:" >&2
    echo "$needed_out" >&2
    exit 1
  fi
  interp_out="$(readelf -l "$bin" 2>&1 || true)"
  if echo "$interp_out" | grep -qw 'INTERP'; then
    echo "error: ELF PT_INTERP program header detected by \`readelf -l\`" >&2
    exit 1
  fi
fi

echo "OK: verified truly static binary with zero dynamic .so dependencies: $bin"
