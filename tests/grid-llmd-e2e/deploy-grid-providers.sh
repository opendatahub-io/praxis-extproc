#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Register two task-owned llm-d-sim pools with the installed downstream Grid CRD.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${GRID_PROVIDERS_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
RESOURCE_PROFILE="${RESOURCE_PROFILE:-strict}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
GRID_NETWORK_NAME="${GRID_NETWORK_NAME:-grid-llmd-e2e}"
GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"
GRID_OPERATOR_SOURCE_REVISION="${GRID_OPERATOR_SOURCE_REVISION:-}"
GRID_OPERATOR_IMAGE="${GRID_OPERATOR_IMAGE:-}"
GRID_OPERATOR_NAMESPACE="${GRID_OPERATOR_NAMESPACE:-praxis-grid-operator-system}"
GRID_OPERATOR_RELEASE_NAME="${GRID_OPERATOR_RELEASE_NAME:-grid-operator}"
GRID_OPERATOR_DEPLOYMENT_NAME="${GRID_OPERATOR_DEPLOYMENT_NAME:-grid-operator}"
GRID_SOURCE_REVISION="${GRID_SOURCE_REVISION:-}"
EPP_SOURCE_REVISION="${EPP_SOURCE_REVISION:-}"
SIMULATOR_SOURCE_REVISION="${SIMULATOR_SOURCE_REVISION:-}"
EXTPROC_SOURCE_REVISION="${EXTPROC_SOURCE_REVISION:-}"
EPP_IMAGE="${EPP_IMAGE:-}"
SIMULATOR_IMAGE="${SIMULATOR_IMAGE:-}"
EXTPROC_IMAGE="${EXTPROC_IMAGE:-}"
POOL_A="${POOL_A_NAME:-pool-a}"
POOL_B="${POOL_B_NAME:-pool-b}"
POOL_A_GATEWAY="${POOL_A_GATEWAY_NAME:-grid-llmd-pool-a}"
POOL_B_GATEWAY="${POOL_B_GATEWAY_NAME:-grid-llmd-pool-b}"
MODEL_NAME="${SIMULATOR_MODEL_NAME:-grid-test-model}"
PROVIDER_PREFIX="${GRID_PROVIDER_PREFIX:-}"
EPP_QUEUE_METRIC="${EPP_QUEUE_METRIC:-llm_d_epp_average_queue_size}"
EPP_KV_CACHE_METRIC="${EPP_KV_CACHE_METRIC:-llm_d_epp_average_kv_cache_utilization}"
EPP_READY_METRIC="${EPP_READY_METRIC:-llm_d_epp_ready_endpoints}"
GRID_SIGNAL_METRIC="${GRID_SIGNAL_METRIC:-$EPP_QUEUE_METRIC}"
QUEUE_CAPACITY="${GRID_EPP_QUEUE_CAPACITY:-4}"
STALE_METRICS_SECONDS="${GRID_EPP_STALE_METRICS_SECONDS:-20}"
WAIT_TIMEOUT="${GRID_PROVIDER_WAIT_TIMEOUT:-900s}"
EVIDENCE_DIR="${GRID_PROVIDERS_EVIDENCE_DIR:-}"
GRID_RESOURCE=""
GRID_GROUP=""
GRID_VERSION=""
GRID_PLURAL=""
ROUTING_GATEWAY=""
ROUTING_GATEWAY_NAMESPACE=""
TASK_SITE=""
METRICS_SCRAPER_SERVICE_ACCOUNT=""
CURRENT_PHASE=initialization
PORT_FORWARD_PIDS=()

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Grid provider-registration plan (no cluster calls):
  1. Verify the expected API/context and task namespace ownership.
  2. Discover the installed inferenceproviders CRD group, served version, scope, and OpenAPI fields.
  3. Read the task GridNetwork/GridSite, each pool Gateway and Gateway-created Service, and each EPP metrics Service.
  4. Read the authenticated EPP Prometheus endpoint through a context-pinned port-forward and require queue, KV-cache, and ready-endpoint samples for each pool.
  5. Render one run-owned InferenceProvider per pool, using its pool Gateway Service as the inference endpoint and its EPP metrics Service as metricsConfig.endpoint.
  6. Server-side dry-run both providers, apply only after exact ownership checks, and wait for current-generation provider readiness.
  7. Require a current Distributed GridNetwork routing map with exactly both pool/model candidates and emit the endpoint inventory used by the ODH operator's Praxis config generator.

EPP metrics require bearer authentication. The helper uses a short-lived
TokenRequest for the downstream Grid operator's dedicated metrics-scraper
ServiceAccount, passes it only through curl stdin, and never writes or logs the
token. HTTP bearer transport is allowed only for the explicit reduced-dev
profile on an ephemeral development cluster; strict runs must use HTTPS.

Modes: plan, apply, verify, cleanup. The resource profile is strict by default;
reduced-dev requires CLUSTER_ENV=ephemeral-dev. API TLS bypass additionally
requires ALLOW_INSECURE_DEV_API=true. This helper never sends inference traffic.
PLAN
  exit 0
fi

