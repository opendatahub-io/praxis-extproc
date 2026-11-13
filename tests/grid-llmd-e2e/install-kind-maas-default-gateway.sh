#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT="${KUBE_CONTEXT:-}"
KUBECONFIG_FILE="${KUBECONFIG:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
RUN_ID="${E2E_RUN_ID:-}"
NAMESPACE="${KIND_MAAS_GATEWAY_NAMESPACE:-maas-system}"
EVIDENCE_DIR="${KIND_MAAS_GATEWAY_EVIDENCE_DIR:-}"
TIMEOUT_SECONDS="${KIND_MAAS_GATEWAY_TIMEOUT_SECONDS:-120}"

for binary in kubectl jq python3; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_SERVER" && -n "$RUN_ID" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, KUBECONFIG, EXPECTED_API_SERVER, E2E_RUN_ID, and evidence directory are required\n' >&2
  exit 2
}
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || { printf 'timeout must be a positive integer\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists; refusing overwrite\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" --request-timeout=20s "$@"; }
on_error() {
  local code="$1"
  trap - ERR
  k get gateway maas-default-gateway -n "$NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},status:.status}' \
      >"$EVIDENCE_DIR/gateway-on-failure.json" 2>&1 || true
  k get service maas-default-gateway maas-default-gateway-istio -n "$NAMESPACE" -o json \
    | jq '[.items[]|{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},spec:{selector:.spec.selector,ports:.spec.ports}}]' \
      >"$EVIDENCE_DIR/services-on-failure.json" 2>&1 || true
  k get endpointslices -n "$NAMESPACE" -l gateway.networking.k8s.io/gateway-name=maas-default-gateway -o json \
    | jq '[.items[]|{name:.metadata.name,labels:.metadata.labels,ports:.ports,endpoints:[.endpoints[]?|{addresses,conditions,targetRef}]}]' \
      >"$EVIDENCE_DIR/endpointslices-on-failure.json" 2>&1 || true
  k get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json \
    | jq '[.items[-60:][]|{reason:.reason,message:.message,object:.involvedObject,lastTimestamp:.lastTimestamp}]' \
      >"$EVIDENCE_DIR/events-on-failure.json" 2>&1 || true
  jq -n --arg run "$RUN_ID" --argjson exitCode "$code" \
    '{status:"failed",run:$run,exitCode:$exitCode,secretValuesRecorded:false}' >"$EVIDENCE_DIR/RESULT.json"
  exit "$code"
}
trap 'on_error $?' ERR

server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_SERVER" ]] || { printf 'Kubernetes API server identity mismatch\n' >&2; exit 1; }
[[ "$(k get --raw=/readyz)" == *ok* ]] || { printf 'Kubernetes API is not ready\n' >&2; exit 1; }
k get gateway maas-default-gateway -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gateway.json"
k get service maas-default-gateway-istio -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/istio-service.json"
k get endpointslices -n "$NAMESPACE" -l gateway.networking.k8s.io/gateway-name=maas-default-gateway -o json >"$EVIDENCE_DIR/istio-endpointslices.json"
python3 "$SCRIPT_DIR/render_kind_maas_default_gateway_service.py" \
  --gateway "$EVIDENCE_DIR/gateway.json" \
  --istio-service "$EVIDENCE_DIR/istio-service.json" \
  --endpointslices "$EVIDENCE_DIR/istio-endpointslices.json" \
  --run-id "$RUN_ID" --output "$EVIDENCE_DIR/alias-service.yaml" >"$EVIDENCE_DIR/render.txt"

existing_status=0
k get service maas-default-gateway -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/existing-alias.json" 2>/dev/null || existing_status=$?
if ((existing_status == 0)); then
  expected_uid="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/gateway.json")"
  jq -e --arg run "$RUN_ID" --arg uid "$expected_uid" \
    '.metadata.labels["praxis.kind/run"]==$run and any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name=="maas-default-gateway" and .uid==$uid)' \
    "$EVIDENCE_DIR/existing-alias.json" >/dev/null || {
    printf 'existing MaaS alias Service is not owned by this run and current Gateway UID\n' >&2
    exit 1
  }
fi
k apply --dry-run=server -f "$EVIDENCE_DIR/alias-service.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
k apply -f "$EVIDENCE_DIR/alias-service.yaml" >"$EVIDENCE_DIR/apply.txt"

deadline=$((SECONDS + TIMEOUT_SECONDS))
ready=false
while (( SECONDS < deadline )); do
  k get endpointslices -n "$NAMESPACE" -l kubernetes.io/service-name=maas-default-gateway -o json >"$EVIDENCE_DIR/alias-endpointslices-live.json"
  if jq -e 'any(.items[]?;.ports|any(.[]?;.port==80)) and any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' \
      "$EVIDENCE_DIR/alias-endpointslices-live.json" >/dev/null; then
    ready=true
    break
  fi
  sleep 2
done
[[ "$ready" == true ]] || { printf 'MaaS Gateway alias has no Ready endpoint within %ss\n' "$TIMEOUT_SECONDS" >&2; exit 1; }
k get service maas-default-gateway -n "$NAMESPACE" -o json \
  | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},type:.spec.type,selector:.spec.selector,ports:.spec.ports}' \
  >"$EVIDENCE_DIR/alias-service-live.json"
jq '{items:[.items[]|{name:.metadata.name,ports:.ports,endpoints:[.endpoints[]?|{addresses,conditions,targetRef}]}]}' \
  "$EVIDENCE_DIR/alias-endpointslices-live.json" >"$EVIDENCE_DIR/alias-endpoints-summary.json"
jq -n --arg run "$RUN_ID" --arg gatewayUID "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/gateway.json")" \
  '{status:"PASS",run:$run,gatewayUID:$gatewayUID,aliasService:"maas-system/maas-default-gateway",gatewayUIDOwnerReference:true,readyEndpoint:true,secretValuesRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'MaaS API Gateway service alias passed current Gateway UID and Ready endpoint checks\n'
