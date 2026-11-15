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

# `log` is optional and enabled only by the opt-in `log-bridge` feature (it
# appears here because the check runs with `--all-features`). It must stay
# dependency-free: the transitive check below fails if it ever pulls a crate.
check ferroly        "ferroly-derive log rustls rustls-pki-types tokio tokio-rustls webpki-roots"
check ferroly-derive "proc-macro2 quote syn"

# The `log` facade crate must not bring anything with it (with the features
# ferroly enables, it has no dependencies of its own).
log_deps="$(cargo tree -p ferroly --all-features -e normal --prefix depth --locked \
  | awk '/^1log / { inside = 1; next } /^1/ { inside = 0 } inside { sub(/^[0-9]+/, ""); print $1 }' \
  | sort -u | tr '\n' ' ' | sed 's/ $//')"
if [ -n "$log_deps" ]; then
  echo "::error::the optional \`log\` dependency (feature log-bridge) gained dependencies: $log_deps"
  exit 1
fi
echo "OK: log -> (no dependencies)"