for bin in oc jq awk sha256sum curl python3; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
[[ "$MODE" =~ ^(apply|verify|cleanup)$ ]] || { printf 'unsupported mode: %s\n' "$MODE" >&2; exit 2; }
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" ]] || { printf 'EXPECTED_API_SERVER and KUBE_CONTEXT are required\n' >&2; exit 2; }
[[ "$RESOURCE_PROFILE" == strict || "$RESOURCE_PROFILE" == reduced-dev ]] || { printf 'RESOURCE_PROFILE must be strict or reduced-dev\n' >&2; exit 2; }
if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev ]] || { printf 'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev\n' >&2; exit 2; }
fi
[[ -n "$RUN_LABEL" && "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || {
  printf 'GRID_RUN_LABEL must be a valid non-empty Kubernetes label value\n' >&2; exit 2;
}
[[ "$GRID_OPERATOR_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ && "$GRID_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ && \
   "$EPP_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ && "$SIMULATOR_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || {
  printf 'Grid operator, Grid, EPP, and simulator source revisions must be full Git SHAs\n' >&2; exit 2;
}
[[ "$GRID_OPERATOR_IMAGE" == *@sha256:* && "$EPP_IMAGE" == *@sha256:* && "$SIMULATOR_IMAGE" == *@sha256:* ]] || {
  printf 'GRID_OPERATOR_IMAGE, EPP_IMAGE, and SIMULATOR_IMAGE must use immutable sha256 digests\n' >&2; exit 2;
}
[[ "$EXTPROC_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ && "$EXTPROC_IMAGE" == *@sha256:* ]] || {
  printf 'EXTPROC_SOURCE_REVISION must be a full Git SHA and EXTPROC_IMAGE an immutable sha256 digest\n' >&2; exit 2;
}
[[ "$QUEUE_CAPACITY" =~ ^[1-9][0-9]*$ && "$STALE_METRICS_SECONDS" =~ ^[1-9][0-9]*$ ]] || {
  printf 'queue capacity and stale metrics seconds must be positive integers\n' >&2; exit 2;
}
for metric in "$EPP_QUEUE_METRIC" "$EPP_KV_CACHE_METRIC" "$EPP_READY_METRIC" "$GRID_SIGNAL_METRIC"; do
  [[ "$metric" =~ ^[a-zA-Z_:][a-zA-Z0-9_:]*$ ]] || { printf 'invalid Prometheus metric name: %s\n' "$metric" >&2; exit 2; }
done
[[ "$GRID_SIGNAL_METRIC" == "$EPP_QUEUE_METRIC" || "$GRID_SIGNAL_METRIC" == "$EPP_KV_CACHE_METRIC" ]] || {
  printf 'GRID_SIGNAL_METRIC must match a configured queueDepth or kvCacheUtilization signal\n' >&2; exit 2;
}
[[ "$WAIT_TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'GRID_PROVIDER_WAIT_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
if [[ -z "$PROVIDER_PREFIX" ]]; then
  PROVIDER_PREFIX="$(printf '%s' "$RUN_LABEL" | tr '[:upper:]' '[:lower:]' | sed -E 's/[^a-z0-9-]+/-/g' | cut -c1-38 | sed -E 's/-+$//')"
fi
PROVIDER_A="$PROVIDER_PREFIX-pool-a"
PROVIDER_B="$PROVIDER_PREFIX-pool-b"
for name in "$PROVIDER_A" "$PROVIDER_B"; do
  [[ "$name" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ && ${#name} -le 63 ]] || {
    printf 'generated provider name is not a Kubernetes label/name: %s\n' "$name" >&2; exit 2;
  }
done
[[ "$PROVIDER_A" != "$PROVIDER_B" ]] || { printf 'provider names must be distinct\n' >&2; exit 2; }
if [[ -z "$EVIDENCE_DIR" ]]; then EVIDENCE_DIR="$REPO_ROOT/evidence/grid-providers-$(date -u +%Y%m%dT%H%M%SZ)"; fi
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists; choose a fresh timestamped directory\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  printf '%s\n' "$line" | tee -a "$EVIDENCE_DIR/RUN-LOG.md" "$REPO_ROOT/RUN-LOG.md"
}

write_result() {
  local status="$1" reason="$2"
  jq -n --arg mode "$MODE" --arg status "$status" --arg reason "$reason" \
    --arg evidence "$EVIDENCE_DIR" --arg ns "$NAMESPACE" --arg network "$GRID_NETWORK_NAME" \
    --arg providerA "$PROVIDER_A" --arg providerB "$PROVIDER_B" \
    '{mode:$mode,status:$status,reason:$reason,evidenceDirectory:$evidence,namespace:$ns,gridNetwork:$network,providers:[$providerA,$providerB],inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
  printf '# Grid provider registration\n\n- Status: %s\n- Reason: %s\n- Inference traffic sent: no\n- Evidence: %s\n' \
    "$status" "$reason" "$EVIDENCE_DIR" >"$EVIDENCE_DIR/FINAL-RESULT.md"
}

on_error() {
  local code="$1" phase="$CURRENT_PHASE"
  trap - ERR
  cleanup_port_forwards
  log "Grid provider mode $MODE stopped with exit=$code at phase=$phase"
  {
    printf '# Grid provider bug capture\n\n- Mode: %s\n- First failed phase: %s\n- Exit code: %s\n' "$MODE" "$phase" "$code"
    # shellcheck disable=SC2016
    printf -- '- Grid source: `%s`\n- Downstream ODH operator source: `%s`\n- Evidence: `%s`\n' \
      "$GRID_SOURCE_REVISION" "$GRID_OPERATOR_SOURCE_REVISION" "$EVIDENCE_DIR"
    printf '\nNo Secret data, token, kubeconfig contents, or inference request was recorded.\n'
  } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  capture_failure_state || true
  write_result BLOCKED "failed at $phase (exit $code)"
  exit "$code"
}
trap 'on_error $?' ERR
trap 'cleanup_port_forwards; exit 130' INT TERM

cleanup_port_forwards() {
  local pid
  for pid in "${PORT_FORWARD_PIDS[@]:-}"; do
    [[ -n "$pid" ]] || continue
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
  done
  PORT_FORWARD_PIDS=()
}

api_access() {
  local user server ready insecure tls_mode
  CURRENT_PHASE=api-access
  user="$(oc whoami)"
  server="$(oc whoami --show-server)"
  ready="$(oc get --raw=/readyz)"
  [[ "$server" == "$EXPECTED_API_SERVER" ]] || { log "API server mismatch: expected $EXPECTED_API_SERVER observed $server"; return 1; }
  [[ "$ready" == *ok* ]] || { log 'API /readyz did not return ok'; return 1; }
  insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
  if [[ "$insecure" == true ]]; then
    [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
      log 'API kubeconfig TLS bypass requires explicit ephemeral-dev opt-in'; return 1;
    }
    tls_mode=explicit-ephemeral-dev-unverified
  else
    tls_mode=kubeconfig-verified
  fi
  printf 'capturedAt=%s\nidentity=%s\nserver=%s\ncontext=%s\nreadyz=ok\napiTlsMode=%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$user" "$server" "$KUBE_CONTEXT" "$tls_mode" >"$EVIDENCE_DIR/api-access-summary.txt"
  chmod 0600 "$EVIDENCE_DIR/api-access-summary.txt"
  log "API identity=$user server=$server readyz=ok tlsMode=$tls_mode"
}

verify_namespace_ownership() {
  local labels
  CURRENT_PHASE=task-namespace-ownership
  labels="$(oc get namespace "$NAMESPACE" -o json | jq -r '[.metadata.labels["app.kubernetes.io/part-of"],.metadata.labels["praxis.fast/qualification-run"]]|@tsv')"
  [[ "$labels" == $'grid-llmd-e2e\t'"$RUN_LABEL" ]] || {
    log "namespace $NAMESPACE does not match this task run's ownership labels"; return 1;
  }
  oc get namespace "$NAMESPACE" -o json | jq '{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels}' >"$EVIDENCE_DIR/task-namespace.json"
}

discover_grid_crd() {
  local crd_file="$EVIDENCE_DIR/inferenceprovider-crd.json" storage_version count
  CURRENT_PHASE=installed-inferenceprovider-crd
  oc get crds -o json | jq '[.items[]|select(.spec.names.kind=="InferenceProvider" and .spec.names.plural=="inferenceproviders" and (.spec.group|endswith(".praxis.fast")))]' \
    >"$EVIDENCE_DIR/inferenceprovider-crd-candidates.json"
  count="$(jq 'length' "$EVIDENCE_DIR/inferenceprovider-crd-candidates.json")"
  [[ "$count" == 1 ]] || { log "expected exactly one downstream ODH InferenceProvider CRD, discovered $count matching CRDs"; return 1; }
  jq '.[0]' "$EVIDENCE_DIR/inferenceprovider-crd-candidates.json" >"$crd_file"
  jq -e 'any(.status.conditions[]?;.type=="Established" and .status=="True")' "$crd_file" >/dev/null || {
    log 'downstream ODH InferenceProvider CRD is not Established'; return 1;
  }
  GRID_GROUP="$(jq -r '.spec.group' "$crd_file")"
  GRID_PLURAL="$(jq -r '.spec.names.plural' "$crd_file")"
  [[ "$(jq -r '.spec.names.kind' "$crd_file")" == InferenceProvider ]] || { log 'installed CRD kind is not InferenceProvider'; return 1; }
  [[ "$(jq -r '.spec.scope' "$crd_file")" == Cluster ]] || { log 'installed InferenceProvider CRD is not cluster-scoped'; return 1; }
  storage_version="$(jq -r '[.spec.versions[]|select(.served==true and .storage==true)|.name][0] // empty' "$crd_file")"
  if [[ -z "$storage_version" ]]; then storage_version="$(jq -r '[.spec.versions[]|select(.served==true)|.name][0] // empty' "$crd_file")"; fi
  [[ -n "$storage_version" ]] || { log 'installed InferenceProvider CRD has no served API version'; return 1; }
  GRID_VERSION="$storage_version"
  GRID_RESOURCE="$GRID_PLURAL.$GRID_GROUP"
  jq -e --arg version "$GRID_VERSION" '
    .spec.versions[]|select(.name==$version)|.schema.openAPIV3Schema.properties.spec.properties as $p|
    (["gridNetworkRef","providerKind","backendKind","endpoint","clusterName","models","siteSelector","accessPolicy","metricsConfig"]|all(.[];. as $field|$p|has($field))) and
    ($p.metricsConfig.properties as $m | ["endpoint","path","poolName","queueCapacity","staleMetricsSeconds","signalNames"]|all(.[];. as $field|$m|has($field))) and
    ($p.metricsConfig.properties.auth.properties.type.enum|index("serviceAccountToken")!=null) and
    $p.metricsConfig.properties.auth.properties.allowPlaintext.type=="boolean" and
    ($p.metricsConfig.properties.signalNames.properties as $s | ["queueDepth","kvCacheUtilization","healthy","readyEndpoints"]|all(.[];. as $field|$s|has($field))) and
    ($p.models.items.properties|has("name")) and
    $p.gridNetworkRef.type=="string" and $p.endpoint.type=="string" and
    $p.metricsConfig.properties.endpoint.type=="string"
  ' "$crd_file" >/dev/null || { log 'installed InferenceProvider schema lacks the required llm-d endpoint/metrics contract'; return 1; }
  jq '{name:.metadata.name,group:.spec.group,plural:.spec.names.plural,kind:.spec.names.kind,scope:.spec.scope,versions:[.spec.versions[]|{name,served,storage,properties:(.schema.openAPIV3Schema.properties.spec.properties|keys)}]}' \
    "$crd_file" >"$EVIDENCE_DIR/inferenceprovider-crd-schema-summary.json"
  jq -S '.spec.versions' "$crd_file" | sha256sum | awk '{print $1}' >"$EVIDENCE_DIR/inferenceprovider-crd-schema-sha256.txt"
  log "Discovered installed InferenceProvider API $GRID_GROUP/$GRID_VERSION ($GRID_RESOURCE); schema hash recorded"
}

capture_network_and_site() {
  local network_file="$EVIDENCE_DIR/gridnetwork.json" sites_file="$EVIDENCE_DIR/gridsites.json"
  CURRENT_PHASE=grid-network-site-preflight
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json >"$network_file"
  jq -e --arg run "$RUN_LABEL" --arg ns "$NAMESPACE" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .status.phase=="Active" and
    (.spec.consumerGateways|length)==1 and
    .spec.consumerGateways[0].namespace==$ns
  ' "$network_file" >/dev/null || { log 'GridNetwork must be task-owned, Active, and have exactly one task consumer Gateway'; return 1; }
  ROUTING_GATEWAY="$(jq -r '.spec.consumerGateways[0].name' "$network_file")"
  ROUTING_GATEWAY_NAMESPACE="$(jq -r '.spec.consumerGateways[0].namespace' "$network_file")"
  jq '{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,phase:.status.phase,consumerGateways:.spec.consumerGateways,routingMapStatus:.status.routingMapStatus}' \
    "$network_file" >"$EVIDENCE_DIR/gridnetwork-summary.json"
  oc get gridsites -o json >"$sites_file"
  TASK_SITE="$(jq -r --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" '
    [.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gridNetworkRef==$network)]|if length==1 then .[0].metadata.name else empty end
  ' "$sites_file")"
  [[ "$TASK_SITE" == "$GRID_SITE_NAME" ]] || { log 'expected exactly one run-owned GridSite on the task GridNetwork'; return 1; }
  jq --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" \
    '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gridNetworkRef==$network)|{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,network:.spec.gridNetworkRef,phase:.status.phase,reason:.status.reason,observedGeneration:.status.observedGeneration}]' \
    "$sites_file" >"$EVIDENCE_DIR/task-gridsites.json"
  jq -e 'length==1 and .[0].phase=="Active" and .[0].observedGeneration==.[0].generation' "$EVIDENCE_DIR/task-gridsites.json" >/dev/null || {
    log 'task GridSite is not Active at its current generation'; return 1;
  }
}

resolve_gateway_service() {
  local pool="$1" gateway="$2" gateway_json listener_port services matches service port host
  gateway_json="$EVIDENCE_DIR/$pool-gateway.json"
  oc get gateway "$gateway" -n "$NAMESPACE" -o json >"$gateway_json"
  jq -e '.metadata.generation as $g | any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and any(.status.conditions[]?;.type=="Programmed" and .status=="True" and .observedGeneration==$g)' "$gateway_json" >/dev/null || {
    log "pool Gateway $gateway is not Accepted/Programmed at current generation"; return 1;
  }
  listener_port="$(jq -r '[.spec.listeners[]?|select(.name=="inference")|.port][0] // empty' "$gateway_json")"
  [[ "$listener_port" =~ ^[0-9]+$ ]] || { log "pool Gateway $gateway lacks its inference listener"; return 1; }
  services="$(oc get services -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$gateway" -o json)"
  matches="$(jq --argjson listener "$listener_port" '[.items[]|select(any(.spec.ports[]?;.port==$listener))|{name:.metadata.name,ports:.spec.ports,labels:.metadata.labels,annotations:.metadata.annotations}]' <<<"$services")"
  [[ "$(jq 'length' <<<"$matches")" == 1 ]] || { log "Gateway $gateway does not have exactly one listener Service on port $listener_port"; return 1; }
  service="$(jq -r '.[0].name' <<<"$matches")"
  port="$(jq -r --argjson listener "$listener_port" '.[0].ports[]|select(.port==$listener)|.port' <<<"$matches")"
  host="$service.$NAMESPACE.svc.cluster.local:$port"
  jq -n --arg pool "$pool" --arg gateway "$gateway" --arg service "$service" --arg host "$host" \
    --argjson listenerPort "$listener_port" --argjson service "$matches" \
    '{pool:$pool,gateway:$gateway,service:$service,listenerPort:$listenerPort,providerEndpoint:("http://"+$host),serviceEvidence:$service}' \
    >"$EVIDENCE_DIR/$pool-gateway-service.json"
}

resolve_metrics_service() {
  local pool="$1" service service_json ports port endpoints
  service="$pool-epp-service"
  CURRENT_PHASE="$pool-epp-metrics-service"
  service_json="$EVIDENCE_DIR/$pool-epp-service.json"
  oc get service "$service" -n "$NAMESPACE" -o json >"$service_json"
  ports="$(jq '[.spec.ports[]?|select(.name=="metrics" or .port==9090)|{name,port}]' "$service_json")"
  [[ "$(jq 'length' <<<"$ports")" == 1 ]] || { log "$pool EPP Service must expose one unambiguous metrics port"; return 1; }
  port="$(jq -r '.[0].port' <<<"$ports")"
  endpoints="$(oc get endpointslices -n "$NAMESPACE" -l "kubernetes.io/service-name=$service" -o json | jq '[.items[]|{name:.metadata.name,ready:[.endpoints[]?|select(.conditions.ready==true)]|length,total:(.endpoints|length)}]')"
  jq -e 'length>0 and any(.[];.ready>0)' <<<"$endpoints" >/dev/null || { log "$pool EPP metrics Service has no ready EndpointSlice target"; return 1; }
  jq -n --arg pool "$pool" --arg service "$service" --arg host "$service.$NAMESPACE.svc.cluster.local:$port" \
    --argjson port "$port" --argjson endpoints "$endpoints" \
    '{pool:$pool,service:$service,port:$port,endpoint:("http://"+$host),path:"/metrics",scrapeUrl:("http://"+$host+"/metrics"),readyEndpoints:$endpoints}' \
    >"$EVIDENCE_DIR/$pool-metrics-service.json"
}

resolve_inferencepool_name() {
  local pool="$1" matches name
  CURRENT_PHASE="$pool-inferencepool-name"
  oc get inferencepools -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/inferencepools.json"
  matches="$(jq --arg pool "$pool" '[.items[]|select(.metadata.labels["app.kubernetes.io/name"]==$pool and any(.metadata.ownerReferences[]?;.kind=="LLMInferenceService" and .name==$pool))|{name:.metadata.name,uid:.metadata.uid,ownerReferences:.metadata.ownerReferences,endpointPickerRef:.spec.endpointPickerRef}]' "$EVIDENCE_DIR/inferencepools.json")"
  [[ "$(jq 'length' <<<"$matches")" == 1 ]] || { log "$pool must own exactly one current InferencePool for EPP signal selection"; return 1; }
  name="$(jq -r '.[0].name' <<<"$matches")"
  jq --arg pool "$pool" --arg name "$name" '.items[]|select(.metadata.name==$name and .metadata.labels["app.kubernetes.io/name"]==$pool)' \
    "$EVIDENCE_DIR/inferencepools.json" >"$EVIDENCE_DIR/$pool-inferencepool.json"
  printf '%s' "$name"
}

capture_metrics_contract() {
  local pool="$1" expected_pool_name="$2" service="$3" remote_port="$4" local_port forward_pid attempt ok=false raw token
  raw="$EVIDENCE_DIR/$pool-epp-metrics-raw.txt"
  CURRENT_PHASE="$pool-epp-metrics-contract"
  if [[ "$RESOURCE_PROFILE" != reduced-dev || "$CLUSTER_ENV" != ephemeral-dev ]]; then
    log 'HTTP EPP metrics require the reduced-dev ephemeral-cluster profile; strict runs must use authenticated HTTPS metrics'
    return 1
  fi
  if ! token="$(oc create token "$METRICS_SCRAPER_SERVICE_ACCOUNT" -n "$GRID_OPERATOR_NAMESPACE" --duration=10m 2>/dev/null)" || [[ -z "$token" ]]; then
    log 'could not mint the short-lived Grid metrics-scraper ServiceAccount token; no credential was recorded'
    return 1
  fi
  local_port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
  oc port-forward -n "$NAMESPACE" "service/$service" "$local_port:$remote_port" >"$EVIDENCE_DIR/$pool-port-forward.log" 2>&1 &
  forward_pid=$!
  PORT_FORWARD_PIDS+=("$forward_pid")
  for attempt in $(seq 1 30); do
    if printf 'header = "Authorization: Bearer %s"\n' "$token" | \
      curl --config - --silent --show-error --fail --connect-timeout 2 --max-time 5 \
        "http://127.0.0.1:$local_port/metrics" -o "$raw" 2>/dev/null; then
      ok=true
      break
    fi
    sleep 1
  done
  printf 'connectionAttempts=%s\n' "$attempt" >"$EVIDENCE_DIR/$pool-port-forward-attempts.txt"
  kill "$forward_pid" 2>/dev/null || true
  wait "$forward_pid" 2>/dev/null || true
  unset token
  local i
  for i in "${!PORT_FORWARD_PIDS[@]}"; do
    if [[ "${PORT_FORWARD_PIDS[$i]}" == "$forward_pid" ]]; then unset 'PORT_FORWARD_PIDS[i]'; fi
  done
  [[ "$ok" == true ]] || { log "$pool authenticated EPP /metrics was not reachable through its service port-forward; no token was recorded"; return 1; }
  if ! awk -v pool_name="$expected_pool_name" -v q="$EPP_QUEUE_METRIC" -v kv="$EPP_KV_CACHE_METRIC" -v ready="$EPP_READY_METRIC" '
    {
      metric=$1; sub(/\{.*/, "", metric)
      if ((metric==q || metric==kv || metric==ready) && index($0,"name=\"" pool_name "\"")>0) {
        print
        seen[metric]=1
      }
    }
    END { exit !(seen[q] && seen[kv] && seen[ready]) }
  ' "$raw" >"$EVIDENCE_DIR/$pool-epp-metrics-samples.prom"; then
    log "$pool EPP /metrics lacks queue, KV-cache, or ready-endpoint samples labeled for InferencePool=$expected_pool_name; see bounded metrics evidence"
    return 1
  fi
  chmod 0600 "$raw" "$EVIDENCE_DIR/$pool-epp-metrics-samples.prom"
  jq -n --arg pool "$pool" --arg poolName "$expected_pool_name" --arg service "$service" --arg queueMetric "$EPP_QUEUE_METRIC" \
    --arg kvMetric "$EPP_KV_CACHE_METRIC" --arg readyMetric "$EPP_READY_METRIC" \
    --arg configuredSignal "$GRID_SIGNAL_METRIC" --arg sampleFile "$pool-epp-metrics-samples.prom" \
    --arg serviceAccount "$METRICS_SCRAPER_SERVICE_ACCOUNT" \
    '{pool:$pool,poolName:$poolName,service:$service,endpointPath:"/metrics",authType:"serviceAccountToken",metricsScraperServiceAccount:$serviceAccount,authTokenRecorded:false,queueMetric:$queueMetric,kvCacheMetric:$kvMetric,readyEndpointsMetric:$readyMetric,signalUsedByGrid:$configuredSignal,poolLabel:{name:$poolName},samples:$sampleFile,reachable:true}' \
    >"$EVIDENCE_DIR/$pool-epp-metrics-contract.json"
  log "$pool authenticated EPP metrics endpoint exposed all configured signals for InferencePool=$expected_pool_name; signal=$GRID_SIGNAL_METRIC; bearer token was not recorded"
}

write_provider_manifest() {
  local pool="$1" provider="$2" endpoint="$3" metrics_endpoint="$4" metrics_pool_name="$5" output allow_plaintext=false
  output="$EVIDENCE_DIR/$provider-inferenceprovider.json"
  case "$metrics_endpoint" in
    http://*)
      [[ "$RESOURCE_PROFILE" == reduced-dev && "$CLUSTER_ENV" == ephemeral-dev ]] || {
        log 'refusing to send an authenticated EPP metrics request over HTTP outside explicit reduced-dev ephemeral mode'
        return 1
      }
      allow_plaintext=true
      ;;
    https://*) allow_plaintext=false ;;
    *) log "unsupported EPP metrics endpoint scheme: $metrics_endpoint"; return 1 ;;
  esac
  jq -n --arg apiVersion "$GRID_GROUP/$GRID_VERSION" --arg provider "$provider" --arg run "$RUN_LABEL" \
    --arg network "$GRID_NETWORK_NAME" --arg model "$MODEL_NAME" --arg endpoint "$endpoint" \
    --arg metrics "$metrics_endpoint" --arg pool "$pool" --arg metricsPoolName "$metrics_pool_name" --arg queue "$EPP_QUEUE_METRIC" \
    --arg kv "$EPP_KV_CACHE_METRIC" --arg ready "$EPP_READY_METRIC" --argjson queueCapacity "$QUEUE_CAPACITY" \
    --argjson allowPlaintext "$allow_plaintext" \
    --argjson staleMetricsSeconds "$STALE_METRICS_SECONDS" --arg gridSource "$GRID_SOURCE_REVISION" \
    --arg eppSource "$EPP_SOURCE_REVISION" --arg operatorSource "$GRID_OPERATOR_SOURCE_REVISION" \
    '{apiVersion:$apiVersion,kind:"InferenceProvider",metadata:{name:$provider,labels:{"app.kubernetes.io/managed-by":"grid-llmd-e2e-run","app.kubernetes.io/part-of":"grid-llmd-e2e","praxis.fast/qualification-run":$run,"praxis.fast/provider-pool":$pool,"praxis.fast/grid-source-revision":$gridSource,"praxis.fast/operator-source-revision":$operatorSource,"praxis.fast/epp-source-revision":$eppSource}},spec:{gridNetworkRef:$network,providerKind:"llm-d-inference-sim",backendKind:"local",endpoint:$endpoint,clusterName:$provider,siteSelector:{matchLabels:{"praxis.fast/qualification-run":$run}},accessPolicy:{siteSelector:{matchLabels:{}}},models:[{name:$model,capabilities:["text_generation"]}],metricsConfig:{endpoint:$metrics,path:"/metrics",timeout:"2s",auth:{type:"serviceAccountToken",allowPlaintext:$allowPlaintext},poolName:$metricsPoolName,queueCapacity:$queueCapacity,staleMetricsSeconds:$staleMetricsSeconds,signalNames:{queueDepth:$queue,kvCacheUtilization:$kv,healthy:$ready,readyEndpoints:$ready}}}}' \
    >"$output"
  chmod 0600 "$output"
  jq -e --arg run "$RUN_LABEL" --arg pool "$pool" --arg endpoint "$endpoint" --arg metrics "$metrics_endpoint" \
    --arg metricsPoolName "$metrics_pool_name" --argjson allowPlaintext "$allow_plaintext" \
    '.kind=="InferenceProvider" and .metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.labels["praxis.fast/provider-pool"]==$pool and .spec.endpoint==$endpoint and .spec.metricsConfig.endpoint==$metrics and .spec.metricsConfig.poolName==$metricsPoolName and .spec.metricsConfig.auth.type=="serviceAccountToken" and .spec.metricsConfig.auth.allowPlaintext==$allowPlaintext and .spec.siteSelector.matchLabels["praxis.fast/qualification-run"]==$run' \
    "$output" >/dev/null
}

verify_task_provider_ownership() {
  local provider="$1" object
  if object="$(oc get "$GRID_RESOURCE" "$provider" -o json 2>/dev/null)"; then
    jq -e --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" \
      '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gridNetworkRef==$network' <<<"$object" >/dev/null || {
      log "InferenceProvider $provider exists without exact task ownership/network identity"; return 1;
    }
  fi
}

apply_or_verify_providers() {
  local operation="$1" provider file
  CURRENT_PHASE=inferenceprovider-server-dry-run
  for provider in "$PROVIDER_A" "$PROVIDER_B"; do
    file="$EVIDENCE_DIR/$provider-inferenceprovider.json"
    if [[ "$operation" == apply ]]; then
      verify_task_provider_ownership "$provider"
      oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e -f "$file" -o json >"$EVIDENCE_DIR/$provider-server-dry-run.json"
    else
      oc get "$GRID_RESOURCE" "$provider" -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:.spec,status:.status}' >"$EVIDENCE_DIR/$provider-live.json"
      jq -e --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" --slurpfile expected "$file" \
        '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gridNetworkRef==$network and .spec==$expected[0].spec' \
        "$EVIDENCE_DIR/$provider-live.json" >/dev/null || { log "live provider $provider differs from the desired task-owned model/network contract"; return 1; }
    fi
  done
  [[ "$operation" == apply ]] || return 0
  CURRENT_PHASE=inferenceprovider-apply
  for provider in "$PROVIDER_A" "$PROVIDER_B"; do
    oc apply --server-side --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/$provider-inferenceprovider.json" >"$EVIDENCE_DIR/$provider-apply.txt"
    log "Applied task-owned InferenceProvider $provider through installed CRD $GRID_GROUP/$GRID_VERSION"
  done
}

capture_provider_status() {
  local provider="$1"
  oc get "$GRID_RESOURCE" "$provider" -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{gridNetworkRef:.spec.gridNetworkRef,providerKind:.spec.providerKind,backendKind:.spec.backendKind,endpoint:.spec.endpoint,clusterName:.spec.clusterName,models:.spec.models,siteSelector:.spec.siteSelector,metricsConfig:.spec.metricsConfig},status:.status}' \
    >"$EVIDENCE_DIR/$provider-live.json"
}

wait_for_provider_ready() {
  local provider="$1" seconds="${WAIT_TIMEOUT%[smh]}" unit="${WAIT_TIMEOUT: -1}" deadline ready=false
  case "$unit" in s) : ;; m) seconds=$((seconds*60)) ;; h) seconds=$((seconds*3600)) ;; esac
  deadline=$((SECONDS+seconds))
  CURRENT_PHASE="$provider-readiness"
  while ((SECONDS<=deadline)); do
    capture_provider_status "$provider"
    if jq -e --arg site "$GRID_SITE_NAME" '
      .metadata.generation as $g |
      .status.observedGeneration==$g and .status.phase=="Available" and
      any(.status.matchingSites[]?;.==$site) and
      ([.status.conditions[]? |
        select((.type=="Ready" or .type=="MetricsAvailable") and
          .observedGeneration==$g and .status=="False")] | length)==0
    ' "$EVIDENCE_DIR/$provider-live.json" >/dev/null; then ready=true; break; fi
    sleep 5
  done
  [[ "$ready" == true ]] || { log "InferenceProvider $provider did not become current-generation Available for GridSite/$GRID_SITE_NAME, or reported a current-generation negative readiness condition"; return 1; }
  log "InferenceProvider $provider is current-generation Available for GridSite/$GRID_SITE_NAME; EPP metric readiness is checked separately"
}

