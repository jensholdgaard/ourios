#!/usr/bin/env bash
# Start or stop the local closed loop (`just closed-loop-up` / `closed-loop-down`):
# the Collector and the local Ourios run detached, with state under
# scratch/closed-loop/. `replay` (or `up` with REPLAY=true) also starts the
# corpus replay (dev/observability/corpus-replay.py) against the Collector. Each service's state file records its PID and the
# process start time, so a PID the OS has reused for another process is never
# treated as ours or signalled.
set -euo pipefail

dir=scratch/closed-loop
ready_timeout=120

started_at() { ps -o lstart= -p "$1" 2>/dev/null | sed 's/  */ /g; s/^ //; s/ $//'; }

# Prints the recorded PID if it is still the process that was launched.
live_pid() {
  local state="$dir/$1.state" pid start
  [ -f "$state" ] || return 1
  pid="$(sed -n 1p "$state")"
  start="$(sed -n 2p "$state")"
  if [ -n "$pid" ] && [ "$(started_at "$pid")" = "$start" ]; then
    echo "$pid"
  else
    rm -f "$state"
    return 1
  fi
}

collector_config=dev/observability/closed-loop-collector.yaml
config_digest() { shasum -a 256 "$collector_config" | cut -d' ' -f1; }

# The state file holds the PID, its start time, and (for the Collector) the
# digest of the config it was started with.
record() { printf '%s\n%s\n%s\n' "$2" "$(started_at "$2")" "${3:-}" > "$dir/$1.state"; }

# True when `pid` itself is listening on `port`, not merely something.
listening_as() { lsof -nP -a -p "$1" -iTCP:"$2" -sTCP:LISTEN >/dev/null 2>&1; }

# Waits until `pid` listens on `port`; fails if it exits or times out.
await_ready() {
  local name=$1 pid=$2 port=$3
  for _ in $(seq 1 "$ready_timeout"); do
    listening_as "$pid" "$port" && return 0
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "$name exited during startup; see $dir/$name.log" >&2
      rm -f "$dir/$name.state"
      return 1
    fi
    sleep 1
  done
  echo "$name not listening on $port after ${ready_timeout}s; stopping it (see $dir/$name.log)" >&2
  stop "$name" >&2 || true
  return 1
}

stop() {
  local name=$1 pid
  if ! pid="$(live_pid "$name")"; then
    echo "$name: not running"
    return 0
  fi
  kill "$pid"
  for _ in $(seq 1 40); do kill -0 "$pid" 2>/dev/null || break; sleep 0.25; done
  if kill -0 "$pid" 2>/dev/null; then
    echo "$name (pid $pid) still running after 10s" >&2
    return 1
  fi
  rm -f "$dir/$name.state"
  echo "$name stopped (pid $pid)"
}

