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
The container environment for the ourios-server process. The compactor is
always on; the receiver and querier are toggled by .Values.roles, and each
role's address/knob env is only emitted when that role is enabled.
*/}}
{{- define "ourios.env" -}}
- name: OURIOS_BUCKET_ROOT
  value: {{ .Values.dataDir | quote }}
- name: OURIOS_COMPACTION_INTERVAL_SECS
  value: {{ .Values.compaction.intervalSecs | quote }}
{{- if .Values.roles.receiver.enabled }}
- name: OURIOS_RECEIVER_ENABLED
  value: "true"
- name: OURIOS_RECEIVER_GRPC_ADDR
  value: "0.0.0.0:4317"
- name: OURIOS_RECEIVER_HTTP_ADDR
  value: "0.0.0.0:4318"
- name: OURIOS_WAL_ROOT
  value: {{ .Values.walDir | quote }}
{{- end }}
{{- if .Values.roles.querier.enabled }}
- name: OURIOS_QUERIER_ENABLED
  value: "true"
- name: OURIOS_QUERIER_HTTP_ADDR
  value: "0.0.0.0:4319"
- name: OURIOS_QUERIER_DEFAULT_WINDOW_SECS
  value: {{ .Values.querier.defaultWindowSecs | quote }}
{{- end }}
{{- with .Values.otel.exporterEndpoint }}
- name: OTEL_EXPORTER_OTLP_ENDPOINT
  value: {{ . | quote }}
{{- end }}
{{- with .Values.extraEnv }}
{{- toYaml . | nindent 0 }}
{{- end }}
{{- end }}
