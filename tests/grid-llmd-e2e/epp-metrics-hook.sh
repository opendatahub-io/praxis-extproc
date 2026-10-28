#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Authenticated, bounded EPP metrics scrape for signal qualification.
set -Eeuo pipefail
umask 077

CONTEXT=""
NAMESPACE=""
SERVICE_ACCOUNT=""
SERVICE_ACCOUNT_NAMESPACE=""
METRIC_NAME=""
POOL_LABEL="name"
EVIDENCE_DIR=""
PHASE="sample"
SERVICES=()

usage() {
  cat <<'USAGE'
Usage: epp-metrics-hook.sh --context CTX --namespace TASK_NS \
  --metrics-service-account NAME --metrics-service-account-namespace NS \
  --metric-name NAME [--pool-label LABEL] --service POOL=NS/SVC:PORT \
  --service POOL=NS/SVC:PORT --evidence-dir DIR --phase NAME
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --metrics-service-account) SERVICE_ACCOUNT="${2:?}"; shift 2 ;;
    --metrics-service-account-namespace) SERVICE_ACCOUNT_NAMESPACE="${2:?}"; shift 2 ;;
    --metric-name) METRIC_NAME="${2:?}"; shift 2 ;;
    --pool-label) POOL_LABEL="${2:?}"; shift 2 ;;
    --service) SERVICES+=("${2:?}"); shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --phase) PHASE="${2:?}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

for binary in oc jq curl awk python3; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ -n "$CONTEXT" && -n "$NAMESPACE" && -n "$SERVICE_ACCOUNT" && -n "$SERVICE_ACCOUNT_NAMESPACE" &&
   -n "$METRIC_NAME" && -n "$EVIDENCE_DIR" && -n "${EXPECTED_API_SERVER:-}" ]] || {
  printf 'missing required EPP metrics hook input\n' >&2
  exit 2
}
[[ "${#SERVICES[@]}" == 2 ]] || { printf 'exactly two EPP metrics Services are required\n' >&2; exit 2; }
[[ "$METRIC_NAME" =~ ^[a-zA-Z_:][a-zA-Z0-9_:]*$ && "$POOL_LABEL" =~ ^[a-zA-Z_][a-zA-Z0-9_]*$ ]] || {
  printf 'metric name or pool label is invalid\n' >&2
  exit 2
}
[[ "$PHASE" =~ ^[A-Za-z0-9_.-]+$ ]] || { printf 'unsafe phase name\n' >&2; exit 2; }

