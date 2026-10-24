{{/*
Expand the name of the chart.
*/}}
{{- define "ourios.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "ourios.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "ourios.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "ourios.labels" -}}
helm.sh/chart: {{ include "ourios.chart" . }}
{{ include "ourios.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "ourios.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ourios.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
The three workload roles. The single source for every template that
ranges over roles — add/remove roles here only.
*/}}
{{- define "ourios.roles" -}}receiver querier compactor{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "ourios.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ourios.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
The ServiceAccount a role's pods run as. Pass (dict "root" $ "role"
"receiver"|"querier"|"compactor"): the role's own serviceAccount when
configured (create=true renders it; name alone binds an existing one),
falling back to the shared serviceAccount otherwise. Role-scoped accounts
are the least-privilege seam — with IRSA, each carries its own
eks.amazonaws.com/role-arn (README "Per-role IAM").
*/}}
{{- define "ourios.roleServiceAccountName" -}}
{{- $ := .root -}}
{{- $sa := (index $.Values .role).serviceAccount | default dict -}}
{{- if $sa.create -}}
{{- /* Truncate the base, not the joined name — a 63-char fullname must
never swallow the role suffix, or all three roles collide on one SA. */ -}}
{{- $base := include "ourios.fullname" $ | trunc (int (sub 62 (len .role))) | trimSuffix "-" -}}
{{- default (printf "%s-%s" $base .role) $sa.name }}
{{- else if $sa.name -}}
{{- $sa.name }}
{{- else -}}
{{- include "ourios.serviceAccountName" $ }}
{{- end }}
{{- end }}

{{/*
The image reference (repository:tag). The tag defaults to `latest`, a floating
tag that follows the newest release, rather than the chart appVersion. Pin a
released tag via image.tag in production.
*/}}
{{- define "ourios.image" -}}
{{- printf "%s:%s" .Values.image.repository (default "latest" .Values.image.tag) }}
{{- end }}

{{/*
Storage config block (RFC 0020 §3.4), shared by every role's config file. For s3
(the S3 API — AWS or any S3-compatible provider): the bucket + optional
addressing. Credentials are ${env:…} references (RFC 0020 §3.5), resolved from
the Secret named by storage.s3.existingSecret (envFrom) or left empty to fall
through to the AWS credential chain (IRSA / instance metadata). For local: the
in-container data root. Emitted under a top-level `storage:` key.
*/}}
{{- define "ourios.storageConfig" -}}
{{- if not (or (eq .Values.storage.backend "local") (eq .Values.storage.backend "s3")) }}
{{- fail (printf "storage.backend must be \"local\" or \"s3\", got %q" .Values.storage.backend) }}
{{- end }}
storage:
  backend: {{ .Values.storage.backend | quote }}
{{- if eq .Values.storage.backend "s3" }}
  s3:
    bucket: {{ required "storage.s3.bucket is required when storage.backend=s3" .Values.storage.s3.bucket | quote }}
{{- with .Values.storage.s3.endpoint }}
    endpoint: {{ . | quote }}
{{- end }}
{{- with .Values.storage.s3.region }}
    region: {{ . | quote }}
{{- end }}
{{- with .Values.storage.s3.prefix }}
    prefix: {{ . | quote }}
{{- end }}
    access_key_id: "${env:OURIOS_S3_ACCESS_KEY_ID:-}"
    secret_access_key: "${env:OURIOS_S3_SECRET_ACCESS_KEY:-}"
    session_token: "${env:OURIOS_S3_SESSION_TOKEN:-}"
{{- else }}
  local:
    bucket_root: {{ .Values.storage.local.bucketRoot | quote }}
{{- end }}
{{- $promoted := .Values.storage.promotedAttributes | default dict }}
{{- if not (kindIs "map" $promoted) }}
{{- fail (printf "storage.promotedAttributes must be a map with resource and log lists, got %v" $promoted) }}
{{- end }}
{{- if or $promoted.resource $promoted.log }}
  promoted_attributes:
{{- range $scope := list "resource" "log" }}
{{- with include "ourios.promotedList" (dict "path" (printf "storage.promotedAttributes.%s" $scope) "entries" (index $promoted $scope)) }}
    {{ $scope }}:
{{- . }}
{{- end }}
{{- end }}
{{- end }}
{{- end }}

