#!/usr/bin/env bash
#
# Chart render assertions: cases where `helm lint` passes but the rendered
# output is wrong. Run from anywhere:  bash deploy/helm/render-tests.sh
#
# The chart is otherwise only exercised by the kind smoke test in
# deploy-test.yml, which installs one values combination. These cover the
# combinations that install does not reach.
set -euo pipefail

CHART="$(cd "$(dirname "${BASH_SOURCE[0]}")/ourios" && pwd)"
failures=0

# Rendered OTEL_* env as "NAME=value" lines, deduplicated across the three
# workloads (they share ourios.commonEnv, so identical output is expected).
otel_env() {
  # Pair each `- name: OTEL_X` with the `value:` line that follows it. POSIX
  # sed only (no gawk 3-arg match, no BSD-hostile paste) so this runs the same
  # on a maintainer's macOS and on the Linux runner.
  helm template t "$CHART" "$@" \
    | sed -n '/^ *- name: OTEL_/{N;s/^ *- name: \(OTEL_[A-Z_]*\)\n *value: "\(.*\)"$/\1=\2/p;}' \
    | sort -u
}

expect() {
  local desc="$1" want="$2"; shift 2
  local got
  if ! got="$(otel_env "$@" 2>&1)"; then
    printf 'FAIL  %s\n      render error: %s\n' "$desc" "$got"
    failures=$((failures + 1))
    return
  fi
  if [[ "$got" == "$want" ]]; then
    printf 'ok    %s\n' "$desc"
  else
    printf 'FAIL  %s\n      want: %s\n      got:  %s\n' "$desc" "${want:-<empty>}" "${got:-<empty>}"
    failures=$((failures + 1))
  fi
}

# Defaults must add no OTEL_* env at all, so installing this chart version
# over an older one changes nothing unless the operator asks for it.
expect "defaults render no OTEL_* env" ""

# Regression: `helm upgrade --reuse-values` from a release predating the
# per-signal keys carries forward the OLD chart's `otel` map (endpoint only)
# without merging new defaults. Direct traversal nil-pointered here.
expect "upgrade path: per-signal maps absent" \
  'OTEL_EXPORTER_OTLP_ENDPOINT=http://old:4317' \
  --set otel.traces=null --set otel.metrics=null --set otel.logs=null \
  --set otel.exporterEndpoint=http://old:4317

expect "upgrade path: whole otel map absent" "" --set otel=null

# `default true X` would flip an explicit false back on; `dig` keys off
# existence, so it must not.
expect "explicit enabled=false is honoured" \
  'OTEL_LOGS_EXPORTER=none
OTEL_METRICS_EXPORTER=none
OTEL_TRACES_EXPORTER=none' \
  --set otel.traces.enabled=false \
  --set otel.metrics.enabled=false \
  --set otel.logs.enabled=false

expect "sampler and export interval map to standard names" \
  'OTEL_METRIC_EXPORT_INTERVAL=15000
OTEL_TRACES_SAMPLER=parentbased_traceidratio
OTEL_TRACES_SAMPLER_ARG=0.01' \
  --set otel.traces.sampler=parentbased_traceidratio \
  --set otel.traces.samplerArg=0.01 \
  --set otel.metrics.exportInterval=15000

# Go templates treat numeric 0 as empty, so a `with`-guarded field drops it.
# `samplerArg: 0` is meaningful — traceidratio 0 samples nothing the caller
# has not already sampled — so it has to survive.
expect "numeric zero samplerArg is not dropped" \
  'OTEL_TRACES_SAMPLER=traceidratio
OTEL_TRACES_SAMPLER_ARG=0' \
  --set otel.traces.sampler=traceidratio \
  --set otel.traces.samplerArg=0

if ((failures)); then
  printf '\n%d assertion(s) failed\n' "$failures" >&2
  exit 1
fi
printf '\nall render assertions passed\n'
