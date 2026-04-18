{{/*
Expand the name of the chart.
*/}}
{{- define "ocync.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "ocync.fullname" -}}
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
Common labels.
*/}}
{{- define "ocync.labels" -}}
helm.sh/chart: {{ printf "%s-%s" (include "ocync.name" .) .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "ocync.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels.
*/}}
{{- define "ocync.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ocync.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use.
*/}}
{{- define "ocync.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "ocync.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Container image with tag defaulting to appVersion-fips.
*/}}
{{- define "ocync.image" -}}
{{- $tag := default (printf "%s-fips" .Chart.AppVersion) .Values.image.tag }}
{{- printf "%s:%s" .Values.image.repository $tag }}
{{- end }}