{{/*
One storage.promotedAttributes list (RFC 0022 / RFC 0042 §3.2), 6-space
indented; pass (dict "path" "<values path>" "entries" <list>). An entry is a
bare key (the string class) or a {key, type} map. Renders nothing for an
absent or empty list.
*/}}
{{- define "ourios.promotedList" -}}
{{- if and (not (kindIs "invalid" .entries)) (not (kindIs "slice" .entries)) }}
{{- fail (printf "%s must be a list of attribute keys, got %v" .path .entries) }}
{{- end }}
{{- range $i, $e := .entries }}
{{- if kindIs "map" $e }}
{{- range $k, $_ := $e }}
{{- if not (has $k (list "key" "type")) }}
{{- fail (printf "%s[%d] has an unknown field %q: an entry is a key, or a map with key and type" $.path $i $k) }}
{{- end }}
{{- end }}
      - key: {{ required (printf "%s[%d].key is required" $.path $i) (include "ourios.setValue" $e.key) | quote }}
{{- with include "ourios.setValue" $e.type }}
        type: {{ . | quote }}
{{- end }}
{{- else if ne (include "ourios.setValue" $e) "" }}
      - {{ toString $e | quote }}
{{- else }}
{{- fail (printf "%s[%d] is empty: an entry is a key, or a map with key and type" $.path $i) }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The full RFC 0020 config file for one role. Pass
(dict "root" $ "role" "receiver"|"querier"|"compactor"): the shared storage block,
the auth section (see ourios.roleHasAuth), plus that role's flags and
listener TLS; the roles it does not run are absent (disabled). The
receiver/querier disable compaction so only the dedicated compactor sweeps
(RFC 0009 §3.2). Data-plane only — OTEL_* / AWS_* stay env (RFC 0020 §3.8).
*/}}
{{- define "ourios.config" -}}
{{- $ := .root -}}
{{- $role := .role -}}
{{- include "ourios.storageConfig" $ }}
{{- if include "ourios.roleHasAuth" (dict "root" $ "role" $role) }}
{{- include "ourios.authConfig" $ }}
{{- end }}
{{- if eq $role "receiver" }}
{{- include "ourios.minerConfig" $ }}
receiver:
  enabled: true
  grpc_addr: "0.0.0.0:4317"
  http_addr: "0.0.0.0:4318"
  wal_root: {{ $.Values.receiver.wal.mountPath | quote }}
  {{- with include "ourios.setValue" $.Values.receiver.encodeWorkers }}
  {{- if not (regexMatch "^[1-9][0-9]*$" .) }}
  {{- fail (printf "receiver.encodeWorkers must be a positive integer, got %v" $.Values.receiver.encodeWorkers) }}
  {{- end }}
  encode_workers: {{ . }}
  {{- end }}
  {{- if $.Values.receiver.templateIdsAllowBootstrap }}
  template_ids_allow_bootstrap: true
  {{- end }}
  {{- with include "ourios.tlsConfig" . }}
  {{- . | trim | nindent 2 }}
  {{- end }}
compaction:
  enabled: false
{{- else if eq $role "querier" }}
{{- if le (int $.Values.querier.defaultWindowSecs) 0 }}
{{- fail (printf "querier.defaultWindowSecs must be a positive integer (seconds), got %v" $.Values.querier.defaultWindowSecs) }}
{{- end }}
querier:
  enabled: true
  http_addr: "0.0.0.0:4319"
  default_window_secs: {{ $.Values.querier.defaultWindowSecs }}
  {{- if include "ourios.bool" (dict "path" "querier.mcp.enabled" "value" (dig "mcp" "enabled" nil $.Values.querier)) }}
  mcp:
    enabled: true
  {{- end }}
  {{- with include "ourios.tlsConfig" . }}
  {{- . | trim | nindent 2 }}
  {{- end }}
compaction:
  enabled: false
{{- else if eq $role "compactor" }}
{{- if le (int $.Values.compactor.intervalSecs) 0 }}
{{- fail (printf "compactor.intervalSecs must be a positive integer (seconds), got %v" $.Values.compactor.intervalSecs) }}
{{- end }}
compaction:
  enabled: true
  interval_secs: {{ $.Values.compactor.intervalSecs }}
{{- else }}
{{- fail (printf "ourios.config: unknown role %q" $role) }}
{{- end }}
{{- end }}

{{/*
The RFC 0050 §3.2 upstream-template dial (`miner.*`). The server reads it
only for the receiver, so only the receiver's config carries it. Absent
unless a key is set.
*/}}
{{- define "ourios.minerConfig" -}}
{{- $m := .Values.miner | default dict -}}
{{- $mode := include "ourios.setValue" $m.upstreamTemplates -}}
{{- $byteLimit := include "ourios.setValue" $m.upstreamTemplateByteLimit -}}
{{- $assocLimit := include "ourios.setValue" $m.upstreamAssociationLimit -}}
{{- if and $mode (not (has $mode (list "ignore" "observe" "adopt"))) }}
{{- fail (printf "miner.upstreamTemplates must be \"ignore\", \"observe\" or \"adopt\", got %v" $m.upstreamTemplates) }}
{{- end }}
{{- range $path, $v := dict "miner.upstreamTemplateByteLimit" $byteLimit "miner.upstreamAssociationLimit" $assocLimit }}
{{- if and $v (not (regexMatch "^[0-9]+$" $v)) }}
{{- fail (printf "%s must be a non-negative integer, got %v" $path $v) }}
{{- end }}
{{- end }}
{{- if or $mode $byteLimit $assocLimit }}
miner:
{{- with $mode }}
  upstream_templates: {{ . | quote }}
{{- end }}
{{- with $byteLimit }}
  upstream_template_byte_limit: {{ . }}
{{- end }}
{{- with $assocLimit }}
  upstream_association_limit: {{ . }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Whether a role's config carries the auth section; pass (dict "root" $
"role" "<role>"). Renders "true" or nothing. The receiver and querier take
it to authenticate. The compactor takes it only when OpenFGA binds a
visibility object: it then runs the graph emitter on each sweep (RFC 0047)
and `graph backfill` reads auth.openfga from its config (RFC 0048).
*/}}
{{- define "ourios.roleHasAuth" -}}
{{- if or (eq .role "receiver") (eq .role "querier") }}true
{{- else if and (eq .role "compactor") (include "ourios.openfgaEnabled" .root) (include "ourios.visibilityObjects" .root) }}true
{{- end }}
{{- end }}

