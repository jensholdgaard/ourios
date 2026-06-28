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
The image reference (repository:tag), defaulting the tag to the chart appVersion.
*/}}
{{- define "ourios.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}

{{/*
Storage-backend env (RFC 0019). For s3: the bucket + optional addressing;
credentials never appear here — they come from the AWS chain (aws.* →
envFrom/IRSA). For local: the in-container data root.
*/}}
{{- define "ourios.storageEnv" -}}
- name: OURIOS_STORAGE_BACKEND
  value: {{ .Values.storage.backend | quote }}
{{- if eq .Values.storage.backend "s3" }}
- name: OURIOS_S3_BUCKET
  value: {{ .Values.storage.s3.bucket | quote }}
{{- with .Values.storage.s3.endpoint }}
- name: OURIOS_S3_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- with .Values.storage.s3.region }}
- name: OURIOS_S3_REGION
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
Env common to every workload: the storage backend, the AWS chain region,
self-telemetry, and any extraEnv. Compaction is wired per-workload — only the
dedicated compactor sweeps; the receiver/querier set OURIOS_COMPACTION_ENABLED=0
(RFC 0009 §3.2) so a single sweeper avoids redundant per-interval listing.
*/}}
{{- define "ourios.commonEnv" -}}
{{ include "ourios.storageEnv" . }}
{{- with .Values.aws.region }}
- name: AWS_DEFAULT_REGION
  value: {{ . | quote }}
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
- name: OURIOS_COMPACTION_INTERVAL_SECS
  value: {{ .Values.compactor.intervalSecs | quote }}
{{- end }}

{{/*
AWS-credential envFrom: the Secret named by aws.existingSecret, if set. Empty
otherwise (IRSA / instance metadata supply credentials with no static keys).
*/}}
{{- define "ourios.awsEnvFrom" -}}
{{- with .Values.aws.existingSecret }}
envFrom:
  - secretRef:
      name: {{ . | quote }}
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
