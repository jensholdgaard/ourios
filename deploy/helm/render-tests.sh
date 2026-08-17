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

# Rendered env of one workload, as "NAME=value" lines in DOCUMENT ORDER
# (order is part of the contract: k8s resolves duplicate env names to the
# last entry, which is how a role-level extraEnv overrides a global one).
# POSIX sed only, so this runs the same on a maintainer's macOS and on the
# Linux runner.
workload_env() {
  local template="$1"; shift
  helm template t "$CHART" --show-only "templates/$template" "$@" \
    | sed -n '/^ *- name: /{N;s/^ *- name: \([A-Z0-9_]*\)\n *value: "\{0,1\}\([^"]*\)"\{0,1\}$/\1=\2/p;}'
}

# Rendered envFrom source names of one workload, in document order.
workload_envfrom() {
  local template="$1"; shift
  helm template t "$CHART" --show-only "templates/$template" "$@" \
    | sed -n '/^ *envFrom:/,/^ *[a-z]*:$/p' \
    | sed -n 's/^ *name: "\{0,1\}\([^"]*\)"\{0,1\}$/\1/p'
}

check() {
  local desc="$1" want="$2" got="$3"
  if [[ "$got" == "$want" ]]; then
    printf 'ok    %s\n' "$desc"
  else
    printf 'FAIL  %s\n      want: %s\n      got:  %s\n' \
      "$desc" "$(printf '%s' "$want" | tr '\n' '|')" "$(printf '%s' "$got" | tr '\n' '|')"
    failures=$((failures + 1))
  fi
}

# --- OTEL_* env -------------------------------------------------------------

# Defaults must add no OTEL_* env at all: the SDK's behaviour is configured
# through its own OTEL_* variables via extraEnv, never through chart defaults.
check "defaults render no OTEL_* env" "" \
  "$(workload_env receiver-statefulset.yaml | grep '^OTEL_' || true)"

# The endpoint is the one OTel knob the chart models (deployment topology).
check "exporterEndpoint renders" \
  "OTEL_EXPORTER_OTLP_ENDPOINT=http://collector:4317" \
  "$(workload_env querier-deployment.yaml --set otel.exporterEndpoint=http://collector:4317 | grep '^OTEL_')"

# Regression (#644): `helm upgrade --reuse-values` from an older release can
# carry an `otel` shape without current keys — or no `otel` at all. Renders
# must not nil-pointer.
check "upgrade path: whole otel map absent" "" \
  "$(workload_env receiver-statefulset.yaml --set otel=null | grep '^OTEL_' || true)"

# --- extraEnv passthrough ---------------------------------------------------

check "global extraEnv passes through verbatim" \
  "OTEL_TRACES_SAMPLER=parentbased_traceidratio
OTEL_TRACES_SAMPLER_ARG=0.01" \
  "$(workload_env compactor-deployment.yaml \
      --set 'extraEnv[0].name=OTEL_TRACES_SAMPLER' \
      --set 'extraEnv[0].value=parentbased_traceidratio' \
      --set 'extraEnv[1].name=OTEL_TRACES_SAMPLER_ARG' \
      --set-string 'extraEnv[1].value=0.01' | grep '^OTEL_')"

# A role's extraEnv lands only on that role's workload.
check "role extraEnv is scoped to its workload (present on querier)" \
  "OTEL_RESOURCE_ATTRIBUTES=service.namespace=query" \
  "$(workload_env querier-deployment.yaml \
      --set 'querier.extraEnv[0].name=OTEL_RESOURCE_ATTRIBUTES' \
      --set 'querier.extraEnv[0].value=service.namespace=query' | grep '^OTEL_')"
check "role extraEnv is scoped to its workload (absent on receiver)" "" \
  "$(workload_env receiver-statefulset.yaml \
      --set 'querier.extraEnv[0].name=OTEL_RESOURCE_ATTRIBUTES' \
      --set 'querier.extraEnv[0].value=service.namespace=query' | grep '^OTEL_' || true)"

# Two roles setting DIFFERENT values for the SAME key must not interfere,
# and a role entry must render AFTER the global one (k8s: last entry wins),
# so a role-level value overrides a global default for the same name.
dup_args=(
  --set 'extraEnv[0].name=OTEL_RESOURCE_ATTRIBUTES'
  --set 'extraEnv[0].value=deployment.environment.name=dev'
  --set 'receiver.extraEnv[0].name=OTEL_RESOURCE_ATTRIBUTES'
  --set 'receiver.extraEnv[0].value=deployment.environment.name=ingest'
  --set 'querier.extraEnv[0].name=OTEL_RESOURCE_ATTRIBUTES'
  --set 'querier.extraEnv[0].value=deployment.environment.name=query'
)
check "duplicate key: receiver's own value renders after the global" \
  "OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=dev
OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=ingest" \
  "$(workload_env receiver-statefulset.yaml "${dup_args[@]}" | grep '^OTEL_')"
check "duplicate key: querier's own value renders after the global" \
  "OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=dev
OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=query" \
  "$(workload_env querier-deployment.yaml "${dup_args[@]}" | grep '^OTEL_')"
check "duplicate key: compactor gets only the global" \
  "OTEL_RESOURCE_ATTRIBUTES=deployment.environment.name=dev" \
  "$(workload_env compactor-deployment.yaml "${dup_args[@]}" | grep '^OTEL_')"

# The s3 credentials envFrom is deliberately outside this feature's scope —
# assert the per-role env work left it exactly as it was.
check "s3 existingSecret envFrom is untouched" \
  "s3-creds" \
  "$(workload_envfrom receiver-statefulset.yaml \
      --set storage.backend=s3 --set storage.s3.bucket=b \
      --set storage.s3.existingSecret=s3-creds \
      --set 'receiver.extraEnv[0].name=OTEL_RESOURCE_ATTRIBUTES' \
      --set 'receiver.extraEnv[0].value=x=y')"

# --- receiver.tenant passthrough (RFC 0045) ---------------------------------

# The receiver's rendered config document (the ConfigMap's `receiver.yaml`).
receiver_config() {
  helm template t "$CHART" --show-only templates/configmap.yaml "$@" \
    | sed -n '/^  receiver.yaml: |/,/^  [a-z]*.yaml: |/p'
}

check "defaults render no receiver.tenant block" "" \
  "$(receiver_config | grep '^      tenant:' || true)"
check "receiver.tenant renders verbatim under receiver" \
  "      tenant:
        rule:
        - k8s.cluster.name
        - service.name
        watch:
        - cloud.region" \
  "$(receiver_config \
      --set 'receiver.tenant.rule[0]=k8s.cluster.name' \
      --set 'receiver.tenant.rule[1]=service.name' \
      --set 'receiver.tenant.watch[0]=cloud.region' \
      | sed -n '/^      tenant:/,/^    [a-z_]*:$/p' | sed '$d')"

if ((failures)); then
  printf '\n%d assertion(s) failed\n' "$failures" >&2
  exit 1
fi
printf '\nall render assertions passed\n'
