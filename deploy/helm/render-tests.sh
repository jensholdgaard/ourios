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

# --- RFC 0059 template-id bootstrap ------------------------------------------

# The chart runs the binary with --config, which reads no bare env var, so
# the authorisation must reach the receiver's rendered config.
receiver_config() {
  helm template t "$CHART" --show-only templates/configmap.yaml "$@" \
    | sed -n '/receiver:/,/compaction:/p'
}
check "bootstrap authorisation is off by default" "" \
  "$(receiver_config | grep 'template_ids_allow_bootstrap' || true)"
check "templateIdsAllowBootstrap renders into the receiver config" \
  "template_ids_allow_bootstrap: true" \
  "$(receiver_config --set receiver.templateIdsAllowBootstrap=true \
      | grep -o 'template_ids_allow_bootstrap: true')"

# --- default render is pinned -------------------------------------------------

# The security surface is opt-in: a default install must render exactly what
# it rendered before it existed. The chart/app version labels move with every
# release, so they are dropped before the comparison. A deliberate change to
# the default render regenerates the golden file:
#   UPDATE_GOLDEN=1 bash deploy/helm/render-tests.sh
GOLDEN="$(dirname "${BASH_SOURCE[0]}")/testdata/default-render.yaml"
default_render() {
  helm template t "$CHART" | grep -v -e 'helm.sh/chart:' -e 'app.kubernetes.io/version:'
}
if [[ -n "${UPDATE_GOLDEN:-}" ]]; then
  default_render > "$GOLDEN"
fi
if default_render | diff -u "$GOLDEN" - >/dev/null; then
  printf 'ok    %s\n' "default render matches testdata/default-render.yaml"
else
  printf 'FAIL  default render differs from testdata/default-render.yaml:\n'
  default_render | diff -u "$GOLDEN" - | sed 's/^/      /' | head -40
  failures=$((failures + 1))
fi

# --- auth, TLS, MCP, OpenFGA -------------------------------------------------

# A render that must fail, and the substring its error must carry.
check_fails() {
  local desc="$1" want="$2"; shift 2
  local out
  if out="$(helm template t "$CHART" "$@" 2>&1)"; then
    check "$desc" "render fails" "render succeeded"
  elif [[ "$out" == *"$want"* ]]; then
    check "$desc" "x" "x"
  else
    check "$desc" "error containing: $want" "$out"
  fi
}

# One role's rendered config file (the ConfigMap key), parsed out of the
# block scalar by its 4-space indent.
role_config() {
  local role="$1"; shift
  helm template t "$CHART" --show-only templates/configmap.yaml "$@" \
    | sed -n "/^  $role.yaml: |/,/^  [a-z]*\.yaml: |/p" \
    | sed -n 's/^    //p'
}

# Sentinel strings standing in for secret material. The chart never sees a
# secret value, only the Secret's name and key, so none of these may reach a
# rendered ConfigMap.
secure_args=(
  --set 'auth.tokens[0].name=edge'
  --set 'auth.tokens[0].tenants[0]=checkout'
  --set 'auth.tokens[0].tenants[1]=payments'
  --set 'auth.tokens[0].secretKeyRef.name=tokens-SENTINEL-SECRET-NAME'
  --set 'auth.tokens[0].secretKeyRef.key=edge-SENTINEL-SECRET-KEY'
  --set 'auth.tokens[1].name=ops'
  --set 'auth.tokens[1].tenants[0]=*'
  --set 'auth.tokens[1].secretKeyRef.name=tokens-SENTINEL-SECRET-NAME'
  --set 'auth.tokens[1].secretKeyRef.key=ops-SENTINEL-SECRET-KEY'
  --set auth.oidc.enabled=true
  --set auth.oidc.issuer=https://dex.example.com
  --set auth.oidc.audience=ourios
  --set auth.oidc.groupsClaim=groups
  --set auth.openfga.enabled=true
  --set auth.openfga.apiUrl=http://openfga:8080
  --set auth.openfga.storeId=01STORE
  --set auth.openfga.apiToken.secretKeyRef.name=fga-SENTINEL-SECRET-NAME
  --set auth.openfga.apiToken.secretKeyRef.key=fga-SENTINEL-SECRET-KEY
  --set receiver.tls.grpc.existingSecret=rx-tls
  --set receiver.tls.grpc.clientCA.existingSecret=rx-ca
  --set-string receiver.tls.grpc.minVersion=1.3
  --set receiver.tls.grpc.reloadIntervalSecs=60
  --set receiver.tls.http.existingSecret=rx-tls
  --set querier.tls.http.existingSecret=q-tls
  --set querier.tls.http.clientCA.existingSecret=q-tls
  --set querier.mcp.enabled=true
)