{{/*
The validated auth.openfga.visibility.objects list as JSON (read back with
fromJson), or nothing when it is absent or empty.
*/}}
{{- define "ourios.visibilityObjects" -}}
{{- $objects := dig "openfga" "visibility" "objects" nil (.Values.auth | default dict) -}}
{{- if and (not (kindIs "invalid" $objects)) (not (kindIs "slice" $objects)) }}
{{- fail (printf "auth.openfga.visibility.objects must be a list of {type, column} entries, got %v" $objects) }}
{{- end }}
{{- if $objects }}
{{- range $i, $o := $objects }}
{{- if not (kindIs "map" $o) }}
{{- fail (printf "auth.openfga.visibility.objects[%d] must be a map with type and column, got %v" $i $o) }}
{{- end }}
{{- end }}
{{- toJson $objects }}
{{- end }}
{{- end }}

{{/*
One optional string list of the visibility section; pass (dict "key" "<yaml
key>" "path" "<values path>" "value" <v> "indent" <n>). Renders nothing for
an absent or empty list, so the server default applies.
*/}}
{{- define "ourios.optList" -}}
{{- if and (not (kindIs "invalid" .value)) (not (kindIs "slice" .value)) }}
{{- fail (printf "%s must be a list of column names, got %v" .path .value) }}
{{- end }}
{{- $pad := repeat (int .indent) " " }}
{{- if .value }}
{{ $pad }}{{ .key }}:
{{- range .value }}
{{ $pad }}  - {{ toString . | quote }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The TLS listeners a role serves (RFC 0030): the value keys under
<role>.tls, which are also the config keys' `<listener>_tls` prefix.
*/}}
{{- define "ourios.tlsListeners" -}}
{{- if eq . "receiver" }}grpc http{{- else if eq . "querier" }}http{{- end }}
{{- end }}

{{/*
A Secret coordinate (a Secret name or key) as a string; pass (dict "path"
"<values path>" "value" <v>). Absent or "" renders nothing; any other
non-string (a YAML 0, false, a map) fails the render, so a typo can never
read as "unset" and silently turn TLS or a token off.
*/}}
{{- define "ourios.secretRef" -}}
{{- if and (not (kindIs "invalid" .value)) (ne (toString .value) "") }}
{{- if not (kindIs "string" .value) }}
{{- fail (printf "%s must be a Secret name or key (a non-empty string), got %v" .path .value) }}
{{- end }}
{{- .value }}
{{- end }}
{{- end }}

{{/*
One listener's validated TLS settings as JSON (read back with fromJson);
pass (dict "root" $ "role" "<role>" "listener" "<listener>"). Every TLS
helper and NOTES.txt reads this one result. Defaults live here rather than
in values.yaml, so `helm upgrade --reuse-values` from a release without
these keys gets them too. `secret` is empty when the listener is
plaintext.
*/}}
{{- define "ourios.tlsListener" -}}
{{- $role := .role -}}
{{- $l := .listener -}}
{{- $p := printf "%s.tls.%s" $role $l -}}
{{- $t := dig "tls" $l (dict) (index .root.Values $role | default dict) -}}
{{- $ca := $t.clientCA | default dict -}}
{{- $secret := include "ourios.secretRef" (dict "path" (printf "%s.existingSecret" $p) "value" $t.existingSecret) -}}
{{- $caSecret := include "ourios.secretRef" (dict "path" (printf "%s.clientCA.existingSecret" $p) "value" $ca.existingSecret) -}}
{{- $caKey := include "ourios.secretRef" (dict "path" (printf "%s.clientCA.key" $p) "value" $ca.key) | default "ca.crt" -}}
{{- $minVersion := include "ourios.setValue" $t.minVersion -}}
{{- $reload := include "ourios.setValue" $t.reloadIntervalSecs -}}
{{- if $secret }}
{{- if and $minVersion (not (has $minVersion (list "1.2" "1.3"))) }}
{{- fail (printf "%s.minVersion must be \"1.2\" or \"1.3\", got %v" $p $t.minVersion) }}
{{- end }}
{{- if and $reload (not (regexMatch "^[1-9][0-9]*$" $reload)) }}
{{- fail (printf "%s.reloadIntervalSecs must be a positive integer (seconds), got %v" $p $t.reloadIntervalSecs) }}
{{- end }}
{{- else if $caSecret }}
{{- fail (printf "%s.clientCA.existingSecret needs %s.existingSecret: mTLS requires the listener's own certificate" $p $p) }}
{{- else if $minVersion }}
{{- fail (printf "%s.minVersion needs %s.existingSecret: without a certificate the listener stays plaintext (RFC 0030 §3.1)" $p $p) }}
{{- else if $reload }}
{{- fail (printf "%s.reloadIntervalSecs needs %s.existingSecret: without a certificate the listener stays plaintext (RFC 0030 §3.1)" $p $p) }}
{{- end }}
{{- toJson (dict "secret" $secret "caSecret" $caSecret "caKey" $caKey "minVersion" $minVersion "reload" $reload) }}
{{- end }}

