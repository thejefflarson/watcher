{{- define "watcher.labels" -}}
helm.sh/chart: {{ .Chart.Name }}-{{ .Chart.Version }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
app.kubernetes.io/part-of: watcher
{{- end }}

{{- define "watcher.siteUrl" -}}
https://{{ .Values.hostname }}
{{- end }}

{{/* Name of the Zalando-generated credential secret for the watcher DB user. */}}
{{- define "watcher.dbSecret" -}}
{{ .Values.postgres.user }}.{{ .Values.postgres.clusterName }}.credentials.postgresql.acid.zalan.do
{{- end }}