check "auth on: tokens are \${env:} references bound to their tenants" \
  'tokens:
- name: "edge"
token: "${env:OURIOS_AUTH_TOKEN_0}"
tenants:
- "checkout"
- "payments"
- name: "ops"
token: "${env:OURIOS_AUTH_TOKEN_1}"
tenants:
- "*"' \
  "$(role_config receiver "${secure_args[@]}" \
      | sed -n '/^  tokens:/,/^  oidc:/p' | sed '$d' | sed 's/^ *//')"
check "auth on: oidc and openfga render, tenant_claim optional under openfga" \
  'oidc:
issuer: "https://dex.example.com"
audience: "ourios"
groups_claim: "groups"
openfga:
api_url: "http://openfga:8080"
store_id: "01STORE"
api_token: "${env:OURIOS_OPENFGA_API_TOKEN}"' \
  "$(role_config querier "${secure_args[@]}" \
      | sed -n '/^  oidc:/,/^querier:/p' | sed '$d' | sed 's/^ *//')"
check "auth on: the compactor config carries no auth section" "" \
  "$(role_config compactor "${secure_args[@]}" | grep '^auth:' || true)"
check "auth on: token env comes from secretKeyRef, receiver" \
  "OURIOS_AUTH_TOKEN_0 tokens-SENTINEL-SECRET-NAME/edge-SENTINEL-SECRET-KEY
OURIOS_AUTH_TOKEN_1 tokens-SENTINEL-SECRET-NAME/ops-SENTINEL-SECRET-KEY
OURIOS_OPENFGA_API_TOKEN fga-SENTINEL-SECRET-NAME/fga-SENTINEL-SECRET-KEY" \
  "$(helm template t "$CHART" --show-only templates/receiver-statefulset.yaml "${secure_args[@]}" \
      | sed -n '/^ *- name: OURIOS_/{N;N;N;N;s/^ *- name: \([A-Z0-9_]*\)\n *valueFrom:\n *secretKeyRef:\n *name: "\([^"]*\)"\n *key: "\([^"]*\)"$/\1 \2\/\3/p;}')"
check "auth on: the querier gets the same secret env" "3" \
  "$(helm template t "$CHART" --show-only templates/querier-deployment.yaml "${secure_args[@]}" \
      | grep -c 'name: OURIOS_\(AUTH_TOKEN_[0-9]\|OPENFGA_API_TOKEN\)$')"
check "auth on: the compactor gets no secret env" "0" \
  "$(helm template t "$CHART" --show-only templates/compactor-deployment.yaml "${secure_args[@]}" \
      | grep -c 'OURIOS_AUTH_TOKEN\|OURIOS_OPENFGA' || true)"

# The security posture: no secret material in any ConfigMap. Every
# token-bearing leaf is an ${env:} reference, the Secret coordinates (the
# sentinels) stay in the pod spec, and the chart renders no Secret object.
all_configmaps="$(helm template t "$CHART" "${secure_args[@]}" \
  | awk '/^---/{cm=0} /^kind: ConfigMap/{cm=1} cm')"
check "no secret coordinates in any rendered ConfigMap" "" \
  "$(printf '%s' "$all_configmaps" | grep 'SENTINEL' || true)"
check "every token leaf in the ConfigMap is an \${env:} reference" "" \
  "$(printf '%s' "$all_configmaps" | grep -E '^ *(token|api_token):' \
      | grep -v -E ': "\$\{env:[A-Z0-9_]+\}"$' || true)"
check "the chart renders no Secret object" "" \
  "$(helm template t "$CHART" "${secure_args[@]}" | grep '^kind: Secret' || true)"
check_fails "an inline token value is refused" "auth.tokens[0].token is not accepted" \
  --set 'auth.tokens[0].name=edge' --set 'auth.tokens[0].tenants[0]=a' \
  --set 'auth.tokens[0].token=SENTINEL-INLINE-TOKEN'
check "the refused inline token never reaches the output" "" \
  "$(helm template t "$CHART" --set 'auth.tokens[0].name=edge' \
      --set 'auth.tokens[0].tenants[0]=a' \
      --set 'auth.tokens[0].token=SENTINEL-INLINE-TOKEN' 2>&1 \
      | grep 'SENTINEL-INLINE-TOKEN' || true)"