{{/*
One `<listener>_tls` config block per listener with a Secret configured; pass
(dict "root" $ "role" "<role>"). The paths point at the read-only Secret
mounts from ourios.tlsVolumes, outside /etc/ourios (the ConfigMap mount).
Mounted without subPath, so a rotated Secret reaches the files and
reloadIntervalSecs picks it up.
*/}}
{{- define "ourios.tlsConfig" -}}
{{- $root := .root -}}
{{- $role := .role -}}
{{- range $l := splitList " " (include "ourios.tlsListeners" $role) }}
{{- $v := include "ourios.tlsListener" (dict "root" $root "role" $role "listener" $l) | fromJson }}
{{- if $v.secret }}
{{ $l }}_tls:
  cert_file: "/etc/ourios-tls/{{ $l }}/tls.crt"
  key_file: "/etc/ourios-tls/{{ $l }}/tls.key"
{{- if $v.caSecret }}
  client_ca_file: "/etc/ourios-tls/{{ $l }}-client-ca/{{ $v.caKey }}"
{{- end }}
{{- with $v.minVersion }}
  min_version: {{ . | quote }}
{{- end }}
{{- with $v.reload }}
  reload_interval_secs: {{ . }}
{{- end }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The read-only Secret volumes behind ourios.tlsConfig; pass (dict "root" $
"role" "<role>"). Renders empty when no listener has TLS.
*/}}
{{- define "ourios.tlsVolumes" -}}
{{- $root := .root -}}
{{- $role := .role -}}
{{- range $l := splitList " " (include "ourios.tlsListeners" $role) }}
{{- $v := include "ourios.tlsListener" (dict "root" $root "role" $role "listener" $l) | fromJson }}
{{- with $v.secret }}
- name: tls-{{ $l }}
  secret:
    secretName: {{ . | quote }}
{{- end }}
{{- with $v.caSecret }}
- name: tls-{{ $l }}-client-ca
  secret:
    secretName: {{ . | quote }}
{{- end }}
{{- end }}
{{- end }}

{{- define "ourios.tlsVolumeMounts" -}}
{{- $root := .root -}}
{{- $role := .role -}}
{{- range $l := splitList " " (include "ourios.tlsListeners" $role) }}
{{- $v := include "ourios.tlsListener" (dict "root" $root "role" $role "listener" $l) | fromJson }}
{{- if $v.secret }}
- name: tls-{{ $l }}
  mountPath: /etc/ourios-tls/{{ $l }}
  readOnly: true
{{- end }}
{{- if $v.caSecret }}
- name: tls-{{ $l }}-client-ca
  mountPath: /etc/ourios-tls/{{ $l }}-client-ca
  readOnly: true
{{- end }}
{{- end }}
{{- end }}

{{/*
A scalar value as a string, or nothing when it is absent or empty. A typed
zero renders "0", so it reaches validation instead of being dropped as
empty the way `default` and `with` would drop it.
*/}}
{{- define "ourios.setValue" -}}
{{- if and (not (kindIs "invalid" .)) (ne (toString .) "") }}{{ toString . }}{{- end }}
{{- end }}

{{/*
Whether a role's listeners serve TLS; pass (dict "root" $ "role" "<role>").
Renders "true" or nothing.
*/}}
{{- define "ourios.tlsEnabled" -}}
{{- $root := .root -}}
{{- $role := .role -}}
{{- range $l := splitList " " (include "ourios.tlsListeners" $role) }}
{{- if (include "ourios.tlsListener" (dict "root" $root "role" $role "listener" $l) | fromJson).secret }}true{{- end }}
{{- end }}
{{- end }}

