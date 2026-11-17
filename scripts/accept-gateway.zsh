# Shared startup for the isolated acceptance scripts.
build_accept_gateway() {
  cargo build -p gateway --bin aubridged --quiet
  ACCEPT_BUILD_DIR=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
}

start_accept_gateway() {
  local accept_dir="$1"
  local port
  PLUGINS_DIR="$accept_dir/plugins" "$ACCEPT_BUILD_DIR/debug/aubridged" >"$accept_dir/gateway.log" 2>&1 &
  GW_PID=$!

  for _ in {1..100}; do
    if [[ -s "$accept_dir/config.json" ]] &&
       port=$(python3 -c 'import json,sys; print(int(json.load(open(sys.argv[1]))["port"]))' "$accept_dir/config.json" 2>/dev/null); then
      GW="http://127.0.0.1:$port"
      if curl -sf --noproxy '*' "$GW/healthz" >/dev/null; then
        return 0
      fi
    fi
    kill -0 "$GW_PID" 2>/dev/null || break
    sleep 0.1
  done
  echo "gateway 未就绪" >&2
  cat "$accept_dir/gateway.log" >&2
  return 1
}

cleanup_accept_gateway() {
  if [[ -n "${GW_PID:-}" ]]; then
    kill -INT "$GW_PID" 2>/dev/null || true
    wait "$GW_PID" 2>/dev/null || true
  fi
  rm -rf -- "$T"
}
