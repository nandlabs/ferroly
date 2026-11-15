#!/usr/bin/env bash
# gRPC interop with grpc-go, in both directions:
#   1. grpc-go client -> ferroly server (crates/ferroly/examples/grpc_interop.rs)
#   2. ferroly client -> grpc-go server (interop/grpc-go)
# Each client prints PASS/FAIL per check and exits non-zero on any failure.
#
# Run from anywhere; needs `cargo` and `go`. Ports: FERROLY_PORT (50051),
# GO_PORT (50052).
set -euo pipefail
cd "$(dirname "$0")/.."
. interop/lib.sh

FERROLY_PORT="${FERROLY_PORT:-50051}"
GO_PORT="${GO_PORT:-50052}"
work="$(mktemp -d)"
pids=()
cleanup() {
  for pid in ${pids[@]+"${pids[@]}"}; do kill "$pid" 2>/dev/null || true; done
  rm -rf "$work"
}
trap cleanup EXIT

cargo build -p ferroly --features grpc --example grpc_interop
ferroly="$(target_dir)/debug/examples/grpc_interop"
(cd interop/grpc-go && go build -o "$work/grpc-go" .)

status=0

echo "== grpc-go client -> ferroly server"
"$ferroly" server "$FERROLY_PORT" 2>"$work/ferroly-server.log" &
pids+=("$!")
wait_for_port "$FERROLY_PORT" "$!"
"$work/grpc-go" client "127.0.0.1:$FERROLY_PORT" || status=1

echo "== ferroly client -> grpc-go server"
"$work/grpc-go" server "$GO_PORT" 2>"$work/go-server.log" &
pids+=("$!")
wait_for_port "$GO_PORT" "$!"
"$ferroly" client "127.0.0.1:$GO_PORT" || status=1

if [ "$status" -ne 0 ]; then
  echo "== ferroly server log"
  cat "$work/ferroly-server.log"
  echo "== grpc-go server log"
  cat "$work/go-server.log"
fi
exit "$status"