{{/*
A boolean switch; pass (dict "path" "<values path>" "value" <v>). Renders
"true" or nothing. Absent is false and a real bool is used as is; anything
else fails the render, so a quoted "false" can never read as on (a
non-empty string is truthy in a template).
*/}}
{{- define "ourios.bool" -}}
{{- if not (kindIs "invalid" .value) }}
{{- if not (kindIs "bool" .value) }}
{{- fail (printf "%s must be a boolean (true or false, unquoted), got %q" .path (toString .value)) }}
{{- end }}
{{- if .value }}true{{- end }}
{{- end }}
{{- end }}

{{/*
The validated auth switches; pass the root context. Each renders "true" or
nothing. auth.tokens must be a list when present: a non-list would
otherwise read as "no tokens" and leave the install in open mode.
*/}}
{{- define "ourios.oidcEnabled" -}}
{{- include "ourios.bool" (dict "path" "auth.oidc.enabled" "value" (dig "oidc" "enabled" nil (.Values.auth | default dict))) }}
{{- end }}

{{- define "ourios.openfgaEnabled" -}}
{{- include "ourios.bool" (dict "path" "auth.openfga.enabled" "value" (dig "openfga" "enabled" nil (.Values.auth | default dict))) }}
{{- end }}

{{- define "ourios.tokensConfigured" -}}
{{- $tokens := (.Values.auth | default dict).tokens }}
{{- if and (not (kindIs "invalid" $tokens)) (not (kindIs "slice" $tokens)) }}
{{- fail (printf "auth.tokens must be a list of token entries, got %v" $tokens) }}
{{- end }}
{{- if $tokens }}true{{- end }}
{{- end }}

{{/*
Whether an `auth` section renders (tokens or oidc configured). Renders "true"
or nothing. OpenFGA alone authenticates nothing, so it does not count.
*/}}
{{- define "ourios.authEnabled" -}}
{{- if or (include "ourios.tokensConfigured" .) (include "ourios.oidcEnabled" .) }}true{{- end }}
{{- end }}

{{/*
One optional scalar leaf of an auth subsection (4-space indent); pass
(dict "key" "<yaml key>" "value" <v>). Renders nothing for an absent or
empty value; 0 still renders.
*/}}
{{- define "ourios.optScalar" -}}
{{- if and (not (kindIs "invalid" .value)) (ne (toString .value) "") }}
{{ repeat 4 " " }}{{ .key }}: {{ toString .value | quote }}
{{- end }}
{{- end }}

{{/*
A validated secretKeyRef as JSON {name, key} (read back with fromJson);
pass (dict "path" "<values path>" "ref" <map>). Each half goes through
ourios.secretRef, so a non-string fails the render.
*/}}
{{- define "ourios.secretKeyRef" -}}
{{- $ref := .ref | default dict -}}
{{- toJson (dict
  "name" (include "ourios.secretRef" (dict "path" (printf "%s.name" .path) "value" $ref.name))
  "key" (include "ourios.secretRef" (dict "path" (printf "%s.key" .path) "value" $ref.key))) }}
{{- end }}

{{/*
The OpenFGA API token's validated secretKeyRef; pass the auth.openfga map.
*/}}
{{- define "ourios.openfgaTokenRef" -}}
{{- include "ourios.secretKeyRef" (dict "path" "auth.openfga.apiToken.secretKeyRef" "ref" (dig "apiToken" "secretKeyRef" (dict) (. | default dict))) }}
{{- end }}