oc() { command oc --context="$CONTEXT" "$@"; }
observed_server="$(oc whoami --show-server)"
[[ "$observed_server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 2; }
[[ "$(oc get --raw=/readyz)" == *ok* ]] || { printf 'API /readyz failed\n' >&2; exit 2; }
insecure="$(oc config view --raw --minify -o json | jq -r '.clusters[0].cluster["insecure-skip-tls-verify"] // false')"
api_ca_verified=true
if [[ "$insecure" == true ]]; then
  [[ "${CLUSTER_ENV:-}" == ephemeral-dev && "${ALLOW_INSECURE_DEV_API:-false}" == true ]] || {
    printf 'insecure API TLS requires explicit ephemeral-dev opt-in\n' >&2
    exit 2
  }
  api_ca_verified=false
fi

namespace_json="$(oc get namespace "$NAMESPACE" -o json)"
run_label="$(jq -er '.metadata.labels["praxis.fast/qualification-run"]' <<<"$namespace_json")"
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

token="$(oc create token "$SERVICE_ACCOUNT" -n "$SERVICE_ACCOUNT_NAMESPACE" --duration=10m 2>/dev/null)" || {
  printf 'short-lived EPP metrics token request failed; no token was recorded\n' >&2
  exit 1
}
[[ -n "$token" ]] || { printf 'EPP metrics token request returned empty output\n' >&2; exit 1; }
declare -A values=()

for entry in "${SERVICES[@]}"; do
  pool="${entry%%=*}"
  service_ref="${entry#*=}"
  [[ "$entry" == *=* && "$pool" =~ ^[A-Za-z0-9._-]+$ ]] || { unset token; printf 'invalid pool/service mapping\n' >&2; exit 2; }
  [[ ! -v "values[$pool]" ]] || { unset token; printf 'duplicate EPP pool mapping\n' >&2; exit 2; }
  if [[ "$service_ref" =~ ^([a-z0-9-]+)/([a-z0-9-]+):([0-9]+)$ ]]; then
    service_namespace="${BASH_REMATCH[1]}"
    service_name="${BASH_REMATCH[2]}"
    remote_port="${BASH_REMATCH[3]}"
  else
    unset token
    printf 'EPP service must be namespace/name:port\n' >&2
    exit 2
  fi
  service_json="$(oc get service "$service_name" -n "$service_namespace" -o json)"
  jq -e --argjson port "$remote_port" 'any(.spec.ports[]?; .port==$port)' <<<"$service_json" >/dev/null || {
    unset token
    printf 'configured EPP metrics port is absent from the Service\n' >&2
    exit 2
  }
  local_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
  raw_file="$EVIDENCE_DIR/$pool-epp-metrics-$PHASE.prom"
  oc --context="$CONTEXT" port-forward -n "$service_namespace" "service/$service_name" "$local_port:$remote_port" \
    >"$EVIDENCE_DIR/$pool-port-forward-$PHASE.log" 2>&1 &
  forward_pid=$!
  success=false
  for _ in $(seq 1 30); do
    if printf 'header = "Authorization: Bearer %s"\n' "$token" | \
       curl --config - --silent --show-error --fail --connect-timeout 2 --max-time 5 \
         "http://127.0.0.1:$local_port/metrics" -o "$raw_file" 2>/dev/null; then
      success=true
      break
    fi
    sleep 1
  done
  kill "$forward_pid" 2>/dev/null || true
  wait "$forward_pid" 2>/dev/null || true
  [[ "$success" == true ]] || { unset token; printf 'authenticated EPP metrics scrape failed for %s\n' "$pool" >&2; exit 1; }
  chmod 0600 "$raw_file"
  sample="$(awk -v metric="$METRIC_NAME" -v label="$POOL_LABEL" -v pool="$pool" '
    {
      name=$1; sub(/\{.*/, "", name)
      if (name!=metric) next
      if (index($0,label "=\"" pool "\"")==0) next
      value=$2
      if (value !~ /^[-+]?[0-9]*\.?[0-9]+([eE][-+]?[0-9]+)?$/) next
      print value
      count++
    }
    END { if (count!=1) exit 1 }
  ' "$raw_file")" || { unset token; printf 'expected one finite %s sample labeled %s=%s\n' "$METRIC_NAME" "$POOL_LABEL" "$pool" >&2; exit 1; }
  values["$pool"]="$sample"
done
unset token

values_json='{}'
for pool in "${!values[@]}"; do
  values_json="$(jq --arg pool "$pool" --argjson value "${values[$pool]}" '. + {($pool):$value}' <<<"$values_json")"
done
jq -n --arg phase "$PHASE" --arg sa "$SERVICE_ACCOUNT" --arg saNamespace "$SERVICE_ACCOUNT_NAMESPACE" \
  --arg run "$run_label" --argjson apiCaVerified "$api_ca_verified" --argjson values "$values_json" \
  '{schema:"praxis-extproc.grid-llmd-epp-metrics/v1",phase:$phase,metricsScraperServiceAccount:($saNamespace+"/"+$sa),qualificationRun:$run,apiCaVerified:$apiCaVerified,values:($values|del(.services))}' \
  >"$EVIDENCE_DIR/epp-metrics-$PHASE.json"
chmod 0600 "$EVIDENCE_DIR/epp-metrics-$PHASE.json"
cat "$EVIDENCE_DIR/epp-metrics-$PHASE.json"
