#!/usr/bin/env bash
# Fails when `ferroly` or `ferroly-derive` gains a direct dependency outside the
# allowed set. Run from the repository root.
set -euo pipefail

check() {
  local pkg="$1" expected="$2" actual
  actual="$(cargo tree -p "$pkg" --all-features -e normal --depth 1 --prefix none --locked \
    | tail -n +2 | awk '{print $1}' | sort -u | tr '\n' ' ' | sed 's/ $//')"
  if [ "$actual" != "$expected" ]; then
    echo "::error::$pkg direct dependencies changed."
    echo "  expected: $expected"
    echo "  actual:   $actual"
    echo "Adding a dependency is a policy decision — see the README's design philosophy."
    exit 1
  fi
  echo "OK: $pkg -> $actual"
}

check ferroly        "ferroly-derive rustls rustls-pki-types tokio tokio-rustls webpki-roots"
check ferroly-derive "proc-macro2 quote syn"
