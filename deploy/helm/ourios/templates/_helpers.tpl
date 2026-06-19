{{/*
Expand the name of the chart.
*/}}
{{- define "ourios.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
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
Image reference. Tag defaults to the chart appVersion when values.image.tag
is empty.
*/}}
{{- define "ourios.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end }}

{{/*
Port portion of the receiver gRPC bind address (OURIOS_RECEIVER_GRPC_ADDR).
Splits on ":" and takes the last field so the container/Service port can never
drift from the address the server actually binds.
*/}}
{{- define "ourios.grpcPort" -}}
{{- $port := (splitList ":" .Values.ourios.receiver.grpcAddr) | last -}}
{{- if not (regexMatch "^[0-9]+$" $port) -}}
{{- fail (printf "ourios.receiver.grpcAddr (%q) must end in a numeric port, e.g. 0.0.0.0:4317" .Values.ourios.receiver.grpcAddr) -}}
{{- end -}}
{{- $port -}}
{{- end }}

{{/*
Port portion of the receiver HTTP bind address (OURIOS_RECEIVER_HTTP_ADDR).
*/}}
{{- define "ourios.httpPort" -}}
{{- $port := (splitList ":" .Values.ourios.receiver.httpAddr) | last -}}
{{- if not (regexMatch "^[0-9]+$" $port) -}}
{{- fail (printf "ourios.receiver.httpAddr (%q) must end in a numeric port, e.g. 0.0.0.0:4318" .Values.ourios.receiver.httpAddr) -}}
{{- end -}}
{{- $port -}}
{{- end }}

{{/*
Name of the service account to use.
*/}}
{{- define "ourios.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ourios.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}