capture_current_provider_metrics() {
  local pool service port pool_name
  for pool in "$POOL_A" "$POOL_B"; do
    service="$(jq -r '.service' "$EVIDENCE_DIR/$pool-metrics-service.json")"
    port="$(jq -r '.port' "$EVIDENCE_DIR/$pool-metrics-service.json")"
    pool_name="$(resolve_inferencepool_name "$pool")"
    capture_metrics_contract "$pool" "$pool_name" "$service" "$port"
  done
  log 'Authenticated EPP queue/KV/readiness samples recaptured for both pools after provider reconciliation'
}

verify_gridnetwork_scoring_config() {
  local network_file="$EVIDENCE_DIR/gridnetwork-scoring-live.json"
  CURRENT_PHASE=gridnetwork-scoring-policy
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json | jq '{metadata:{name:.metadata.name,generation:.metadata.generation,labels:.metadata.labels},spec:{routingPolicy:.spec.routingPolicy,scoringPolicy:.spec.scoringPolicy,selectionPolicy:.spec.selectionPolicy,metricsRefreshInterval:.spec.metricsRefreshInterval},status:{phase:.status.phase,routingMapStatus:.status.routingMapStatus}}' >"$network_file"
  jq -e --arg network "$GRID_NETWORK_NAME" --arg run "$RUN_LABEL" '
    .metadata.name==$network and .metadata.labels["praxis.fast/qualification-run"]==$run and
    .spec.routingPolicy=="scoreFirst" and .spec.scoringPolicy.strategy=="queueDepth" and
    .spec.selectionPolicy.mode=="deterministic" and .spec.metricsRefreshInterval=="10s"
  ' "$network_file" >/dev/null || {
    log 'live task GridNetwork does not have the run-owned scoreFirst/queueDepth/deterministic/10s policy'; return 1;
  }
  log 'Live task GridNetwork scoring policy matches the rendered Helm policy'
}

