#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Prove that the task Praxis Gateway mounted the operator serving config
# generated from the distributed Grid overlay and snapshot upstream-attempt
# counters for the one-backend public HTTPRoute.
set -Eeuo pipefail
umask 077

CONTEXT=""
NAMESPACE=""
EXPECTED_REVISION=""
CONFIG_MAP_NAMESPACE=""
CONFIG_MAP=""
HTTP_ROUTE_NAME="grid-llmd-inference"
GRID_GATEWAY_SERVICE="grid-llmd-e2e/grid-gateway"
EVIDENCE_DIR=""
PHASE="snapshot"
GATEWAY_NAME=""
HOST=""

usage() {
  cat <<'USAGE'
Usage: gateway-revision-hook.sh --context CTX --namespace NS \
  --expected-revision SHA --config-map-namespace NS --config-map NAME \
  --http-route-name NAME --grid-gateway-service NS/NAME \
  --evidence-dir DIR --phase NAME
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --expected-revision) EXPECTED_REVISION="${2:?}"; shift 2 ;;
    --config-map-namespace) CONFIG_MAP_NAMESPACE="${2:?}"; shift 2 ;;
    --config-map) CONFIG_MAP="${2:?}"; shift 2 ;;
    --http-route-name) HTTP_ROUTE_NAME="${2:?}"; shift 2 ;;
    --grid-gateway-service) GRID_GATEWAY_SERVICE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --phase) PHASE="${2:?}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

for binary in oc jq python3 sha256sum; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ -n "$CONTEXT" && -n "$NAMESPACE" && -n "$EXPECTED_REVISION" &&
   -n "$CONFIG_MAP_NAMESPACE" && -n "$CONFIG_MAP" && -n "$EVIDENCE_DIR" ]] || {
  printf 'missing required Gateway revision hook input\n' >&2
  exit 2
}
[[ "$EXPECTED_REVISION" =~ ^[a-fA-F0-9]{64}$ ]] || { printf 'expected revision must be a SHA-256 hex digest\n' >&2; exit 2; }
[[ "$GRID_GATEWAY_SERVICE" =~ ^[a-z0-9-]+/[a-z0-9-]+$ ]] || { printf 'Grid Gateway Service must be namespace/name\n' >&2; exit 2; }
[[ "$PHASE" =~ ^[A-Za-z0-9_.-]+$ ]] || { printf 'unsafe phase name\n' >&2; exit 2; }
[[ -n "${EXPECTED_API_SERVER:-}" ]] || { printf 'EXPECTED_API_SERVER is required\n' >&2; exit 2; }

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
route_json="$(oc get httproute "$HTTP_ROUTE_NAME" -n "$NAMESPACE" -o json)"
run_label="$(jq -er '.metadata.labels["praxis.fast/qualification-run"]' <<<"$namespace_json")"
jq -e --arg run "$run_label" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  any(.status.parents[]?.conditions[]?; .type=="Accepted" and .status=="True") and
  any(.status.parents[]?.conditions[]?; .type=="ResolvedRefs" and .status=="True")