{{/*
The `auth` config section (RFC 0026 tokens, RFC 0029 oidc, RFC 0047 openfga)
for the roles ourios.roleHasAuth selects. Absent unless tokens or oidc are configured,
which keeps open mode the default. Every secret leaf is an ${env:…}
reference to a variable ourios.authEnv fills from a secretKeyRef, so no
secret value reaches the ConfigMap.
*/}}
{{- define "ourios.authConfig" -}}
{{- $auth := .Values.auth | default dict -}}
{{- $tokens := ternary $auth.tokens list (eq (include "ourios.tokensConfigured" .) "true") -}}
{{- $oidc := $auth.oidc | default dict -}}
{{- $fga := $auth.openfga | default dict -}}
{{- $oidcOn := include "ourios.oidcEnabled" . -}}
{{- $fgaOn := include "ourios.openfgaEnabled" . -}}
{{- if and $fgaOn (not (include "ourios.authEnabled" .)) }}
{{- fail "auth.openfga.enabled needs auth.tokens or auth.oidc.enabled: OpenFGA binds the tenants of what they authenticate and never authenticates on its own (RFC 0047 §3.1)" }}
{{- end }}
{{- if include "ourios.authEnabled" . }}
auth:
{{- with $tokens }}
  tokens:
{{- range $i, $t := . }}
{{- if hasKey $t "token" }}
{{- fail (printf "auth.tokens[%d].token is not accepted: put the token in a Secret and reference it with auth.tokens[%d].secretKeyRef (name, key)" $i $i) }}
{{- end }}
{{- $ref := include "ourios.secretKeyRef" (dict "path" (printf "auth.tokens[%d].secretKeyRef" $i) "ref" $t.secretKeyRef) | fromJson }}
{{- if not (and $ref.name $ref.key) }}
{{- fail (printf "auth.tokens[%d].secretKeyRef.name and .key are required: the token value comes only from a Secret" $i) }}
{{- end }}
{{- if not (and (kindIs "slice" $t.tenants) $t.tenants) }}
{{- fail (printf "auth.tokens[%d].tenants must list at least one tenant, or \"*\" for all (RFC 0026 §3.1)" $i) }}
{{- end }}
    - name: {{ required (printf "auth.tokens[%d].name is required" $i) $t.name | quote }}
      token: "${env:OURIOS_AUTH_TOKEN_{{ $i }}}"
      tenants:
{{- range $t.tenants }}
        - {{ toString . | quote }}
{{- end }}
{{- end }}
{{- end }}
{{- if $oidcOn }}
  oidc:
    issuer: {{ required "auth.oidc.issuer is required with auth.oidc.enabled" $oidc.issuer | quote }}
    audience: {{ required "auth.oidc.audience is required with auth.oidc.enabled" $oidc.audience | quote }}
{{- if $fgaOn }}
{{- include "ourios.optScalar" (dict "key" "tenant_claim" "value" $oidc.tenantClaim) }}
{{- else }}
    tenant_claim: {{ required "auth.oidc.tenantClaim is required with auth.oidc.enabled unless auth.openfga binds the tenants (RFC 0029 §3.1)" $oidc.tenantClaim | quote }}
{{- end }}
{{- include "ourios.optScalar" (dict "key" "name_claim" "value" $oidc.nameClaim) }}
{{- include "ourios.optScalar" (dict "key" "clock_skew_secs" "value" $oidc.clockSkewSecs) }}
{{- include "ourios.optScalar" (dict "key" "agent_claim" "value" $oidc.agentClaim) }}
{{- include "ourios.optScalar" (dict "key" "groups_claim" "value" $oidc.groupsClaim) }}
{{- end }}
{{- if $fgaOn }}
  openfga:
    api_url: {{ required "auth.openfga.apiUrl is required with auth.openfga.enabled" $fga.apiUrl | quote }}
    store_id: {{ required "auth.openfga.storeId is required with auth.openfga.enabled" $fga.storeId | quote }}
{{- include "ourios.optScalar" (dict "key" "authorization_model_id" "value" $fga.authorizationModelId) }}
{{- $ref := include "ourios.openfgaTokenRef" $fga | fromJson }}
{{- if and (or $ref.name $ref.key) (not (and $ref.name $ref.key)) }}
{{- fail "auth.openfga.apiToken.secretKeyRef needs both name and key, or neither (no API token)" }}
{{- end }}
{{- if $ref.name }}
    api_token: "${env:OURIOS_OPENFGA_API_TOKEN}"
{{- end }}
{{- include "ourios.optScalar" (dict "key" "session_ttl_secs" "value" $fga.sessionTtlSecs) }}
{{- include "ourios.optScalar" (dict "key" "consistency" "value" $fga.consistency) }}
{{- include "ourios.optScalar" (dict "key" "request_timeout_secs" "value" $fga.requestTimeoutSecs) }}
{{- include "ourios.optScalar" (dict "key" "server_list_objects_deadline_ms" "value" $fga.serverListObjectsDeadlineMs) }}
{{- include "ourios.visibilityConfig" . }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The RFC 0047 §3.4 / RFC 0048 §3.2 `auth.openfga.visibility` block; pass the
root context. Absent unless a key is set, so every field takes the server
default. Nothing in it is secret. Its columns must be promoted
(storage.promotedAttributes); the server checks that at startup.
*/}}
{{- define "ourios.visibilityConfig" -}}
{{- $v := dig "openfga" "visibility" (dict) (.Values.auth | default dict) | default dict -}}
{{- $ids := $v.identities | default dict -}}
{{- $objects := include "ourios.visibilityObjects" . -}}
{{- $body := "" -}}
{{- if $objects }}
{{- $body = print $body "\n      objects:" }}
{{- range $i, $o := fromJsonArray $objects }}
{{- $body = print $body "\n        - type: " (required (printf "auth.openfga.visibility.objects[%d].type is required" $i) (include "ourios.setValue" $o.type) | quote) }}
{{- $body = print $body "\n          column: " (required (printf "auth.openfga.visibility.objects[%d].column is required" $i) (include "ourios.setValue" $o.column) | quote) }}
{{- end }}
{{- end }}
{{- $identities := print
  (include "ourios.optList" (dict "key" "user_columns" "path" "auth.openfga.visibility.identities.userColumns" "value" $ids.userColumns "indent" 8))
  (include "ourios.optList" (dict "key" "agent_columns" "path" "auth.openfga.visibility.identities.agentColumns" "value" $ids.agentColumns "indent" 8)) }}
{{- with $identities }}
{{- $body = print $body "\n      identities:" . }}
{{- end }}
{{- with include "ourios.setValue" $v.selfPrincipalColumn }}
{{- $body = print $body "\n      self_principal_column: " (. | quote) }}
{{- end }}
{{- $body = print $body (include "ourios.optList" (dict "key" "content_columns" "path" "auth.openfga.visibility.contentColumns" "value" $v.contentColumns "indent" 6)) }}
{{- range $key, $path := dict "max_objects" "maxObjects" "list_timeout_ms" "listTimeoutMs" }}
{{- with include "ourios.setValue" (index $v $path) }}
{{- if not (regexMatch "^[1-9][0-9]*$" .) }}
{{- fail (printf "auth.openfga.visibility.%s must be a positive integer, got %v" $path .) }}
{{- end }}
{{- $body = print $body "\n      " $key ": " . }}
{{- end }}
{{- end }}
{{- with $body }}
    visibility:
{{- . }}
{{- end }}
{{- end }}

