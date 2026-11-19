#!/usr/bin/env bash
set -euo pipefail

cargo fmt --check
cargo clippy --all-targets -- -D warnings

if cargo tree -e normal --prefix none | grep -Eq '^(reqwest|hyper|rustls|native-tls|openssl|openssl-sys|ureq|curl|curl-sys|h2|tokio-rustls|webpki|rustls-webpki|libsystemd|libsystemd-sys|libdbus-sys|dbus|bluer|btleplug) '; then
  echo "error: forbidden outbound HTTP/TLS or C system dependency detected (ADR-0001)" >&2
  exit 1
fi

timeout 60 cargo test