' <<<"$route_json" >/dev/null || { printf 'HTTPRoute ownership/readiness gate failed\n' >&2; exit 2; }
[[ "$(jq -r '.metadata.name' <<<"$route_json")" == "$HTTP_ROUTE_NAME" ]] || { printf 'HTTPRoute identity changed\n' >&2; exit 2; }
backend_count="$(jq '[.spec.rules[]?.backendRefs[]?] | length' <<<"$route_json")"
service_namespace="${GRID_GATEWAY_SERVICE%%/*}"
service_name="${GRID_GATEWAY_SERVICE#*/}"
jq -e --arg name "$service_name" --arg ns "$service_namespace" --arg default_ns "$NAMESPACE" '
  ([.spec.rules[]?.backendRefs[]?] | length)==1 and
  any(.spec.rules[]?.backendRefs[]?; .name==$name and ((.namespace // $default_ns)==$ns))
' <<<"$route_json" >/dev/null || { printf 'HTTPRoute must have exactly one backend, the task Grid Gateway Service\n' >&2; exit 2; }
HOST="$(jq -er '.spec.hostnames | if length==1 then .[0] else error("expected one route hostname") end' <<<"$route_json")"
GATEWAY_NAME="$(jq -er '.spec.parentRefs | map(select((.kind // "Gateway")=="Gateway")) | if length==1 then .[0].name else error("expected one parent Gateway") end' <<<"$route_json")"

config_map_json="$(oc get configmap "$CONFIG_MAP" -n "$CONFIG_MAP_NAMESPACE" -o json)"
jq -e --arg revision "$EXPECTED_REVISION" '
  .data["routing-overlay.json"] as $raw
  | (try ($raw|fromjson) catch null) as $envelope
  | $envelope.revision.value==$revision
    and .metadata.annotations["grid.praxis.fast/overlay-revision"]==$revision
    and .metadata.annotations["grid.praxis.fast/overlay-content-digest"]==$envelope.content_digest.value
' <<<"$config_map_json" >/dev/null || { printf 'Grid overlay ConfigMap does not contain the current expected revision and digest\n' >&2; exit 2; }
network_name="$(jq -er '.metadata.labels["grid.praxis.fast/network"]' <<<"$config_map_json")"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/grid-gateway-revision.XXXXXX")"
chmod 0700 "$tmp_dir"
trap 'rm -rf "$tmp_dir"' EXIT
app_pods="$(oc get pods -n "$NAMESPACE" -l app.kubernetes.io/instance=grid-gateway,app.kubernetes.io/name=praxis-gateway -o json)"
app_pod="$(jq -er '
  [.items[] | select(.status.phase=="Running" and any(.status.conditions[]?; .type=="Ready" and .status=="True"))]
  | if length==1 then .[0].metadata.name else error("expected one Ready task Praxis Gateway Pod") end
' <<<"$app_pods")"
app_pod_uid="$(jq -er --arg name "$app_pod" '.items[]|select(.metadata.name==$name)|.metadata.uid' <<<"$app_pods")"
app_pod_json="$(jq -ce --arg name "$app_pod" '.items[]|select(.metadata.name==$name)' <<<"$app_pods")"
praxis_gateway_name="$(jq -er '.metadata.labels["grid.praxis.fast/gateway"]' <<<"$config_map_json")"
deployment_json="$(oc get deployment "$praxis_gateway_name" -n "$NAMESPACE" -o json)"
deployment_generation="$(jq -er '.metadata.generation' <<<"$deployment_json")"
network_json="$(oc get gridnetwork "$network_name" -o json)"
praxis_config_volume="$(jq -er '
  [.spec.containers[]?|select(.name=="praxis")|.volumeMounts[]?|select(.mountPath=="/etc/praxis")|.name]
  | unique | if length==1 then .[0] else error("expected one /etc/praxis ConfigMap mount") end
' <<<"$app_pod_json")"
praxis_config_map="$(jq -er --arg volume "$praxis_config_volume" '
  [.spec.volumes[]?|select(.name==$volume)|.configMap.name]
  | unique | if length==1 then .[0] else error("/etc/praxis mount is not backed by one ConfigMap") end
' <<<"$app_pod_json")"
praxis_config_map_json="$(oc get configmap "$praxis_config_map" -n "$NAMESPACE" -o json)"
overlay_volume="$(jq -er '
  [.spec.containers[]?|select(.name=="praxis")|.volumeMounts[]?|select(.mountPath=="/etc/praxis/routing")|.name]
  | unique | if length==1 then .[0] else error("expected one direct Grid overlay mount at /etc/praxis/routing") end
' <<<"$app_pod_json")"
mounted_overlay_map="$(jq -er --arg volume "$overlay_volume" '
  [.spec.volumes[]?|select(.name==$volume)|.configMap.name]
  | unique | if length==1 then .[0] else error("Grid overlay mount is not backed by one ConfigMap") end
' <<<"$app_pod_json")"
[[ "$mounted_overlay_map" == "$CONFIG_MAP" ]] || { printf 'Gateway does not directly mount the expected Grid overlay ConfigMap\n' >&2; exit 2; }
jq -j '.data["routing-overlay.json"]' <<<"$config_map_json" >"$tmp_dir/routing-overlay.expected.json"
jq -j '.data["praxis.yaml"]' <<<"$praxis_config_map_json" >"$tmp_dir/praxis.expected.yaml"
oc exec "$app_pod" -n "$NAMESPACE" -c praxis -- cat /etc/praxis/routing/routing-overlay.json >"$tmp_dir/routing-overlay.mounted.json"
oc exec "$app_pod" -n "$NAMESPACE" -c praxis -- cat /etc/praxis/praxis.yaml >"$tmp_dir/praxis.mounted.yaml"
printf '%s\n' "$network_json" >"$tmp_dir/gridnetwork.json"
printf '%s\n' "$deployment_json" >"$tmp_dir/deployment.json"
printf '%s\n' "$praxis_config_map_json" >"$tmp_dir/praxis-configmap.json"
printf '%s\n' "$config_map_json" >"$tmp_dir/overlay-configmap.json"
serving_verification="$(python3 - "$tmp_dir" "$EXPECTED_REVISION" "$praxis_gateway_name" "$NAMESPACE" "$deployment_generation" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
expected_revision, gateway_name, namespace, deployment_generation = sys.argv[2:]
deployment_generation = int(deployment_generation)

def load(name):
    return json.loads((root / name).read_text())

network = load("gridnetwork.json")
deployment = load("deployment.json")
praxis_cm = load("praxis-configmap.json")
overlay_cm = load("overlay-configmap.json")
network_generation = network.get("metadata", {}).get("generation")
network_namespace = network.get("metadata", {}).get("namespace")
if network_namespace not in (None, "", namespace):
    raise SystemExit("GridNetwork is outside the task namespace")
gateway = next((item for item in network.get("spec", {}).get("consumerGateways", []) if item.get("name") == gateway_name and item.get("namespace", namespace) == namespace), None)
if not gateway:
    raise SystemExit("GridNetwork does not declare the mounted Praxis Gateway")
praxis_spec = gateway.get("praxisConfig", {})
base_name = praxis_spec.get("configMapName")
if praxis_spec.get("generate") is not True or not base_name:
    raise SystemExit("GridNetwork is not configured for generated Praxis consumer config")
alternate_name = "grid-consumer-config-" + hashlib.sha256(base_name.encode()).hexdigest()[:16]
if praxis_cm.get("metadata", {}).get("name") not in (base_name, alternate_name):
    raise SystemExit("mounted Praxis ConfigMap is neither the configured base nor its rollout-safe alternate")
labels = praxis_cm.get("metadata", {}).get("labels", {})
if labels.get("app.kubernetes.io/managed-by") != "grid-operator" or labels.get("grid.praxis.fast/network") != network.get("metadata", {}).get("name") or labels.get("grid.praxis.fast/gateway") != gateway_name:
    raise SystemExit("mounted Praxis ConfigMap is not owned by the current GridNetwork Gateway")
praxis_bytes = (praxis_cm.get("data") or {}).get("praxis.yaml", "").encode()
if not praxis_bytes or (root / "praxis.mounted.yaml").read_bytes() != praxis_bytes:
    raise SystemExit("Gateway mounted Praxis config differs from the operator-owned ConfigMap")
praxis_digest = hashlib.sha256(praxis_bytes).hexdigest()
statuses = network.get("status", {}).get("praxisConfigStatus", [])
if not any(item.get("gatewayName") == gateway_name and item.get("namespace") == namespace and item.get("configMapName") == base_name and item.get("phase") == "Rendered" and item.get("observedGeneration") == network_generation for item in statuses):
    raise SystemExit("operator Praxis ConfigMap is not Rendered at the current GridNetwork generation")
mounts = network.get("status", {}).get("mountReconciliationStatus", [])
if not any(item.get("gatewayName") == gateway_name and item.get("namespace") == namespace and item.get("deploymentName") == deployment.get("metadata", {}).get("name") and item.get("phase") == "Ready" and item.get("observedGeneration") == network_generation and item.get("deploymentGeneration") == deployment_generation and item.get("appliedRevision") == praxis_digest for item in mounts):
    raise SystemExit("operator Praxis mount reconciliation is not Ready for the mounted config and current Deployment generation")
if deployment.get("status", {}).get("observedGeneration") != deployment_generation or (deployment.get("status", {}).get("readyReplicas") or 0) < 1:
    raise SystemExit("Gateway Deployment has not observed its current generation")

overlay_bytes = (overlay_cm.get("data") or {}).get("routing-overlay.json", "").encode()
if not overlay_bytes or (root / "routing-overlay.mounted.json").read_bytes() != overlay_bytes:
    raise SystemExit("Gateway mounted Grid overlay differs from the current ConfigMap")
envelope = json.loads(overlay_bytes)
revision = envelope.get("revision", {}).get("value")
content_digest = envelope.get("content_digest", {}).get("value")
annotations = overlay_cm.get("metadata", {}).get("annotations", {})
overlay_labels = overlay_cm.get("metadata", {}).get("labels", {})
if revision != expected_revision or annotations.get("grid.praxis.fast/overlay-revision") != revision or annotations.get("grid.praxis.fast/overlay-content-digest") != content_digest:
    raise SystemExit("mounted Grid overlay revision or content digest is not the expected Distributed revision")
if overlay_labels.get("grid.praxis.fast/network") != network.get("metadata", {}).get("name") or overlay_labels.get("grid.praxis.fast/gateway") != gateway_name:
    raise SystemExit("mounted Grid overlay is not owned by the expected GridNetwork Gateway")
routing = next((item for item in network.get("status", {}).get("routingMapStatus", []) if item.get("gatewayName") == gateway_name and item.get("namespace") == namespace), None)
if not routing or routing.get("phase") != "Distributed" or routing.get("observedGeneration") != network_generation or routing.get("distributedRevision") != revision or routing.get("configMapName") != overlay_cm.get("metadata", {}).get("name"):
    raise SystemExit("GridNetwork does not report this overlay as Distributed at the current generation")
candidates = envelope.get("overlay", {}).get("candidates", [])
if len(candidates) != 2 or len({item.get("stable_id") for item in candidates}) != 2:
    raise SystemExit("Gateway overlay does not contain exactly two distinct candidates")
print(json.dumps({"revision": revision, "praxisConfigDigest": praxis_digest, "mountedPraxisConfigMatches": True, "overlayCandidateIdentitiesMatch": True, "overlayCandidateCount": len(candidates)}, separators=(",", ":")))
PY
)"
mounted_revision="$(jq -er '.revision' <<<"$serving_verification")"
praxis_config_digest="$(jq -er '.praxisConfigDigest' <<<"$serving_verification")"
overlay_candidate_count="$(jq -er '.overlayCandidateCount' <<<"$serving_verification")"
[[ "$mounted_revision" == "$EXPECTED_REVISION" && "$overlay_candidate_count" == 2 ]] || {
  printf 'Gateway mounted serving config does not represent the expected two-candidate Grid overlay\n' >&2
  exit 2
}

proxy_pods="$(oc get pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$GATEWAY_NAME" -o json)"
proxy_pod="$(jq -er '
  [.items[] | select(.status.phase=="Running" and any(.status.conditions[]?; .type=="Ready" and .status=="True"))]
  | if length==1 then .[0].metadata.name else error("expected one Ready task Gateway proxy Pod") end
' <<<"$proxy_pods")"
proxy_pod_uid="$(jq -er --arg name "$proxy_pod" '.items[]|select(.metadata.name==$name)|.metadata.uid' <<<"$proxy_pods")"
service_cluster="$service_name.$service_namespace.svc.cluster.local"
config_dump="$(oc exec "$proxy_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET /config_dump)"
route="$(jq -cer --arg host "$HOST:443" --arg service "$service_cluster" '
  [.configs[]? | .dynamic_route_configs[]?.route_config as $rc | $rc.virtual_hosts[]? as $vh | $vh.routes[]?
   | select($vh.name==$host and ((.route.cluster // "")|contains($service)))
   | {routeConfiguration:$rc.name,vhost:$vh.name,routeName:.name,cluster:.route.cluster,
      retryLimit:(.route.retry_policy.num_retries // 0)}]
  | if length==1 then .[0] else error("expected exactly one effective route to the task Gateway Service") end
' <<<"$config_dump")"
[[ "$(jq -r '.retryLimit|type' <<<"$route")" == number ]] || { printf 'effective Envoy retry limit is not numeric\n' >&2; exit 2; }

route_cluster="$(jq -er '.cluster' <<<"$route")"
route_cluster_sha256="$(printf '%s' "$route_cluster" | sha256sum | awk '{print $1}')"
counter_json="$(oc exec "$proxy_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET '/clusters?format=json' | python3 -c '
import json
import sys

cluster_name = sys.argv[1]
document = json.load(sys.stdin)
clusters = [item for item in document.get("cluster_statuses", []) if item.get("name") == cluster_name]
if len(clusters) != 1:
    raise SystemExit("Envoy did not expose exactly one route cluster in /clusters")
hosts = clusters[0].get("host_statuses", [])
if not hosts:
    raise SystemExit("Envoy route cluster has no upstream host status")

def counter_values(name):
    return [int(stat["value"]) for host in hosts for stat in host.get("stats", []) if stat.get("name") == name and "value" in stat]

request_values = counter_values("rq_total")
retry_values = counter_values("rq_retry")
request_counter_present = all(
    any(stat.get("name") in {"rq_active", "rq_total"} for stat in host.get("stats", []))
    for host in hosts
)
values = {
    "retryAttempts": sum(retry_values) if retry_values else None,
    "retryCounterPresent": bool(retry_values),
    "upstreamRequests": sum(request_values),
    "requestCounterPresent": request_counter_present,
}
print(json.dumps(values, separators=(",", ":")))
' "$route_cluster")"

jq -n \
  --arg revision "$mounted_revision" --arg podUid "$app_pod_uid" --arg proxyPodUid "$proxy_pod_uid" \
  --arg clusterSha256 "$route_cluster_sha256" --arg praxisConfigDigest "$praxis_config_digest" --arg phase "$PHASE" \
  --argjson apiCaVerified "$api_ca_verified" --argjson retryLimit "$(jq -r '.retryLimit' <<<"$route")" \
  --argjson backendCount "$backend_count" --argjson candidateCount "$overlay_candidate_count" --argjson counters "$counter_json" \
  '{schema:"praxis-extproc.grid-llmd-gateway-revision/v1",phase:$phase,revision:$revision,podUid:$podUid,proxyPodUid:$proxyPodUid,
    apiCaVerified:$apiCaVerified,routeRetryLimit:$retryLimit,routeBackendCount:$backendCount,routeClusterSha256:$clusterSha256,
    routeBackendServiceMatches:true,praxisConfigDigest:$praxisConfigDigest,mountedPraxisConfigMatches:true,
    overlayCandidateIdentitiesMatch:true,overlayCandidateCount:$candidateCount,
    retryAttempts:$counters.retryAttempts,upstreamRequests:$counters.upstreamRequests,
    retryCounterPresent:$counters.retryCounterPresent,requestCounterPresent:$counters.requestCounterPresent,
    counterSource:"envoy_clusters_per_host_rq_total"}' \
  >"$EVIDENCE_DIR/gateway-revision-$PHASE.json"
chmod 0600 "$EVIDENCE_DIR/gateway-revision-$PHASE.json"
cat "$EVIDENCE_DIR/gateway-revision-$PHASE.json"