up() {
  command -v lsof >/dev/null || { echo "lsof is required for the readiness check; install it first" >&2; exit 1; }
  mkdir -p "$dir" scratch/dogfood/store scratch/dogfood/wal
  local collector_bin ourios_bin target_dir pid
  collector_bin="$(command -v otelcol-contrib || echo scratch/bin/otelcol-contrib)"
  [ -x "$collector_bin" ] || { echo "otelcol-contrib not found; run 'just closed-loop-install'" >&2; exit 1; }
  set -a; [ -f .env ] && . ./.env; set +a
  : "${CLOSED_LOOP_OTLP_ENDPOINT:?set CLOSED_LOOP_OTLP_ENDPOINT (your OTLP/HTTP backend) in .env}"
  export CLOSED_LOOP_OTLP_AUTHORIZATION="${CLOSED_LOOP_OTLP_AUTHORIZATION:-}"

  # Build first, so a failed build leaves nothing running. The binary path comes
  # from Cargo, which honours CARGO_TARGET_DIR and .cargo/config.
  cargo build -q -p ourios-server
  target_dir="$(cargo metadata --format-version 1 --no-deps \
    | python3 -I -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
  ourios_bin="$target_dir/debug/ourios-server"

  # A running Collector started from an older config (say, before a span
  # scrubber was added) is restarted, so a config change is never silently
  # left unapplied.
  if pid="$(live_pid collector)" && [ "$(sed -n 3p "$dir/collector.state")" != "$(config_digest)" ]; then
    echo "collector config changed; restarting it"
    stop collector
  fi
  if pid="$(live_pid collector)"; then
    echo "collector already running (pid $pid)"
  else
    # Start from a snapshot, so the recorded digest is of exactly the config
    # the Collector runs even if the file is edited during startup.
    cp "$collector_config" "$dir/collector.yaml"
    nohup "$collector_bin" --config "$dir/collector.yaml" \
      > "$dir/collector.log" 2>&1 < /dev/null &
    pid=$!
    record collector "$pid" "$(shasum -a 256 "$dir/collector.yaml" | cut -d' ' -f1)"
    await_ready collector "$pid" 4318
    echo "collector ready (pid $pid), log $dir/collector.log"
  fi

  if pid="$(live_pid ourios)"; then
    echo "ourios already running (pid $pid)"
  else
    # OTLP/HTTP on 4318 with every exporter setting pinned, as closed-loop-ourios.
    OURIOS_DOGFOOD_ROOT="$(pwd)/scratch/dogfood" \
    OTEL_LOGS_EXPORTER=otlp OTEL_TRACES_EXPORTER=otlp OTEL_METRICS_EXPORTER=otlp \
    OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318 \
    OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
    OTEL_EXPORTER_OTLP_LOGS_PROTOCOL=http/protobuf \
    OTEL_EXPORTER_OTLP_TRACES_PROTOCOL=http/protobuf \
    OTEL_EXPORTER_OTLP_METRICS_PROTOCOL=http/protobuf \
    OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=http://127.0.0.1:4318/v1/logs \
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://127.0.0.1:4318/v1/traces \
    OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=http://127.0.0.1:4318/v1/metrics \
    OTEL_SERVICE_NAME=ourios-server \
    OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=development \
      nohup "$ourios_bin" --config dogfood-config.yaml \
      > "$dir/ourios.log" 2>&1 < /dev/null &
    pid=$!
    record ourios "$pid"
    await_ready ourios "$pid" 24318
    echo "ourios ready (pid $pid), log $dir/ourios.log"
  fi
}

# The replay sends to the Collector, so it needs the Collector up but not Ourios:
# the Collector buffers and retries the export until Ourios is listening.
replay() {
  local pid
  if pid="$(live_pid replay)"; then
    echo "replay already running (pid $pid)"
    return 0
  fi
  live_pid collector >/dev/null || { echo "collector is not running; run 'just closed-loop-up' first" >&2; exit 1; }
  # A Collector started from an older config may lack the corpus-replay route
  # and would send the replay into the claude-code tenant.
  if [ "$(sed -n 3p "$dir/collector.state")" != "$(config_digest)" ]; then
    echo "collector runs an older config; run 'just closed-loop-up' to apply the new config" >&2
    exit 1
  fi
  mkdir -p "$dir"
  nohup python3 -I dev/observability/corpus-replay.py \
    --endpoint "${REPLAY_ENDPOINT:-http://127.0.0.1:4318/v1/logs}" \
    --rate "${REPLAY_RATE:-3}" --mutation-rate "${REPLAY_MUTATION_RATE:-0.1}" \
    > "$dir/replay.log" 2>&1 < /dev/null &
  pid=$!
  record replay "$pid"
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    echo "replay exited during startup; see $dir/replay.log" >&2
    rm -f "$dir/replay.state"
    return 1
  fi
  echo "replay running (pid $pid), log $dir/replay.log"
}

# The replay first, then Ourios, so Ourios's last telemetry still reaches the Collector.
down() { stop replay; stop ourios; stop collector; }

case "${1:-}" in
  up) up; if [ "${REPLAY:-false}" = true ]; then replay; fi ;;
  replay) replay ;;
  down) down ;;
  *) echo "usage: $0 up|replay|down" >&2; exit 2 ;;
esac