{{/*
The env behind ourios.authConfig's ${env:…} references: each token and the
OpenFGA API token, read from the referenced Secret with secretKeyRef. Empty
when auth is off. Only the roles ourios.roleHasAuth selects take it.
*/}}
{{- define "ourios.authEnv" -}}
{{- if include "ourios.authEnabled" . }}
{{- $auth := .Values.auth | default dict -}}
{{- range $i, $t := ternary $auth.tokens list (eq (include "ourios.tokensConfigured" .) "true") }}
- name: OURIOS_AUTH_TOKEN_{{ $i }}
  valueFrom:
    secretKeyRef:
      {{- $ref := include "ourios.secretKeyRef" (dict "path" (printf "auth.tokens[%d].secretKeyRef" $i) "ref" $t.secretKeyRef) | fromJson }}
      name: {{ $ref.name | quote }}
      key: {{ $ref.key | quote }}
{{- end }}
{{- $fga := $auth.openfga | default dict }}
{{- $ref := include "ourios.openfgaTokenRef" $fga | fromJson }}
{{- if and (include "ourios.openfgaEnabled" .) $ref.name }}
- name: OURIOS_OPENFGA_API_TOKEN
  valueFrom:
    secretKeyRef:
      name: {{ $ref.name | quote }}
      key: {{ $ref.key | quote }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Env common to every workload. The data-plane config is the mounted --config file
(RFC 0020); the only env vars are the self-telemetry OTLP endpoint, the AWS SDK
region (which drives the credential chain for s3), and any extraEnv — OTEL_* /
AWS_* are read directly by their SDKs, never modeled in the config (RFC 0020
§3.8). The chart deliberately models no OTel SDK knob beyond the endpoint: that
vocabulary is the SDK's env-var contract, not ours, so it is set verbatim in
extraEnv (see values.yaml). May render empty (the workloads guard the `env:`
block). The dedicated compactor is the only sweeper (the per-role config
disables compaction on the receiver/querier), so it must be enabled.
*/}}
{{- define "ourios.commonEnv" -}}
{{- if not .Values.compactor.enabled }}
{{- fail "compactor.enabled=false leaves the deployment with no sweeper: the receiver and querier disable compaction in their config, so the dedicated compactor is the chart's only compactor. Set compactor.enabled=true (small files accumulate otherwise — hazard #4)." }}
{{- end }}
{{- if and (eq .Values.storage.backend "s3") .Values.storage.s3.region }}
- name: AWS_DEFAULT_REGION
  value: {{ .Values.storage.s3.region | quote }}
{{- end }}
{{- with dig "exporterEndpoint" "" (default (dict) .Values.otel) }}
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- with .Values.extraEnv }}
{{- toYaml . }}
{{- end }}
{{- end }}

{{/*
Env for one workload; pass (dict "root" $ "role" "<role>"): the common env
above plus the role's own extraEnv. The role list renders AFTER the global
one, so two roles can carry different values for the same name (e.g. per-role
OTEL_RESOURCE_ATTRIBUTES) and a duplicate resolves to the role's entry —
Kubernetes takes the last occurrence.
*/}}
{{- define "ourios.workloadEnv" -}}
{{- $roleEnv := dig "extraEnv" (list) (index .root.Values .role | default (dict)) }}
{{- /* The auth env names are reserved: an extraEnv entry with one of them
would replace the Secret-sourced token (Kubernetes keeps the last). */}}
{{- range $source, $list := dict "extraEnv" (.root.Values.extraEnv | default list) (printf "%s.extraEnv" .role) $roleEnv }}
{{- range $list }}
{{- if regexMatch "^OURIOS_(AUTH_TOKEN_[0-9]+|OPENFGA_API_TOKEN)$" (toString .name) }}
{{- fail (printf "%s sets %s, a name the chart reserves for a Secret-sourced auth token: set the token through auth.tokens[].secretKeyRef or auth.openfga.apiToken.secretKeyRef instead" $source .name) }}
{{- end }}
{{- end }}
{{- end }}
{{- if include "ourios.roleHasAuth" . }}
{{- include "ourios.authEnv" .root }}
{{- end }}
{{- include "ourios.commonEnv" .root }}
{{- /* index-then-dig: dig cannot traverse the typed .Values root, and the
role key may be absent under `helm upgrade --reuse-values`. */}}
{{- with $roleEnv }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{/*
The mounted RFC 0020 config file, passed to the binary via --config. The
ConfigMap holds one key per role; each workload mounts its own to
/etc/ourios/config.yaml. Pass (dict "root" $ "role" "<role>") for the volume.
*/}}
{{- define "ourios.configVolume" -}}
- name: config
  configMap:
    name: {{ include "ourios.fullname" .root }}-config
    items:
      - key: {{ .role }}.yaml
        path: config.yaml
{{- end }}

