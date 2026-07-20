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
The image reference (repository:tag). The tag defaults to `latest` (a
publishable floating tag) rather than the chart appVersion — appVersion tracks
the unreleased crate version (0.0.0), for which no image is ever pushed. Pin a
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
{{- end }}

{{/*
The full RFC 0020 config file for one role. Pass
(dict "root" $ "role" "receiver"|"querier"|"compactor"): the shared storage block
plus that role's flags; the roles it does not run are absent (disabled). The
receiver/querier disable compaction so only the dedicated compactor sweeps
(RFC 0009 §3.2). Data-plane only — OTEL_* / AWS_* stay env (RFC 0020 §3.8).
*/}}
{{- define "ourios.config" -}}
{{- $ := .root -}}
{{- $role := .role -}}
{{- include "ourios.storageConfig" $ }}
{{- if eq $role "receiver" }}
receiver:
  enabled: true
  grpc_addr: "0.0.0.0:4317"
  http_addr: "0.0.0.0:4318"
  wal_root: {{ $.Values.receiver.wal.mountPath | quote }}
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
Env common to every workload. The data-plane config is the mounted --config file
(RFC 0020); the only env vars are the self-telemetry OTLP endpoint, the AWS SDK
region (which drives the credential chain for s3), and any extraEnv — OTEL_* /
AWS_* are read directly by their SDKs, never modeled in the config (RFC 0020
§3.8). May render empty (the workloads guard the `env:` block). The dedicated
compactor is the only sweeper (the per-role config disables compaction on the
receiver/querier), so it must be enabled.
*/}}
{{- define "ourios.commonEnv" -}}
{{- if not .Values.compactor.enabled }}
{{- fail "compactor.enabled=false leaves the deployment with no sweeper: the receiver and querier disable compaction in their config, so the dedicated compactor is the chart's only compactor. Set compactor.enabled=true (small files accumulate otherwise — hazard #4)." }}
{{- end }}
{{- if and (eq .Values.storage.backend "s3") .Values.storage.s3.region }}
- name: AWS_DEFAULT_REGION
  value: {{ .Values.storage.s3.region | quote }}
{{- end }}
{{- with .Values.otel.exporterEndpoint }}
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- with .Values.extraEnv }}
{{- toYaml . }}
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
