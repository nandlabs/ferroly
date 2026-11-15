#!/usr/bin/env bash
# HTTP/2 interop with Go's net/http client (interop/go-http, standard library
# only) against crates/ferroly/examples/h2spec_server.rs (cleartext, prior
# knowledge): many sequential and concurrent requests with 64 KiB answers on
# one connection, and concurrent uploads beyond the body budget. The client
# prints PASS/FAIL per check and exits non-zero on any failure.
#
# Run from anywhere; needs `cargo` and `go`. Port: GO_HTTP_PORT (18081).
set -euo pipefail
cd "$(dirname "$0")/.."
. interop/lib.sh

PORT="${GO_HTTP_PORT:-18081}"
work="$(mktemp -d)"
pid=""
cleanup() {
  if [ -n "$pid" ]; then kill "$pid" 2>/dev/null || true; fi
  rm -rf "$work"
}
trap cleanup EXIT

cargo build -p ferroly --features http2 --example h2spec_server
(cd interop/go-http && go build -o "$work/go-http" .)

"$(target_dir)/debug/examples/h2spec_server" "$PORT" 2>"$work/server.log" &
pid="$!"
wait_for_port "$PORT" "$pid"

status=0
"$work/go-http" "127.0.0.1:$PORT" || status=1
if [ "$status" -ne 0 ]; then
  echo "== server log"
  cat "$work/server.log"
fi
exit "$status"