{{- define "ourios.configVolumeMount" -}}
- name: config
  mountPath: /etc/ourios
  readOnly: true
{{- end }}

{{/*
Object-store credential envFrom: the Secret named by storage.s3.existingSecret,
if set. The Secret holds the S3-named credential keys Ourios reads
(OURIOS_S3_ACCESS_KEY_ID / OURIOS_S3_SECRET_ACCESS_KEY [/ OURIOS_S3_SESSION_TOKEN],
RFC 0019 §3.4) — working against AWS S3 and every S3-compatible backend. Empty
otherwise (IRSA / instance metadata supply credentials via the AWS chain, no
static keys). Only emitted for the s3 backend — the local backend has no
credentials, so a stray existingSecret is neither mounted nor cross-checked. The
two credential modes are mutually exclusive — static keys would shadow the IRSA
web-identity credentials — so configuring both is rejected.
*/}}
{{- define "ourios.s3CredentialsEnvFrom" -}}
{{- if eq .Values.storage.backend "s3" }}
{{- $anyArn := index (.Values.serviceAccount.annotations | default dict) "eks.amazonaws.com/role-arn" }}
{{- if and $anyArn (not .Values.serviceAccount.create) }}
{{- fail "serviceAccount.annotations \"eks.amazonaws.com/role-arn\" (IRSA) requires serviceAccount.create=true so the chart applies it; with create=false the chart renders no ServiceAccount and the annotation has no effect. Either set serviceAccount.create=true, or annotate your existing ServiceAccount out-of-band and remove it here." }}
{{- end }}
{{- range $role := splitList " " (include "ourios.roles" $) }}
{{- $sa := (index $.Values $role).serviceAccount | default dict }}
{{- $roleArn := index ($sa.annotations | default dict) "eks.amazonaws.com/role-arn" }}
{{- if and $roleArn (not $sa.create) }}
{{- fail (printf "%s.serviceAccount.annotations \"eks.amazonaws.com/role-arn\" (IRSA) requires %s.serviceAccount.create=true so the chart applies it; with create=false the annotation has no effect. Either set create=true, or annotate the existing ServiceAccount out-of-band and remove it here." $role $role) }}
{{- end }}
{{- $anyArn = or $anyArn $roleArn }}
{{- end }}
{{- if and .Values.storage.s3.existingSecret $anyArn }}
{{- fail "storage.s3.existingSecret and IRSA (an \"eks.amazonaws.com/role-arn\" annotation on the shared or a per-role serviceAccount) are mutually exclusive: static keys would shadow the web-identity credentials. Set exactly one credential mode." }}
{{- end }}
{{- with .Values.storage.s3.existingSecret }}
envFrom:
  - secretRef:
      name: {{ . | quote }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The local data volume mount, only for the local backend (the s3 backend mounts
no data volume — the store is S3).
*/}}
{{- define "ourios.dataVolumeMount" -}}
{{- if eq .Values.storage.backend "local" }}
- name: data
  mountPath: {{ .Values.storage.local.bucketRoot }}
{{- end }}
{{- end }}

{{/*
The local data volume, only for the local backend. A single shared PVC mounted
by every workload (dev/single-node; see values.yaml).
*/}}
{{- define "ourios.dataVolume" -}}
{{- if eq .Values.storage.backend "local" }}
- name: data
  persistentVolumeClaim:
    claimName: {{ include "ourios.fullname" . }}-data
{{- end }}
{{- end }}

{{/*
Merged pod annotations for a workload: the chart-level .Values.podAnnotations
plus the per-role map, with role-specific entries winning on conflict (so shared
annotations and role-specific ones both apply). Pass a dict with "global" and
"role" keys. Renders nothing when both are empty.
*/}}
{{- define "ourios.podAnnotations" -}}
{{- $merged := merge (deepCopy (.role | default dict)) (.global | default dict) -}}
{{- with $merged }}
{{- toYaml . }}
{{- end }}
{{- end }}