verify_overlay_queue_scores() {
  local overlay_file="$1" pool_a_name pool_b_name
  pool_a_name="$(jq -r '.metadata.name // empty' "$EVIDENCE_DIR/$POOL_A-inferencepool.json")"
  pool_b_name="$(jq -r '.metadata.name // empty' "$EVIDENCE_DIR/$POOL_B-inferencepool.json")"
  [[ -n "$pool_a_name" && -n "$pool_b_name" && "$pool_a_name" != "$pool_b_name" ]] || {
    log "resolved task InferencePool names are missing or not distinct: $POOL_A=$pool_a_name $POOL_B=$pool_b_name"; return 1;
  }
  python3 "$SCRIPT_DIR/verify-grid-queue-scores.py" \
    --overlay "$overlay_file" --queue-metric "$EPP_QUEUE_METRIC" \
    --queue-capacity "$QUEUE_CAPACITY" --model "$MODEL_NAME" \
    --provider-a "$PROVIDER_A" --provider-b "$PROVIDER_B" \
    --pool-a "$pool_a_name" --pool-b "$pool_b_name" \
    --pool-a-samples "$EVIDENCE_DIR/$POOL_A-epp-metrics-samples.prom" \
    --pool-b-samples "$EVIDENCE_DIR/$POOL_B-epp-metrics-samples.prom" \
    >"$EVIDENCE_DIR/grid-queue-score-contract.json" || {
      log 'Distributed Grid queue-depth score/rank does not match the fresh authenticated EPP samples'; return 1;
    }
  log 'Both Distributed overlay queue-depth scores match fresh authenticated per-pool EPP samples and queue-capacity normalization'
}

