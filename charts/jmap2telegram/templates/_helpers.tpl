{{- define "jmap2telegram.name" -}}
{{- .Chart.Name -}}
{{- end -}}

{{- define "jmap2telegram.fullname" -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "jmap2telegram.labels" -}}
app.kubernetes.io/name: {{ include "jmap2telegram.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "jmap2telegram.selectorLabels" -}}
app.kubernetes.io/name: {{ include "jmap2telegram.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "jmap2telegram.secretName" -}}
{{- if .Values.telegram.existingSecret -}}
{{ .Values.telegram.existingSecret }}
{{- else -}}
{{ include "jmap2telegram.fullname" . }}
{{- end -}}
{{- end -}}

{{- define "jmap2telegram.pvcName" -}}
{{- if .Values.persistence.existingClaim -}}
{{ .Values.persistence.existingClaim }}
{{- else -}}
{{ include "jmap2telegram.fullname" . }}-data
{{- end -}}
{{- end -}}
