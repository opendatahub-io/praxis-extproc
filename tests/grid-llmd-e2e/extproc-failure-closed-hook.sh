#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Exercise Envoy's fail-closed ExtProc behavior using only this run's ExtProc.
set -Eeuo pipefail
umask 077

CONTEXT=""
NAMESPACE=""
EVIDENCE_DIR=""
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
DEPLOYMENT_NAME="${GRID_LLMD_EXTPROC_DEPLOYMENT:-praxis-extproc}"
SERVICE_NAME="${GRID_LLMD_EXTPROC_SERVICE:-praxis-extproc}"
ENVOYFILTER_NAME="${GRID_LLMD_EXTPROC_ENVOYFILTER:-praxis-grid-llmd-extproc}"
HTTPROUTE_NAME="${GRID_LLMD_HTTPROUTE_NAME:-grid-llmd-inference}"
GATEWAY_URL="${GRID_LLMD_GATEWAY_URL:-}"
GATEWAY_CONNECT_TO="${GRID_LLMD_GATEWAY_CONNECT_TO:-}"
GATEWAY_PATH="${GRID_LLMD_GATEWAY_PATH:-/v1/chat/completions}"
GATEWAY_CA_FILE="${GRID_LLMD_GATEWAY_CA_FILE:-}"
MODEL="${GRID_LLMD_MODEL:-}"
TIMEOUT_SECONDS="${GRID_LLMD_TIMEOUT_SECONDS:-120}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ORIGINAL_REPLICAS=""
RESTORE_NEEDED=false
RESTORE_FAILED=false
TEMP_DIR=""