wait_for_grid_overlay() {
  local seconds="${WAIT_TIMEOUT%[smh]}" unit="${WAIT_TIMEOUT: -1}" deadline network_file status_file cm_file overlay_file envelope_file ready=false
  case "$unit" in s) : ;; m) seconds=$((seconds*60)) ;; h) seconds=$((seconds*3600)) ;; esac
  deadline=$((SECONDS+seconds))
  network_file="$EVIDENCE_DIR/gridnetwork-live.json"
  status_file="$EVIDENCE_DIR/grid-routing-map-status.json"
  cm_file="$EVIDENCE_DIR/grid-routing-overlay-configmap.json"
  overlay_file="$EVIDENCE_DIR/grid-routing-config.json"
  envelope_file="$EVIDENCE_DIR/grid-routing-overlay.json"
  CURRENT_PHASE=distributed-grid-candidates
  verify_gridnetwork_scoring_config
  while ((SECONDS<=deadline)); do
    oc get gridnetwork "$GRID_NETWORK_NAME" -o json >"$network_file"
    jq --arg gateway "$ROUTING_GATEWAY" --arg ns "$ROUTING_GATEWAY_NAMESPACE" \
      '{metadata:{name:.metadata.name,generation:.metadata.generation},phase:.status.phase,routingMapStatus:[.status.routingMapStatus[]?|select(.gatewayName==$gateway and .namespace==$ns)]}' \
      "$network_file" >"$status_file"
    if jq -e --argjson minCandidates 2 '
      .metadata.generation as $g |
      .phase=="Active" and (.routingMapStatus|length)==1 and
      all(.routingMapStatus[];.phase=="Distributed" and .renderedRevision!="" and .distributedRevision==.renderedRevision and .contentDigest!="" and .configMapName!="" and .configMapResourceVersion!="" and .candidateCount >= $minCandidates and .observedGeneration==$g)
    ' "$status_file" >/dev/null 2>&1; then
      local cm_name cm_ns
      cm_name="$(jq -r '.routingMapStatus[0].configMapName // empty' "$status_file")"
      cm_ns="$(jq -r '.routingMapStatus[0].namespace // empty' "$status_file")"
        local cm_resource_version
        cm_resource_version="$(jq -r '.routingMapStatus[0].configMapResourceVersion // empty' "$status_file")"
      if [[ -n "$cm_name" && -n "$cm_ns" ]] && oc get configmap "$cm_name" -n "$cm_ns" -o json >"$cm_file" 2>/dev/null; then
          jq -e --arg resourceVersion "$cm_resource_version" '.metadata.resourceVersion==$resourceVersion' "$cm_file" >/dev/null || {
            sleep 5
            continue
          }
        if jq -e '.data["routing-config.json"]|fromjson' "$cm_file" >"$overlay_file" 2>/dev/null && \
          jq -e '.data["routing-overlay.json"]|fromjson' "$cm_file" >"$envelope_file" 2>/dev/null && \
          jq -e --arg model "$MODEL_NAME" --arg a "$PROVIDER_A" --arg b "$PROVIDER_B" \
            --arg revision "$(jq -r '.routingMapStatus[0].distributedRevision' "$status_file")" \
            --arg digest "$(jq -r '.routingMapStatus[0].contentDigest' "$status_file")" \
            --arg annotationRevision "$(jq -r '.metadata.annotations["grid.praxis.fast/overlay-revision"] // empty' "$cm_file")" \
            --arg annotationDigest "$(jq -r '.metadata.annotations["grid.praxis.fast/overlay-content-digest"] // empty' "$cm_file")" '
            .revision.value==$revision and .content_digest.value==$digest and
            .provenance.producer=="grid-operator" and
            $annotationRevision==$revision and $annotationDigest==$digest and
            . as $overlay |
            [$overlay.overlay.candidates[]?|select(.kind=="inference_model" and .name==$model and (.cluster==$a or .cluster==$b))] as $taskCandidates |
            ([$taskCandidates[].cluster]|unique|sort)==([$a,$b]|sort) and
            all($taskCandidates[];
              (.score|type)=="number" and (.rank|type)=="number" and
              (.score_breakdown.queue_depth|type)=="number")
          ' "$envelope_file" >/dev/null && jq -e '.network|type=="string"' "$overlay_file" >/dev/null; then
          if verify_overlay_queue_scores "$envelope_file"; then
            ready=true
            break
          fi
        fi
      fi
    fi
    sleep 5
  done
  [[ "$ready" == true ]] || { log 'Grid did not distribute a current overlay containing both task provider candidates with score/rank data'; return 1; }
  jq -n --arg network "$GRID_NETWORK_NAME" --arg gateway "$ROUTING_GATEWAY" --arg ns "$ROUTING_GATEWAY_NAMESPACE" \
    --arg configMap "$(jq -r '.routingMapStatus[0].configMapName' "$status_file")" \
    --arg rendered "$(jq -r '.routingMapStatus[0].renderedRevision' "$status_file")" \
    --arg distributed "$(jq -r '.routingMapStatus[0].distributedRevision' "$status_file")" \
    --argjson candidates "$(jq '.routingMapStatus[0].candidateCount' "$status_file")" \
    --argjson providers "$(jq --arg model "$MODEL_NAME" --arg a "$PROVIDER_A" --arg b "$PROVIDER_B" '[.overlay.candidates[]|select(.kind=="inference_model" and .name==$model and (.cluster==$a or .cluster==$b))|{model:.name,cluster,site,score,rank,scoreBreakdown:.score_breakdown}]' "$envelope_file")" \
    '{network:$network,gateway:{name:$gateway,namespace:$ns},configMap:$configMap,phase:"Distributed",renderedRevision:$rendered,distributedRevision:$distributed,candidateCount:$candidates,candidates:$providers}' \
    >"$EVIDENCE_DIR/grid-provider-overlay-summary.json"
  log "Grid overlay is current and Distributed with both provider candidates; queue-depth scores match EPP samples and rendered/distributed revisions match"
}