check_fails "a token without a secretKeyRef is refused" "secretKeyRef.name and .key are required" \
  --set 'auth.tokens[0].name=edge' --set 'auth.tokens[0].tenants[0]=a'
check_fails "a token without tenants is refused" "must list at least one tenant" \
  --set 'auth.tokens[0].name=edge' --set 'auth.tokens[0].secretKeyRef.name=s' \
  --set 'auth.tokens[0].secretKeyRef.key=k'
check_fails "openfga alone is refused (it authenticates nothing)" "auth.openfga.enabled needs" \
  --set auth.openfga.enabled=true --set auth.openfga.apiUrl=http://f --set auth.openfga.storeId=s
check_fails "oidc without openfga requires tenantClaim" "auth.oidc.tenantClaim is required" \
  --set auth.oidc.enabled=true --set auth.oidc.issuer=https://i --set auth.oidc.audience=a

check "TLS + mTLS on: listener blocks point at the Secret mounts" \
  'grpc_tls:
cert_file: "/etc/ourios-tls/grpc/tls.crt"
key_file: "/etc/ourios-tls/grpc/tls.key"
client_ca_file: "/etc/ourios-tls/grpc-client-ca/ca.crt"
min_version: "1.3"
reload_interval_secs: 60
http_tls:
cert_file: "/etc/ourios-tls/http/tls.crt"
key_file: "/etc/ourios-tls/http/tls.key"' \
  "$(role_config receiver "${secure_args[@]}" \
      | sed -n '/^  grpc_tls:/,/^compaction:/p' | sed '$d' | sed 's/^ *//')"
check "TLS + mTLS on: querier http_tls with a client CA" \
  'http_tls:
cert_file: "/etc/ourios-tls/http/tls.crt"
key_file: "/etc/ourios-tls/http/tls.key"
client_ca_file: "/etc/ourios-tls/http-client-ca/ca.crt"' \
  "$(role_config querier "${secure_args[@]}" \
      | sed -n '/^  http_tls:/,/^compaction:/p' | sed '$d' | sed 's/^ *//')"
check "TLS on: Secrets mount read-only, outside the ConfigMap mount" \
  "tls-grpc /etc/ourios-tls/grpc true
tls-grpc-client-ca /etc/ourios-tls/grpc-client-ca true
tls-http /etc/ourios-tls/http true" \
  "$(helm template t "$CHART" --show-only templates/receiver-statefulset.yaml "${secure_args[@]}" \
      | sed -n '/^ *- name: tls-/{N;N;s/^ *- name: \([a-z-]*\)\n *mountPath: \([^ ]*\)\n *readOnly: \([a-z]*\)$/\1 \2 \3/p;}')"
check "TLS on: volumes name the referenced Secrets" \
  "tls-http q-tls
tls-http-client-ca q-tls" \
  "$(helm template t "$CHART" --show-only templates/querier-deployment.yaml "${secure_args[@]}" \
      | sed -n '/^ *- name: tls-/{N;N;s/^ *- name: \([a-z-]*\)\n *secret:\n *secretName: "\([^"]*\)"$/\1 \2/p;}')"
check_fails "a client CA without the listener certificate is refused" \
  "querier.tls.http.clientCA.existingSecret needs" \
  --set querier.tls.http.clientCA.existingSecret=ca
check_fails "an unknown TLS minVersion is refused" "minVersion must be" \
  --set receiver.tls.http.existingSecret=t --set-string receiver.tls.http.minVersion=1.1

check "MCP on: querier.mcp.enabled renders" \
  "mcp:
enabled: true" \
  "$(role_config querier --set querier.mcp.enabled=true \
      | sed -n '/^  mcp:/,/^  [a-z_]*:$/p' | sed 's/^ *//' | head -2)"
check "MCP off by default" "" "$(role_config querier | grep 'mcp:' || true)"

# TLS-only settings without the listener's certificate would leave it on
# plaintext while looking configured: refuse them on every listener.
for listener in receiver.tls.grpc receiver.tls.http querier.tls.http; do
  check_fails "$listener.minVersion without existingSecret is refused" \
    "$listener.minVersion needs $listener.existingSecret" \
    --set-string "$listener.minVersion=1.3"
  check_fails "$listener.reloadIntervalSecs without existingSecret is refused" \
    "$listener.reloadIntervalSecs needs $listener.existingSecret" \
    --set "$listener.reloadIntervalSecs=60"
done