usage() {
  cat <<'USAGE'
Usage: extproc-failure-closed-hook.sh --context CTX --namespace NS --evidence-dir DIR

Requires EXPECTED_API_SERVER, GRID_LLMD_GATEWAY_URL,
GRID_LLMD_GATEWAY_CA_FILE, and GRID_LLMD_MODEL. Optional GRID_LLMD_* resource
names override the task-owned defaults. GRID_LLMD_GATEWAY_CONNECT_TO may map
the verified route hostname to a load-balancer address while preserving TLS
SNI and hostname verification. Kubeconfig/API CA configuration is inherited
from the xtask runner and is never copied into evidence.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

for binary in oc jq python3 curl sha256sum; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ -n "$CONTEXT" && -n "$NAMESPACE" && -n "$EVIDENCE_DIR" && -n "$EXPECTED_API_SERVER" ]] || {
  printf 'context, namespace, evidence directory, and EXPECTED_API_SERVER are required\n' >&2
  exit 2
}
[[ -n "$GATEWAY_URL" && -n "$GATEWAY_CA_FILE" && -s "$GATEWAY_CA_FILE" && -n "$MODEL" ]] || {
  printf 'GRID_LLMD_GATEWAY_URL, readable GRID_LLMD_GATEWAY_CA_FILE, and GRID_LLMD_MODEL are required\n' >&2
  exit 2
}
[[ "$GATEWAY_URL" == https://* ]] || { printf 'Gateway URL must use HTTPS\n' >&2; exit 2; }
[[ "$GATEWAY_PATH" == /* && "$GATEWAY_PATH" != *'?'* && "$GATEWAY_PATH" != *'#'* ]] || {
  printf 'Gateway path must be an absolute path without query or fragment\n' >&2
  exit 2
}
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]{0,3}$ ]] || { printf 'timeout must be 1..9999 seconds\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/grid-llmd-fail-closed.XXXXXX")"
chmod 0700 "$TEMP_DIR"

oc() {
  command oc --context="$CONTEXT" --request-timeout=15s "$@"
}

log() {
  printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" >&2
}

restore_extproc() {
  local scale_result wait_result
  [[ "$RESTORE_NEEDED" == true ]] || return 0
  log "Restoring task-owned ExtProc Deployment replicas to $ORIGINAL_REPLICAS"
  set +e
  if scale_result="$(oc scale "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" --replicas="$ORIGINAL_REPLICAS" 2>&1)"; then
    printf '%s\n' "$scale_result" >"$EVIDENCE_DIR/restore-scale.txt"
  else
    RESTORE_FAILED=true
    printf '%s\n' "$scale_result" >"$EVIDENCE_DIR/restore-error.txt"
    set -e
    return 1
  fi
  if wait_result="$(oc wait "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" --for=condition=Available --timeout="${TIMEOUT_SECONDS}s" 2>&1)"; then
    printf '%s\n' "$wait_result" >"$EVIDENCE_DIR/restore-wait.txt"
  else
    RESTORE_FAILED=true
    printf '%s\n' "$wait_result" >"$EVIDENCE_DIR/restore-error.txt"
    set -e
    return 1
  fi
  set -e
  RESTORE_NEEDED=false
  jq -n --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson replicas "$ORIGINAL_REPLICAS" \
    '{restored:true,replicas:$replicas,observedAt:$observedAt}' >"$EVIDENCE_DIR/restore-result.json"
  return 0
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM HUP
  if [[ "$RESTORE_NEEDED" == true ]]; then
    restore_extproc || status=1
  fi
  if [[ "$RESTORE_FAILED" == true ]]; then status=1; fi
  if [[ -n "$TEMP_DIR" && -d "$TEMP_DIR" ]]; then
    rm -rf "$TEMP_DIR"
  fi
  if (( status != 0 )); then
    jq -n --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson restoreFailed "$RESTORE_FAILED" \
      '{status:"blocked",restoreFailed:$restoreFailed,observedAt:$observedAt}' >"$EVIDENCE_DIR/hook-result.json" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

CURRENT_SERVER="$(oc whoami --show-server)"
[[ "$CURRENT_SERVER" == "$EXPECTED_API_SERVER" ]] || { log 'API server mismatch'; exit 2; }
[[ "$(oc get --raw=/readyz)" == *ok* ]] || { log 'API /readyz failed'; exit 2; }
API_TLS_VERIFIED=true
INSECURE="$(oc config view --raw --minify -o json | jq -r '.clusters[0].cluster["insecure-skip-tls-verify"] // false')"
if [[ "$INSECURE" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
    log 'insecure API TLS rejected without ephemeral-dev opt-in'
    exit 2
  }
  API_TLS_VERIFIED=false
fi

NAMESPACE_JSON="$(oc get namespace "$NAMESPACE" -o json)"
RUN_LABEL="$(jq -er '.metadata.labels["praxis.fast/qualification-run"]' <<<"$NAMESPACE_JSON")"
DEPLOYMENT_JSON="$(oc get "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" -o json)"
SERVICE_JSON="$(oc get "service/$SERVICE_NAME" -n "$NAMESPACE" -o json)"
FILTER_JSON="$(oc get envoyfilter.networking.istio.io "$ENVOYFILTER_NAME" -n "$NAMESPACE" -o json)"
ROUTE_JSON="$(oc get httproute "$HTTPROUTE_NAME" -n "$NAMESPACE" -o json)"

jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  .spec.replicas==1 and .status.observedGeneration==.metadata.generation and
  (.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1 and
  ([.spec.template.spec.containers[]?|select(.name=="praxis-extproc")]|length)==1
' <<<"$DEPLOYMENT_JSON" >/dev/null || { log 'ExtProc Deployment is not a single ready run-owned replica'; exit 2; }
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  (.spec.selector["praxis.fast/qualification-run"]==$run) and
  .spec.selector["app.kubernetes.io/name"]=="praxis-extproc"
' <<<"$SERVICE_JSON" >/dev/null || { log 'ExtProc Service ownership/selector gate failed'; exit 2; }
jq -e --argjson deploymentSelector "$(jq -c '.spec.selector.matchLabels' <<<"$DEPLOYMENT_JSON")" \
  '.spec.selector==$deploymentSelector' <<<"$SERVICE_JSON" >/dev/null || {
  log 'ExtProc Service selector differs from the run-owned Deployment selector'
  exit 2
}
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  ([.spec.configPatches[]?|select(.applyTo=="HTTP_FILTER")|.patch.value.typed_config.failure_mode_allow]|length)>0 and
  all(.spec.configPatches[]?|select(.applyTo=="HTTP_FILTER");.patch.value.typed_config.failure_mode_allow==false) and
  any(.spec.configPatches[]?|select(.applyTo=="CLUSTER");.patch.value.name=="praxis-extproc-grpc")
' <<<"$FILTER_JSON" >/dev/null || { log 'ExtProc EnvoyFilter is not task-owned and fail-closed'; exit 2; }
jq -e --arg run "$RUN_LABEL" --arg ns "$NAMESPACE" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  any(.status.parents[]?.conditions[]?;.type=="Accepted" and .status=="True") and
  any(.status.parents[]?.conditions[]?;.type=="ResolvedRefs" and .status=="True") and
  ([.spec.parentRefs[]?|select((.kind//"Gateway")=="Gateway" and (.namespace//$ns)==$ns)]|length)==1 and
  ([.spec.rules[]?.backendRefs[]?]|length)==1 and
  all(.spec.rules[]?.backendRefs[]?;.kind==null or .kind=="Service")
' <<<"$ROUTE_JSON" >/dev/null || { log 'task HTTPRoute is not accepted, resolved, and single-backend'; exit 2; }

GATEWAY_NAME="$(jq -er '.spec.parentRefs|map(select((.kind//"Gateway")=="Gateway"))|if length==1 then .[0].name else error("expected one Gateway parent") end' <<<"$ROUTE_JSON")"
GATEWAY_HOST="$(jq -er '.spec.hostnames|if length==1 then .[0] else error("expected one hostname") end' <<<"$ROUTE_JSON")"
GATEWAY_JSON="$(oc get gateway "$GATEWAY_NAME" -n "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  .metadata.generation as $g |
  all(.status.conditions[]?|select(.type=="Accepted" or .type=="Programmed");.status=="True" and .observedGeneration==$g)
' <<<"$GATEWAY_JSON" >/dev/null || { log 'task Gateway is not run-owned, Accepted, and Programmed'; exit 2; }
URL_AUTHORITY="${GATEWAY_URL#https://}"
URL_AUTHORITY="${URL_AUTHORITY%%/*}"
[[ "$URL_AUTHORITY" =~ ^([A-Za-z0-9.-]+)(:([1-9][0-9]{0,4}))?$ ]] || { log 'Gateway URL authority is invalid'; exit 2; }
URL_HOST="${BASH_REMATCH[1]}"
URL_PORT="${BASH_REMATCH[3]:-443}"
[[ "$URL_HOST" == "$GATEWAY_HOST" ]] || { log 'Gateway URL hostname does not match the task HTTPRoute'; exit 2; }
((URL_PORT <= 65535)) || { log 'Gateway URL port is invalid'; exit 2; }
CONNECT_ARGS=()
if [[ -n "$GATEWAY_CONNECT_TO" ]]; then
  [[ "$GATEWAY_CONNECT_TO" =~ ^([A-Za-z0-9.-]+):([1-9][0-9]{0,4}):([A-Za-z0-9.-]+):([1-9][0-9]{0,4})$ ]] || {
    log 'Gateway connect-to mapping is invalid'; exit 2;
  }
  CONNECT_URL_HOST="${BASH_REMATCH[1]}"
  CONNECT_URL_PORT="${BASH_REMATCH[2]}"
  CONNECT_HOST="${BASH_REMATCH[3]}"
  CONNECT_PORT="${BASH_REMATCH[4]}"
  ((CONNECT_URL_PORT <= 65535 && CONNECT_PORT <= 65535)) || { log 'Gateway connect-to port is invalid'; exit 2; }
  [[ "$CONNECT_URL_HOST" == "$URL_HOST" && "$CONNECT_URL_PORT" == "$URL_PORT" ]] || {
    log 'Gateway connect-to source host/port does not match the verified URL authority'; exit 2;
  }
  CONNECT_ARGS=(--connect-to "${CONNECT_URL_HOST}:${CONNECT_URL_PORT}:${CONNECT_HOST}:${CONNECT_PORT}")
fi
BACKEND_NAME="$(jq -er '.spec.rules[0].backendRefs[0].name' <<<"$ROUTE_JSON")"
BACKEND_NAMESPACE="$(jq -r --arg ns "$NAMESPACE" '.spec.rules[0].backendRefs[0].namespace//$ns' <<<"$ROUTE_JSON")"
[[ "$BACKEND_NAMESPACE" == "$NAMESPACE" ]] || { log 'HTTPRoute backend Service is outside the task namespace'; exit 2; }
BACKEND_CLUSTER="$BACKEND_NAME.$BACKEND_NAMESPACE.svc.cluster.local"
EXTPROC_CLUSTER="praxis-extproc-grpc"
BACKEND_SERVICE_JSON="$(oc get service "$BACKEND_NAME" -n "$BACKEND_NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
  <<<"$BACKEND_SERVICE_JSON" >/dev/null || { log 'HTTPRoute backend Service is not owned by this run'; exit 2; }
jq -e --arg gateway "$GATEWAY_NAME" '.spec.workloadSelector.labels["gateway.networking.k8s.io/gateway-name"]==$gateway' \
  <<<"$FILTER_JSON" >/dev/null || { log 'ExtProc EnvoyFilter does not target this task Gateway'; exit 2; }
FILTER_CLUSTER_SERVICE="$(jq -r '[.spec.configPatches[]?|select(.applyTo=="CLUSTER" and .patch.value.name=="praxis-extproc-grpc")|.patch.value.load_assignment.endpoints[]?.lb_endpoints[]?.endpoint.address.socket_address.address][0] // empty' <<<"$FILTER_JSON")"
[[ "$FILTER_CLUSTER_SERVICE" == "$SERVICE_NAME.$NAMESPACE.svc.cluster.local" ]] || {
  log 'ExtProc Envoy cluster does not target the task-owned Service'
  exit 2
}

GATEWAY_PODS="$(oc get pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$GATEWAY_NAME" -o json)"
PROXY_POD="$(jq -er '[.items[]|select(.status.phase=="Running" and any(.status.conditions[]?;.type=="Ready" and .status=="True"))]|if length==1 then .[0].metadata.name else error("expected exactly one Ready task Gateway proxy") end' <<<"$GATEWAY_PODS")"
PROXY_UID="$(jq -er --arg pod "$PROXY_POD" '.items[]|select(.metadata.name==$pod)|.metadata.uid' <<<"$GATEWAY_PODS")"
jq -n --arg run "$RUN_LABEL" --arg ns "$NAMESPACE" --arg deployment "$DEPLOYMENT_NAME" \
  --arg service "$SERVICE_NAME" --arg filter "$ENVOYFILTER_NAME" --arg gateway "$GATEWAY_NAME" \
  --arg route "$HTTPROUTE_NAME" --arg proxyPod "$PROXY_POD" --arg proxyUid "$PROXY_UID" \
  --arg gatewayUid "$(jq -r '.metadata.uid' <<<"$GATEWAY_JSON")" \
  --arg backendServiceUid "$(jq -r '.metadata.uid' <<<"$BACKEND_SERVICE_JSON")" \
  --arg backend "$BACKEND_CLUSTER" --arg extproc "$EXTPROC_CLUSTER" --arg api "$CURRENT_SERVER" \
  --arg gatewayHost "$GATEWAY_HOST" --arg gatewayConnectTo "$GATEWAY_CONNECT_TO" \
  --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson apiTlsVerified "$API_TLS_VERIFIED" \
  '{taskOwned:true,runLabel:$run,namespace:$ns,deployment:$deployment,service:$service,envoyFilter:$filter,gateway:$gateway,gatewayHost:$gatewayHost,gatewayConnectTo:(if $gatewayConnectTo=="" then null else $gatewayConnectTo end),gatewayUid:$gatewayUid,httpRoute:$route,backendServiceUid:$backendServiceUid,proxyPod:$proxyPod,proxyPodUid:$proxyUid,backendCluster:$backend,extprocCluster:$extproc,apiServer:$api,apiCaVerified:$apiTlsVerified,observedAt:$observedAt}' \
  >"$EVIDENCE_DIR/precondition.json"
jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,selector:.spec.selector,containers:[.spec.template.spec.containers[]?|{name,image}]},status:{observedGeneration:.status.observedGeneration,readyReplicas:(.status.readyReplicas//0),availableReplicas:(.status.availableReplicas//0)}}' \
  <<<"$DEPLOYMENT_JSON" >"$EVIDENCE_DIR/extproc-deployment-before.json"

read_proxy_stats() {
  oc exec "$PROXY_POD" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET "/clusters?format=json" \
    | python3 -c '
import json
import sys

backend, extproc = sys.argv[1:]
document = json.load(sys.stdin)
parsed = {"backendRequests": {}, "extprocCluster": {}, "extprocFilterFailures": {}}

def collect(fragment, selected_names):
    matches = [item for item in document.get("cluster_statuses", []) if fragment in item.get("name", "")]
    if len(matches) != 1:
        raise SystemExit("Envoy /clusters did not expose exactly one matching task cluster")
    cluster = matches[0]
    values = {}
    for host_index, host in enumerate(cluster.get("host_statuses", [])):
        for stat in host.get("stats", []):
            if stat.get("name") in selected_names and "value" in stat:
                key = "{}::host-{}::{}".format(cluster.get("name"), host_index, stat.get("name"))
                values[key] = int(stat["value"])
    return values

parsed["backendRequests"] = collect(backend, {"rq_total"})
parsed["extprocCluster"] = collect(extproc, {"cx_connect_fail", "rq_error", "rq_timeout", "rq_total"})
print(json.dumps(parsed, sort_keys=True, separators=(",", ":")))
    ' "$BACKEND_CLUSTER" "$EXTPROC_CLUSTER"
}

parse_endpoints_ready() {
  local endpoint_json
  endpoint_json="$(oc get endpointslice -n "$NAMESPACE" -l "kubernetes.io/service-name=$SERVICE_NAME" -o json)"
  jq '[.items[].endpoints[]?|select(.conditions.ready==true)]|length' <<<"$endpoint_json"
}

BEFORE_STATS="$(read_proxy_stats)"
jq -e '.backendRequests|type=="object" and length>0' <<<"$BEFORE_STATS" >/dev/null || {
  log 'no task Gateway backend upstream_rq_total counter is available for a zero-receipt proof'
  exit 2
}
READY_ENDPOINTS_BEFORE="$(parse_endpoints_ready)"
[[ "$READY_ENDPOINTS_BEFORE" == 1 ]] || { log 'ExtProc must have exactly one Ready endpoint before failure injection'; exit 2; }
ORIGINAL_REPLICAS="$(jq -r '.spec.replicas' <<<"$DEPLOYMENT_JSON")"
RESTORE_NEEDED=true
log 'Scaling only the run-owned Praxis ExtProc Deployment to zero for one failure-closed request'
oc scale "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" --replicas=0 >"$EVIDENCE_DIR/scale-down.txt"

deadline=$((SECONDS + TIMEOUT_SECONDS))
while ((SECONDS < deadline)); do
  current="$(oc get "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" -o json | jq -r '.spec.replicas')"
  ready_endpoints="$(parse_endpoints_ready)"
  if [[ "$current" == 0 && "$ready_endpoints" == 0 ]]; then break; fi
  sleep 2
done
[[ "${current:-1}" == 0 && "${ready_endpoints:-1}" == 0 ]] || { log 'ExtProc endpoint did not become unavailable before timeout'; exit 1; }
jq -n --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson replicas "$current" --argjson readyEndpoints "$ready_endpoints" \
  '{deploymentReplicas:$replicas,readyServiceEndpoints:$readyEndpoints,observedAt:$observedAt}' \
  >"$EVIDENCE_DIR/extproc-unavailable.json"

REQUEST_ID="$(python3 -c 'import uuid; print(uuid.uuid4())')"
REQUEST_URL="${GATEWAY_URL%/}${GATEWAY_PATH}"
BODY="$(jq -cn --arg model "$MODEL" --arg id "$REQUEST_ID" '{model:$model,messages:[{role:"user",content:("isolated failure-closed probe "+$id)}],stream:false}')"
printf '%s\n' "$BODY" >"$TEMP_DIR/request.json"
chmod 0600 "$TEMP_DIR/request.json"
REQUEST_STARTED="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
set +e
HTTP_STATUS="$(curl --silent --show-error --connect-timeout 10 --max-time "$TIMEOUT_SECONDS" \
  "${CONNECT_ARGS[@]}" --cacert "$GATEWAY_CA_FILE" --request POST --header 'content-type: application/json' \
  --header "x-request-id: $REQUEST_ID" --data-binary "@$TEMP_DIR/request.json" \
  --dump-header "$TEMP_DIR/response-headers.txt" --output /dev/null \
  --write-out '%{http_code}' "$REQUEST_URL" 2>"$TEMP_DIR/curl-error.txt")"
CURL_STATUS=$?
set -e
[[ "$CURL_STATUS" == 0 ]] || { sed -E 's/(authorization|token):.*/\1: [REDACTED]/I' "$TEMP_DIR/curl-error.txt" >&2; log 'Gateway request failed before an HTTP response'; exit 1; }
[[ "$HTTP_STATUS" =~ ^[0-9]{3}$ ]] || { log 'Gateway returned no parseable HTTP status'; exit 1; }
RESPONSE_REQUEST_ID="$(awk 'BEGIN{IGNORECASE=1} /^x-request-id:/ {sub(/^[^:]*:[[:space:]]*/,""); sub(/\r$/,""); value=$0} END{print value}' "$TEMP_DIR/response-headers.txt")"
[[ "$RESPONSE_REQUEST_ID" == "$REQUEST_ID" ]] || { log 'response did not echo the unique request ID'; exit 1; }
REQUEST_FINISHED="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

AFTER_STATS="$(read_proxy_stats)"
printf '%s\n' "$BEFORE_STATS" | jq -S . >"$EVIDENCE_DIR/proxy-counters-before.json"
printf '%s\n' "$AFTER_STATS" | jq -S . >"$EVIDENCE_DIR/proxy-counters-after.json"
BACKEND_REQUESTS="$(jq -n --argjson before "$BEFORE_STATS" --argjson after "$AFTER_STATS" '
  ([($before.backendRequests//{})[]]|add//0) as $b |
  ([($after.backendRequests//{})[]]|add//0) as $a | $a-$b
')"
EXTPROC_FAILURE_OBSERVED="$(jq -n --argjson before "$BEFORE_STATS" --argjson after "$AFTER_STATS" '
  def changedFailure($old;$new):
    ($new|to_entries|map(select(.value > ($old[.key]//0)))|length)>0;
  changedFailure(($before.extprocFilterFailures//{});($after.extprocFilterFailures//{})) or
  changedFailure(($before.extprocCluster//{});($after.extprocCluster//{}))
')"
[[ "$HTTP_STATUS" -ge 500 && "$EXTPROC_FAILURE_OBSERVED" == true && "$BACKEND_REQUESTS" == 0 ]] || {
  log "failure-closed assertion failed: status=$HTTP_STATUS extprocFailureObserved=$EXTPROC_FAILURE_OBSERVED backendRequestCount=$BACKEND_REQUESTS"
  exit 1
}

jq -n --arg schema "praxis-extproc.grid-llmd-failure-closed/v1" --arg resource "deployment/$DEPLOYMENT_NAME" \
  --arg requestId "$REQUEST_ID" --arg started "$REQUEST_STARTED" --arg finished "$REQUEST_FINISHED" \
  --argjson apiCaVerified "$API_TLS_VERIFIED" --argjson status "$HTTP_STATUS" \
  --argjson extprocFailureObserved "$EXTPROC_FAILURE_OBSERVED" --argjson backendRequestCount "$BACKEND_REQUESTS" \
  '{schema:$schema,apiCaVerified:$apiCaVerified,taskOwned:true,resource:$resource,requestId:$requestId,requestStatusCode:$status,extprocFailureObserved:$extprocFailureObserved,failureClosed:($status>=500 and $extprocFailureObserved and $backendRequestCount==0),backendRequestCount:$backendRequestCount,requestStarted:$started,requestFinished:$finished,responseRequestIdMatched:true}' \
  >"$EVIDENCE_DIR/failure-closed-report.json"
chmod 0600 "$EVIDENCE_DIR/failure-closed-report.json"

restore_extproc
RESTORE_NEEDED=false
RESTORED_DEPLOYMENT="$(oc get "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" --argjson replicas "$ORIGINAL_REPLICAS" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and .spec.replicas==$replicas and
  .status.observedGeneration==.metadata.generation and (.status.readyReplicas//0)==$replicas and
  (.status.availableReplicas//0)==$replicas
' <<<"$RESTORED_DEPLOYMENT" >/dev/null || { log 'ExtProc did not return to the original ready state'; exit 1; }
jq -n --arg requestId "$REQUEST_ID" --arg requestStarted "$REQUEST_STARTED" --arg requestFinished "$REQUEST_FINISHED" \
  --argjson status "$HTTP_STATUS" --argjson extprocFailureObserved "$EXTPROC_FAILURE_OBSERVED" \
  --argjson backendRequestCount "$BACKEND_REQUESTS" --argjson apiCaVerified "$API_TLS_VERIFIED" \
  --arg resource "deployment/$DEPLOYMENT_NAME" \
  '{schema:"praxis-extproc.grid-llmd-failure-closed/v1",apiCaVerified:$apiCaVerified,taskOwned:true,resource:$resource,requestId:$requestId,requestStatusCode:$status,extprocFailureObserved:$extprocFailureObserved,failureClosed:($status>=500 and $extprocFailureObserved and $backendRequestCount==0),backendRequestCount:$backendRequestCount,requestStarted:$requestStarted,requestFinished:$requestFinished}' \
  | tee "$EVIDENCE_DIR/hook-result.json"
chmod 0600 "$EVIDENCE_DIR/hook-result.json"