verify_grid_operator_runtime() {
  local deployment="$EVIDENCE_DIR/grid-operator-deployment.json" pods="$EVIDENCE_DIR/grid-operator-pods.json"
  local operator_sa scraper_namespace can_create_token can_scrape_metrics
  CURRENT_PHASE=downstream-grid-operator-runtime-image
  oc get deployment "$GRID_OPERATOR_DEPLOYMENT_NAME" -n "$GRID_OPERATOR_NAMESPACE" -o json >"$deployment"
  jq -e --arg image "$GRID_OPERATOR_IMAGE" '
    .metadata.generation==.status.observedGeneration and .spec.replicas==1 and
    (.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1 and
    any(.status.conditions[]?;.type=="Available" and .status=="True") and
    (.spec.template.spec.containers|length)>0 and all(.spec.template.spec.containers[];.image==$image)
  ' "$deployment" >/dev/null || { log 'downstream Grid operator Deployment is not current/Available on the pinned image'; return 1; }
  oc get pods -n "$GRID_OPERATOR_NAMESPACE" -l "app.kubernetes.io/instance=$GRID_OPERATOR_RELEASE_NAME" -o json |
    jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,phase:.status.phase,containers:[.status.containerStatuses[]?|{name,image,imageID,ready}]}]' >"$pods"
  local digest="${GRID_OPERATOR_IMAGE##*@}"
  jq -e --arg image "$GRID_OPERATOR_IMAGE" --arg digest "$digest" '
    length==1 and .[0].phase=="Running" and length>0 and
    all(.[0].containers[];.ready==true and .image==$image and (.imageID|endswith($digest)))
  ' "$pods" >/dev/null || { log 'downstream Grid operator Pod imageID does not match the pinned digest'; return 1; }
  operator_sa="$(jq -r '.spec.template.spec.serviceAccountName // empty' "$deployment")"
  METRICS_SCRAPER_SERVICE_ACCOUNT="$(jq -r '[.spec.template.spec.containers[].env[]?|select(.name=="GRID_METRICS_SCRAPER_SERVICE_ACCOUNT")|.value][0] // empty' "$deployment")"
  scraper_namespace="$(jq -r '[.spec.template.spec.containers[].env[]?|select(.name=="GRID_METRICS_SCRAPER_NAMESPACE")|.value][0] // empty' "$deployment")"
  [[ -n "$operator_sa" && -n "$METRICS_SCRAPER_SERVICE_ACCOUNT" ]] || { log 'downstream Grid operator is missing its TokenRequest metrics-scraper identity'; return 1; }
  [[ "$scraper_namespace" == "\$(POD_NAMESPACE)" || "$scraper_namespace" == "$GRID_OPERATOR_NAMESPACE" ]] || {
    log 'downstream Grid metrics-scraper ServiceAccount namespace is not the operator namespace'; return 1;
  }
  oc get serviceaccount "$METRICS_SCRAPER_SERVICE_ACCOUNT" -n "$GRID_OPERATOR_NAMESPACE" -o json |
    jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid}' >"$EVIDENCE_DIR/metrics-scraper-serviceaccount.json"
  can_create_token="$(oc auth can-i create "serviceaccounts/$METRICS_SCRAPER_SERVICE_ACCOUNT" --subresource=token --as="system:serviceaccount:$GRID_OPERATOR_NAMESPACE:$operator_sa" -n "$GRID_OPERATOR_NAMESPACE" || true)"
  can_scrape_metrics="$(oc auth can-i get /metrics --as="system:serviceaccount:$GRID_OPERATOR_NAMESPACE:$METRICS_SCRAPER_SERVICE_ACCOUNT" || true)"
  jq -n --arg operatorServiceAccount "$operator_sa" --arg scraperServiceAccount "$METRICS_SCRAPER_SERVICE_ACCOUNT" \
    --arg namespace "$GRID_OPERATOR_NAMESPACE" --arg canCreateToken "$can_create_token" --arg canScrapeMetrics "$can_scrape_metrics" \
    '{operatorServiceAccount:$operatorServiceAccount,metricsScraperServiceAccount:$scraperServiceAccount,namespace:$namespace,operatorCanCreateScraperTokens:($canCreateToken=="yes"),scraperCanGetMetrics:($canScrapeMetrics=="yes"),tokenRecorded:false}' \
    >"$EVIDENCE_DIR/metrics-scraper-identity.json"
  jq -e '.operatorCanCreateScraperTokens and .scraperCanGetMetrics' "$EVIDENCE_DIR/metrics-scraper-identity.json" >/dev/null || {
    log 'downstream Grid operator metrics token RBAC is incomplete'; return 1;
  }
  log "Downstream Grid operator is current/Available; runtime imageID matches $digest"
}

