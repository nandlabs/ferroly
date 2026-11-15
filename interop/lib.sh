# Shared helpers for the interop scripts; sourced, not run.

# The cargo target directory (honours CARGO_TARGET_DIR).
target_dir() {
  echo "${CARGO_TARGET_DIR:-target}"
}

# wait_for_port <port> <pid>: waits up to 30 s for 127.0.0.1:<port> to accept
# connections, failing early if the process <pid> exits.
wait_for_port() {
  local port="$1" pid="$2" i
  for i in $(seq 1 150); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "the server (pid $pid) exited before listening on $port" >&2
      return 1
    fi
    sleep 0.2
  done
  echo "nothing listening on 127.0.0.1:$port after 30 s" >&2
  return 1
}
