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
The image reference (repository:tag). The tag defaults to `latest` (a
publishable floating tag) rather than the chart appVersion — appVersion tracks
the unreleased crate version (0.0.0), for which no image is ever pushed. Pin a
released tag via image.tag in production.
*/}}
{{- define "ourios.image" -}}
{{- printf "%s:%s" .Values.image.repository (default "latest" .Values.image.tag) }}
{{- end }}

{{/*
Storage-backend env (RFC 0019). For s3 (the S3 API — AWS or any S3-compatible
provider): the bucket + optional addressing. Credentials never appear here —
they come from the S3 credential chain (storage.s3.existingSecret → envFrom, or
IRSA on EKS). The region drives both the store (OURIOS_S3_REGION) and the SDK
credential chain (AWS_DEFAULT_REGION), which the S3 client needs for every
S3-compatible backend. For local: the in-container data root.
*/}}
{{- define "ourios.storageEnv" -}}
{{- if not (or (eq .Values.storage.backend "local") (eq .Values.storage.backend "s3")) }}
{{- fail (printf "storage.backend must be \"local\" or \"s3\", got %q" .Values.storage.backend) }}
{{- end }}
- name: OURIOS_STORAGE_BACKEND
  value: {{ .Values.storage.backend | quote }}
{{- if eq .Values.storage.backend "s3" }}
- name: OURIOS_S3_BUCKET
  value: {{ required "storage.s3.bucket is required when storage.backend=s3" .Values.storage.s3.bucket | quote }}
{{- with .Values.storage.s3.endpoint }}
- name: OURIOS_S3_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- with .Values.storage.s3.region }}
- name: OURIOS_S3_REGION
  value: {{ . | quote }}
- name: AWS_DEFAULT_REGION
  value: {{ . | quote }}
{{- end }}
{{- with .Values.storage.s3.prefix }}
- name: OURIOS_S3_PREFIX
  value: {{ . | quote }}
{{- end }}
{{- else }}
- name: OURIOS_BUCKET_ROOT
  value: {{ .Values.storage.local.bucketRoot | quote }}
{{- end }}
{{- end }}

{{/*
Env common to every workload: the storage backend, self-telemetry, and any
extraEnv. Compaction is wired per-workload — only the dedicated compactor
sweeps; the receiver/querier set OURIOS_COMPACTION_ENABLED=0 (RFC 0009 §3.2) so
a single sweeper avoids redundant per-interval listing.
*/}}
{{- define "ourios.commonEnv" -}}
{{- if not .Values.compactor.enabled }}
{{- fail "compactor.enabled=false leaves the deployment with no sweeper: the receiver and querier set OURIOS_COMPACTION_ENABLED=0, so the dedicated compactor is the chart's only compactor. Set compactor.enabled=true (small files accumulate otherwise — hazard #4)." }}
{{- end }}
{{ include "ourios.storageEnv" . }}
{{- with .Values.otel.exporterEndpoint }}
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- with .Values.extraEnv }}
{{- toYaml . }}
{{- end }}
{{- end }}

{{/*
Receiver-role env (RFC 0003). The WAL root is the per-replica local PVC mount —
never S3 (CLAUDE.md §3.4/§3.6).
*/}}
{{- define "ourios.receiverEnv" -}}
- name: OURIOS_RECEIVER_ENABLED
  value: "true"
# Compaction runs only on the dedicated compactor (RFC 0009 §3.2).
- name: OURIOS_COMPACTION_ENABLED
  value: "0"
- name: OURIOS_RECEIVER_GRPC_ADDR
  value: "0.0.0.0:4317"
- name: OURIOS_RECEIVER_HTTP_ADDR
  value: "0.0.0.0:4318"
- name: OURIOS_WAL_ROOT
  value: {{ .Values.receiver.wal.mountPath | quote }}
{{- end }}

{{/*
Querier-role env (RFC 0016).
*/}}
{{- define "ourios.querierEnv" -}}
{{- if le (int .Values.querier.defaultWindowSecs) 0 }}
{{- fail (printf "querier.defaultWindowSecs must be a positive integer (seconds), got %v" .Values.querier.defaultWindowSecs) }}
{{- end }}
- name: OURIOS_QUERIER_ENABLED
  value: "true"
# Compaction runs only on the dedicated compactor (RFC 0009 §3.2).
- name: OURIOS_COMPACTION_ENABLED
  value: "0"
- name: OURIOS_QUERIER_HTTP_ADDR
  value: "0.0.0.0:4319"
- name: OURIOS_QUERIER_DEFAULT_WINDOW_SECS
  value: {{ .Values.querier.defaultWindowSecs | quote }}
{{- end }}

{{/*
Compactor-role env (RFC 0009): the sweep cadence. Compaction is on by default
in the binary, so the dedicated compactor needs no enable flag — only the
interval. The receiver/querier disable it via OURIOS_COMPACTION_ENABLED=0.
*/}}
{{- define "ourios.compactorEnv" -}}
{{- if le (int .Values.compactor.intervalSecs) 0 }}
{{- fail (printf "compactor.intervalSecs must be a positive integer (seconds), got %v" .Values.compactor.intervalSecs) }}
{{- end }}
- name: OURIOS_COMPACTION_INTERVAL_SECS
  value: {{ .Values.compactor.intervalSecs | quote }}
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
{{- $roleArn := index (.Values.serviceAccount.annotations | default dict) "eks.amazonaws.com/role-arn" }}
{{- if and $roleArn (not .Values.serviceAccount.create) }}
{{- fail "serviceAccount.annotations \"eks.amazonaws.com/role-arn\" (IRSA) requires serviceAccount.create=true so the chart applies it; with create=false the chart renders no ServiceAccount and the annotation has no effect. Either set serviceAccount.create=true, or annotate your existing ServiceAccount out-of-band and remove it here." }}
{{- end }}
{{- if and .Values.storage.s3.existingSecret $roleArn }}
{{- fail "storage.s3.existingSecret and IRSA (serviceAccount.annotations \"eks.amazonaws.com/role-arn\") are mutually exclusive: static keys would shadow the web-identity credentials. Set exactly one credential mode." }}
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
