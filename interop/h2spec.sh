#!/usr/bin/env bash
# Runs the h2spec HTTP/2 conformance suite against
# crates/ferroly/examples/h2spec_server.rs (cleartext, prior knowledge).
#
# Needs `cargo` and an `h2spec` binary: on PATH, or given as H2SPEC=/path.
# Extra arguments go to h2spec (e.g. `-j report.xml`, or a section such as
# `http2/6.5`). Port: H2SPEC_PORT (18080).
set -euo pipefail
cd "$(dirname "$0")/.."
. interop/lib.sh

H2SPEC="${H2SPEC:-h2spec}"
PORT="${H2SPEC_PORT:-18080}"

cargo build -p ferroly --features http2 --example h2spec_server
"$(target_dir)/debug/examples/h2spec_server" "$PORT" &
pid="$!"
trap 'kill "$pid" 2>/dev/null || true' EXIT
wait_for_port "$PORT" "$pid"

"$H2SPEC" -h 127.0.0.1 -p "$PORT" -o 5 "$@"
