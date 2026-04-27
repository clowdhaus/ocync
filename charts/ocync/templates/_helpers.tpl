{{/*
Cross-field validation. Called from `configmap.yaml` (which always renders)
so a misconfig fails the entire `helm template` / `helm install` invocation
rather than producing a silently-broken release.

Validates:
  - `mode` is one of watch | cronjob | job (typo => no workload renders)
  - `secretProviderClass.provider` is set when `secretProviderClass.enabled`
  - `workloadIdentity.provider` is one of none | aws | gcp | azure
*/}}
{{- define "ocync.validate" -}}
{{- $modes := list "watch" "cronjob" "job" -}}
{{- if not (has .Values.mode $modes) -}}
{{- fail (printf "ocync: invalid mode '%s' (must be one of: watch, cronjob, job)" (.Values.mode | toString)) -}}
{{- end -}}
{{- $wiProviders := list "none" "aws" "gcp" "azure" "" -}}
{{- $wiProvider := .Values.workloadIdentity.provider | default "none" -}}
{{- if not (has $wiProvider $wiProviders) -}}
{{- fail (printf "ocync: invalid workloadIdentity.provider '%s' (must be one of: none, aws, gcp, azure)" $wiProvider) -}}
{{- end -}}
{{- end }}

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

{{/*
Workload Identity ServiceAccount annotations.

Returns the cloud-specific annotation map for the configured provider.
Merge with `.Values.serviceAccount.annotations` at the call site.

aws   -> eks.amazonaws.com/role-arn
gcp   -> iam.gke.io/gcp-service-account
azure -> azure.workload.identity/client-id (+ tenant-id if set)
none  -> empty map
*/}}
{{- define "ocync.workloadIdentity.serviceAccountAnnotations" -}}
{{- $provider := .Values.workloadIdentity.provider | default "none" -}}
{{- if eq $provider "aws" -}}
{{- $roleArn := required "ocync: workloadIdentity.aws.roleArn is required when workloadIdentity.provider is 'aws'" .Values.workloadIdentity.aws.roleArn -}}
eks.amazonaws.com/role-arn: {{ $roleArn | quote }}
{{ end -}}
{{- if eq $provider "gcp" -}}
{{- $sa := required "ocync: workloadIdentity.gcp.serviceAccount is required when workloadIdentity.provider is 'gcp'" .Values.workloadIdentity.gcp.serviceAccount -}}
iam.gke.io/gcp-service-account: {{ $sa | quote }}
{{ end -}}
{{- if eq $provider "azure" -}}
{{- $clientId := required "ocync: workloadIdentity.azure.clientId is required when workloadIdentity.provider is 'azure'" .Values.workloadIdentity.azure.clientId -}}
azure.workload.identity/client-id: {{ $clientId | quote }}
{{ with .Values.workloadIdentity.azure.tenantId -}}
azure.workload.identity/tenant-id: {{ . | quote }}
{{ end -}}
{{- end -}}
{{- end }}

{{/*
Workload Identity pod labels.

Azure AD Workload Identity requires `azure.workload.identity/use: "true"`
on the pod for the mutating webhook to inject the projected SA token.
Other providers do not require pod-level labels.
*/}}
{{- define "ocync.workloadIdentity.podLabels" -}}
{{- if eq (.Values.workloadIdentity.provider | default "none") "azure" -}}
{{- $_ := required "ocync: workloadIdentity.azure.clientId is required when workloadIdentity.provider is 'azure'" .Values.workloadIdentity.azure.clientId -}}
azure.workload.identity/use: "true"
{{ end -}}
{{- end }}
