{{/*
Pod template (metadata + spec) shared by Deployment, CronJob, and Job
workloads. Renders the body of `spec.template` (Deployment/Job) or
`spec.jobTemplate.spec.template` (CronJob).

Behavior is gated on `.Values.mode`:
  watch   -> args [watch, ...], ports + liveness/readiness probes,
             checksum/config annotation; no restartPolicy (defaults Always)
  cronjob -> args [sync, ...]; restartPolicy: OnFailure
  job     -> args [sync, ...]; restartPolicy: OnFailure

Pass the root context (`.`) and pipe through `nindent N` at the call
site to position the rendered block under the correct parent.
*/}}
{{- define "ocync.podTemplate" -}}
metadata:
  {{- if or (eq .Values.mode "watch") (not (empty .Values.podAnnotations)) }}
  annotations:
    {{- if eq .Values.mode "watch" }}
    checksum/config: {{ toYaml .Values.config | sha256sum }}
    {{- end }}
    {{- with .Values.podAnnotations }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- end }}
  labels:
    {{- include "ocync.selectorLabels" . | nindent 4 }}
    {{- with include "ocync.workloadIdentity.podLabels" . }}
    {{- . | nindent 4 }}
    {{- end }}
    {{- with .Values.podLabels }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
spec:
  {{- with .Values.imagePullSecrets }}
  imagePullSecrets:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  serviceAccountName: {{ include "ocync.serviceAccountName" . }}
  securityContext:
    {{- toYaml .Values.podSecurityContext | nindent 4 }}
  {{- if ne .Values.mode "watch" }}
  restartPolicy: OnFailure
  {{- end }}
  containers:
    - name: {{ .Chart.Name }}
      image: {{ include "ocync.image" . }}
      imagePullPolicy: {{ .Values.image.pullPolicy }}
      args:
        {{- if eq .Values.mode "watch" }}
        - watch
        - --config
        - /etc/ocync/config.yaml
        - --interval
        - {{ .Values.watch.interval | quote }}
        - --health-port
        - {{ .Values.watch.healthPort | quote }}
        {{- else }}
        - sync
        - --config
        - /etc/ocync/config.yaml
        {{- end }}
        {{- range .Values.extraArgs }}
        - {{ . | quote }}
        {{- end }}
      {{- if eq .Values.mode "watch" }}
      ports:
        - name: health
          containerPort: {{ .Values.watch.healthPort }}
          protocol: TCP
      livenessProbe:
        httpGet:
          path: /healthz
          port: health
      readinessProbe:
        httpGet:
          path: /readyz
          port: health
      {{- end }}
      {{- with .Values.env }}
      env:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      {{- with .Values.envFrom }}
      envFrom:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      securityContext:
        {{- toYaml .Values.securityContext | nindent 8 }}
      {{- with .Values.resources }}
      resources:
        {{- toYaml . | nindent 8 }}
      {{- end }}
      volumeMounts:
        - name: config
          mountPath: /etc/ocync
          readOnly: true
        - name: tmp
          mountPath: /tmp
        {{- with .Values.extraVolumeMounts }}
        {{- toYaml . | nindent 8 }}
        {{- end }}
  volumes:
    - name: config
      configMap:
        name: {{ include "ocync.fullname" . }}
    - name: tmp
      emptyDir: {}
    {{- with .Values.extraVolumes }}
    {{- toYaml . | nindent 4 }}
    {{- end }}
  {{- with .Values.nodeSelector }}
  nodeSelector:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .Values.tolerations }}
  tolerations:
    {{- toYaml . | nindent 4 }}
  {{- end }}
  {{- with .Values.affinity }}
  affinity:
    {{- toYaml . | nindent 4 }}
  {{- end }}
{{- end }}