# Explicit invalid values must fail the render, typed zeros included: the
# empty-value shortcuts (`default`, `with`) would otherwise drop them.
check_fails "a numeric minVersion 0 is refused, not dropped" "minVersion must be" \
  --set receiver.tls.http.existingSecret=t --set receiver.tls.http.minVersion=0
check_fails "reloadIntervalSecs 0 is refused, not dropped" \
  "reloadIntervalSecs must be a positive integer" \
  --set querier.tls.http.existingSecret=t --set querier.tls.http.reloadIntervalSecs=0
check_fails "a non-integer reloadIntervalSecs is refused" \
  "reloadIntervalSecs must be a positive integer" \
  --set querier.tls.http.existingSecret=t --set-string querier.tls.http.reloadIntervalSecs=1m
fga_args=(
  --set 'auth.tokens[0].name=e' --set 'auth.tokens[0].tenants[0]=a'
  --set 'auth.tokens[0].secretKeyRef.name=s' --set 'auth.tokens[0].secretKeyRef.key=k'
  --set auth.openfga.enabled=true --set auth.openfga.apiUrl=http://f
  --set auth.openfga.storeId=s
)
check_fails "an OpenFGA apiToken secretKeyRef with a key but no name is refused" \
  "needs both name and key" "${fga_args[@]}" --set auth.openfga.apiToken.secretKeyRef.key=k
check_fails "an OpenFGA apiToken secretKeyRef with a name but no key is refused" \
  "needs both name and key" "${fga_args[@]}" --set auth.openfga.apiToken.secretKeyRef.name=n
check "OpenFGA without an apiToken renders no api_token" "" \
  "$(role_config querier "${fga_args[@]}" | grep 'api_token' || true)"
check_fails "a reserved token env name in global extraEnv is refused" \
  "extraEnv sets OURIOS_AUTH_TOKEN_0" \
  "${fga_args[@]}" --set 'extraEnv[0].name=OURIOS_AUTH_TOKEN_0' --set 'extraEnv[0].value=x'
check_fails "a reserved token env name in a role's extraEnv is refused" \
  "querier.extraEnv sets OURIOS_OPENFGA_API_TOKEN" \
  --set 'querier.extraEnv[0].name=OURIOS_OPENFGA_API_TOKEN' --set 'querier.extraEnv[0].value=x'

# NOTES.txt is not part of `helm template`; a client-only dry-run renders it
# without a cluster.
install_notes() {
  helm install --dry-run=client -n demo t "$CHART" "$@" | sed -n '/^NOTES:/,$p'
}
check "NOTES: auth off warns about open mode" "WARNING: authentication is OFF (RFC 0026 open mode). Any client that can" \
  "$(install_notes | grep 'WARNING: authentication is OFF')"
check "NOTES: querier TLS gives a CA- and name-aware curl" \
  "--cacert ca.crt --resolve t-ourios-querier.demo.svc:4319:127.0.0.1 \\
--data 'template_id == 0' https://t-ourios-querier.demo.svc:4319/v1/query" \
  "$(install_notes --set querier.tls.http.existingSecret=q \
      | grep -E -e '--cacert|--cert|https://' | sed 's/^ *//')"
check "NOTES: querier mTLS adds the client certificate" \
  "--cacert ca.crt --resolve t-ourios-querier.demo.svc:4319:127.0.0.1 \\
--cert client.crt --key client.key \\
--data 'template_id == 0' https://t-ourios-querier.demo.svc:4319/v1/query" \
  "$(install_notes --set querier.tls.http.existingSecret=q \
      --set querier.tls.http.clientCA.existingSecret=q \
      | grep -E -e '--cacert|--cert|https://' | sed 's/^ *//')"

# `helm upgrade --reuse-values` from a release before these keys existed
# carries no auth / tls / mcp maps at all (cf. #644): the render must not
# nil-pointer and must stay open mode.
check "upgrade path: auth, tls and mcp maps absent" "" \
  "$(role_config querier --set auth=null --set receiver.tls=null \
      --set querier.tls=null --set querier.mcp=null \
      | grep -E '^auth:|_tls:|mcp:' || true)"

check "helm test: an authenticated querier is probed over TCP, not an anonymous query" \
  "nc -z -w5 t-ourios-querier 4319" \
  "$(helm template t "$CHART" --show-only templates/tests/test-connection.yaml "${secure_args[@]}" \
      | grep -o 'nc -z -w5 t-ourios-querier 4319')"

if ((failures)); then
  printf '\n%d assertion(s) failed\n' "$failures" >&2
  exit 1
fi
printf '\nall render assertions passed\n'