write_endpoint_inventory() {
  local gateway_a gateway_b
  gateway_a="$(jq -r '.providerEndpoint' "$EVIDENCE_DIR/$POOL_A-gateway-service.json" | sed 's#^http://##')"
  gateway_b="$(jq -r '.providerEndpoint' "$EVIDENCE_DIR/$POOL_B-gateway-service.json" | sed 's#^http://##')"
  jq -n --arg clusterA "$PROVIDER_A" --arg addressA "$gateway_a" --arg clusterB "$PROVIDER_B" --arg addressB "$gateway_b" \
    '[{cluster:$clusterA,address:$addressA,transport:{mode:"plaintext"}},{cluster:$clusterB,address:$addressB,transport:{mode:"plaintext"}}]' \
    >"$EVIDENCE_DIR/grid-praxis-cluster-endpoints.json"
  jq -e 'length==2 and ([.[].cluster]|unique|length)==2 and all(.[];(.address|type=="string") and (.address|contains(".svc.cluster.local:")) and .transport.mode=="plaintext")' \
    "$EVIDENCE_DIR/grid-praxis-cluster-endpoints.json" >/dev/null || { log 'generated Praxis endpoint inventory failed its schema checks'; return 1; }
  chmod 0600 "$EVIDENCE_DIR/grid-praxis-cluster-endpoints.json"
}

write_invocation() {
  local extproc_head extproc_branch helper_sha tracked_hash untracked_hash tls_mode
  extproc_head="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  extproc_branch="$(git -C "$REPO_ROOT" branch --show-current)"
  helper_sha="$(sha256sum "$SCRIPT_DIR/deploy-grid-providers.sh" | awk '{print $1}')"
  tracked_hash="$(git -C "$REPO_ROOT" diff --binary HEAD | sha256sum | awk '{print $1}')"
  untracked_hash="$(git -C "$REPO_ROOT" ls-files --others --exclude-standard -z |
    while IFS= read -r -d '' file; do
      path="$REPO_ROOT/$file"
      if [[ -L "$path" ]]; then
        printf '%s\0%s\0' "$file" "$(readlink -- "$path")" | sha256sum | awk -v name="$file" '{print $1 "  " name}'
      elif [[ -f "$path" ]]; then
        sha256sum "$path" | awk -v name="$file" '{print $1 "  " name}'
      else
        printf 'untracked path disappeared while hashing: %s\n' "$file" >&2
        exit 1
      fi
    done |
    LC_ALL=C sort -k2 | sha256sum | awk '{print $1}')"
  tls_mode="$(sed -n 's/^apiTlsMode=//p' "$EVIDENCE_DIR/api-access-summary.txt")"
  cat >"$EVIDENCE_DIR/INVOCATION.md" <<INVOCATION
# Grid provider registration invocation

- Captured: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- API server/context: $EXPECTED_API_SERVER / $KUBE_CONTEXT
- Identity: recorded in api-access-summary.txt
- API TLS mode: $tls_mode
- Cluster environment/resource profile: $CLUSTER_ENV / $RESOURCE_PROFILE
- Result label: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'DEV-SMOKE / REDUCED-CAPACITY'; else printf 'STRICT / FULL-CAPACITY'; fi)
- Namespace/run: $NAMESPACE / $RUN_LABEL
- GridNetwork/GridSite: $GRID_NETWORK_NAME / $GRID_SITE_NAME
- Providers: $PROVIDER_A / $PROVIDER_B
- Source SHAs: Grid=$GRID_SOURCE_REVISION; ODH Grid operator=$GRID_OPERATOR_SOURCE_REVISION; EPP=$EPP_SOURCE_REVISION; simulator=$SIMULATOR_SOURCE_REVISION; ExtProc=${EXTPROC_SOURCE_REVISION:-not-supplied}
- Immutable images: ODH Grid operator=$GRID_OPERATOR_IMAGE; EPP=$EPP_IMAGE; simulator=$SIMULATOR_IMAGE; ExtProc=${EXTPROC_IMAGE:-not-supplied}
- Praxis ExtProc checkout: $extproc_branch at $extproc_head; tracked diff sha256=$tracked_hash; untracked-content sha256=$untracked_hash
- Provider helper sha256: $helper_sha
- Grid CRD schema hash: recorded after installed CRD discovery in source-and-image-lock.json
- Inference traffic: not sent by this helper

No credential values, Secret contents, certificate material, or kubeconfig data were recorded.
INVOCATION
  chmod 0600 "$EVIDENCE_DIR/INVOCATION.md"
}

