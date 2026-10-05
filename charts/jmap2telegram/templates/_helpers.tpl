{{- define "jmap2telegram.name" -}}
{{- .Chart.Name -}}
{{- end -}}

{{- /*
fullname is deliberately the bare release name, not the conventional
name+release concatenation. Do not "fix" this in passing: existing installs
already own Secret <fullname> and PVC <fullname>-data under this scheme,
and changing the convention would rename those resources on upgrade — Helm
would provision fresh ones instead of adopting the existing ones, orphaning
the bot's encrypted credential store (and the token secret) behind.
*/ -}}
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