write_source_lock() {
  local phase="${1:-final}" manifests=("$EVIDENCE_DIR"/*-inferenceprovider.json) manifest_hashes provider_overlay=null file lock_json lock_md
  manifest_hashes="$EVIDENCE_DIR/provider-manifests.sha256"
  if [[ "$phase" == preapply ]]; then
    lock_json="$EVIDENCE_DIR/source-and-image-lock-preapply.json"
    lock_md="$EVIDENCE_DIR/source-and-image-lock-preapply.md"
  else
    lock_json="$EVIDENCE_DIR/source-and-image-lock.json"
    lock_md="$EVIDENCE_DIR/source-and-image-lock.md"
  fi
  if [[ -f "$EVIDENCE_DIR/grid-provider-overlay-summary.json" ]]; then
    provider_overlay="$(jq -c . "$EVIDENCE_DIR/grid-provider-overlay-summary.json")"
  fi
  if [[ ! -f "$manifest_hashes" ]]; then
    : >"$manifest_hashes"
    for file in "${manifests[@]}"; do
      [[ -f "$file" ]] && sha256sum "$file" >>"$manifest_hashes"
    done
  fi
  jq -n --arg capturedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg api "$EXPECTED_API_SERVER" \
    --arg context "$KUBE_CONTEXT" --arg run "$RUN_LABEL" --arg profile "$RESOURCE_PROFILE" \
    --arg crdName "$(jq -r '.metadata.name' "$EVIDENCE_DIR/inferenceprovider-crd.json")" \
    --arg gridGroup "$GRID_GROUP" --arg gridVersion "$GRID_VERSION" --arg schemaHash "$(cat "$EVIDENCE_DIR/inferenceprovider-crd-schema-sha256.txt")" \
    --arg gridSource "$GRID_SOURCE_REVISION" --arg operatorSource "$GRID_OPERATOR_SOURCE_REVISION" \
    --arg eppSource "$EPP_SOURCE_REVISION" --arg simSource "$SIMULATOR_SOURCE_REVISION" --arg extprocSource "$EXTPROC_SOURCE_REVISION" \
    --arg operatorImage "$GRID_OPERATOR_IMAGE" --arg eppImage "$EPP_IMAGE" --arg simImage "$SIMULATOR_IMAGE" --arg extprocImage "$EXTPROC_IMAGE" \
    --arg metric "$GRID_SIGNAL_METRIC" --arg queue "$EPP_QUEUE_METRIC" --arg kv "$EPP_KV_CACHE_METRIC" --arg ready "$EPP_READY_METRIC" \
    --arg metricsAuthType serviceAccountToken --arg metricsServiceAccount "$METRICS_SCRAPER_SERVICE_ACCOUNT" \
    --arg poolAName "$(jq -r '.name' "$EVIDENCE_DIR/$POOL_A-inferencepool.json")" \
    --arg poolBName "$(jq -r '.name' "$EVIDENCE_DIR/$POOL_B-inferencepool.json")" \
    --argjson metricsAllowPlaintext "$(jq -r '.spec.metricsConfig.auth.allowPlaintext' "$EVIDENCE_DIR/$PROVIDER_A-inferenceprovider.json")" \
    --argjson queueCapacity "$QUEUE_CAPACITY" --argjson staleMetricsSeconds "$STALE_METRICS_SECONDS" \
    --argjson providerOverlay "$provider_overlay" \
    '{capturedAt:$capturedAt,cluster:{apiServer:$api,context:$context},runLabel:$run,resourceProfile:$profile,resultLabel:(if $profile=="reduced-dev" then "DEV-SMOKE / REDUCED-CAPACITY" else "STRICT / FULL-CAPACITY" end),crd:{name:$crdName,group:$gridGroup,version:$gridVersion,schemaSha256:$schemaHash},sourceRevisions:{grid:$gridSource,downstreamOdhGridOperator:$operatorSource,llmDEpp:$eppSource,llmDSim:$simSource,praxisExtProc:(if $extprocSource=="" then null else $extprocSource end)},immutableImages:{downstreamOdhGridOperator:$operatorImage,llmDEpp:$eppImage,llmDSim:$simImage,praxisExtProc:(if $extprocImage=="" then null else $extprocImage end)},signalContract:{queueMetric:$queue,kvCacheMetric:$kv,readyEndpointsMetric:$ready,configuredSignal:$metric,queueCapacity:$queueCapacity,staleMetricsSeconds:$staleMetricsSeconds},metricsScrape:{authType:$metricsAuthType,serviceAccount:$metricsServiceAccount,allowPlaintext:$metricsAllowPlaintext,tokenRecorded:false,poolNames:{poolA:$poolAName,poolB:$poolBName}},providerOverlay:$providerOverlay,renderedManifestsSha256File:"provider-manifests.sha256",inferenceTrafficSent:false}' \
    >"$lock_json"
  jq -r '
    "# Grid provider source and image lock\n\n- Captured: \(.capturedAt)\n- API: `\(.cluster.apiServer)`\n- Resource profile: `\(.resourceProfile)` — \(.resultLabel)\n- CRD: `\(.crd.name)` (`\(.crd.group)/\(.crd.version)`), schema SHA-256 `\(.crd.schemaSha256)`\n- Grid source: `\(.sourceRevisions.grid)`\n- Downstream ODH Grid operator: `\(.sourceRevisions.downstreamOdhGridOperator)` / `\(.immutableImages.downstreamOdhGridOperator)`\n- EPP: `\(.sourceRevisions.llmDEpp)` / `\(.immutableImages.llmDEpp)`\n- Simulator: `\(.sourceRevisions.llmDSim)` / `\(.immutableImages.llmDSim)`\n- ExtProc: `\(.sourceRevisions.praxisExtProc // "not supplied")` / `\(.immutableImages.praxisExtProc // "not supplied")`\n- Signal: `\(.signalContract.configuredSignal)`; queue `\(.signalContract.queueMetric)`; KV-cache `\(.signalContract.kvCacheMetric)`; ready endpoints `\(.signalContract.readyEndpointsMetric)`\n- Rendered provider manifest hashes: `provider-manifests.sha256`\n- Traffic sent: no\n"
  ' "$lock_json" >"$lock_md"
  chmod 0600 "$manifest_hashes" "$lock_json" "$lock_md"
}

prepare_provider_inputs() {
  local metrics_service_a metrics_service_b metrics_port_a metrics_port_b metrics_pool_a metrics_pool_b
  capture_network_and_site
  resolve_gateway_service "$POOL_A" "$POOL_A_GATEWAY"
  resolve_gateway_service "$POOL_B" "$POOL_B_GATEWAY"
  resolve_metrics_service "$POOL_A"
  resolve_metrics_service "$POOL_B"
  metrics_service_a="$(jq -r '.service' "$EVIDENCE_DIR/$POOL_A-metrics-service.json")"
  metrics_service_b="$(jq -r '.service' "$EVIDENCE_DIR/$POOL_B-metrics-service.json")"
  metrics_port_a="$(jq -r '.port' "$EVIDENCE_DIR/$POOL_A-metrics-service.json")"
  metrics_port_b="$(jq -r '.port' "$EVIDENCE_DIR/$POOL_B-metrics-service.json")"
  metrics_pool_a="$(resolve_inferencepool_name "$POOL_A")"
  metrics_pool_b="$(resolve_inferencepool_name "$POOL_B")"
  capture_metrics_contract "$POOL_A" "$metrics_pool_a" "$metrics_service_a" "$metrics_port_a"
  capture_metrics_contract "$POOL_B" "$metrics_pool_b" "$metrics_service_b" "$metrics_port_b"
  write_provider_manifest "$POOL_A" "$PROVIDER_A" "$(jq -r '.providerEndpoint' "$EVIDENCE_DIR/$POOL_A-gateway-service.json")" \
    "$(jq -r '.endpoint' "$EVIDENCE_DIR/$POOL_A-metrics-service.json")" "$metrics_pool_a"
  write_provider_manifest "$POOL_B" "$PROVIDER_B" "$(jq -r '.providerEndpoint' "$EVIDENCE_DIR/$POOL_B-gateway-service.json")" \
    "$(jq -r '.endpoint' "$EVIDENCE_DIR/$POOL_B-metrics-service.json")" "$metrics_pool_b"
  write_endpoint_inventory
}

capture_failure_state() {
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json 2>/dev/null | jq '{name:.metadata.name,generation:.metadata.generation,phase:.status.phase,routingMapStatus:.status.routingMapStatus}' >"$EVIDENCE_DIR/gridnetwork-on-failure.json" || true
  if [[ -n "$GRID_RESOURCE" ]]; then
    oc get "$GRID_RESOURCE" -o json 2>/dev/null | jq --arg run "$RUN_LABEL" '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run)|{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,spec:.spec,status:.status}]' >"$EVIDENCE_DIR/task-providers-on-failure.json" || true
  fi
  oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json 2>/dev/null | jq '[.items[-100:][]?|{type,reason,message,lastTimestamp,involvedObject:{kind:.involvedObject.kind,name:.involvedObject.name}}]' >"$EVIDENCE_DIR/task-events-on-failure.json" || true
}

cleanup_task_providers() {
  local provider object
  CURRENT_PHASE=task-provider-cleanup
  for provider in "$PROVIDER_A" "$PROVIDER_B"; do
    if object="$(oc get "$GRID_RESOURCE" "$provider" -o json 2>/dev/null)"; then
      jq -e --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" \
        '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gridNetworkRef==$network' <<<"$object" >/dev/null || {
        log "refusing to delete provider $provider without exact task ownership and GridNetwork reference"; return 1;
      }
      oc delete "$GRID_RESOURCE" "$provider" --wait=true --timeout="$WAIT_TIMEOUT" >"$EVIDENCE_DIR/$provider-delete.txt"
      log "Deleted task-owned InferenceProvider $provider"
    fi
  done
  oc get "$GRID_RESOURCE" -o json | jq --arg run "$RUN_LABEL" '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run)|.metadata.name]' >"$EVIDENCE_DIR/run-providers-after-cleanup.json"
  jq -e 'length==0' "$EVIDENCE_DIR/run-providers-after-cleanup.json" >/dev/null || { log 'task-owned InferenceProvider resources remain after cleanup'; return 1; }
}

api_access
write_invocation
verify_namespace_ownership
verify_grid_operator_runtime
discover_grid_crd
case "$MODE" in
  apply|verify)
    prepare_provider_inputs
    write_source_lock preapply
    apply_or_verify_providers "$MODE"
    CURRENT_PHASE='provider-readiness'
    wait_for_provider_ready "$PROVIDER_A"
    wait_for_provider_ready "$PROVIDER_B"
    capture_current_provider_metrics
    wait_for_grid_overlay
    write_source_lock
    write_result PASS 'both task providers are current-generation Available; authenticated EPP metrics match both Distributed overlay queue-depth scores'
    ;;
  cleanup)
    write_source_lock
    cleanup_task_providers
    write_result CLEANED 'only run-owned InferenceProvider objects were removed; shared Grid/operator resources were preserved'
    ;;
esac
log "Grid provider mode $MODE completed; no inference traffic sent"
find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
