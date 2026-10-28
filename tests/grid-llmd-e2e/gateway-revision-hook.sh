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

app_pods="$(oc get pods -n "$NAMESPACE" -l app.kubernetes.io/instance=grid-gateway,app.kubernetes.io/name=praxis-gateway -o json)"
app_pod="$(jq -er '
  [.items[] | select(.status.phase=="Running" and any(.status.conditions[]?; .type=="Ready" and .status=="True"))]
  | if length==1 then .[0].metadata.name else error("expected one Ready task Praxis Gateway Pod") end
' <<<"$app_pods")"
app_pod_uid="$(jq -er --arg name "$app_pod" '.items[]|select(.metadata.name==$name)|.metadata.uid' <<<"$app_pods")"
app_pod_json="$(jq -ce --arg name "$app_pod" '.items[]|select(.metadata.name==$name)' <<<"$app_pods")"
praxis_gateway_name="$(jq -er '.metadata.labels["app.kubernetes.io/instance"]' <<<"$app_pod_json")"
serving_config_path="$(jq -er '
  [.spec.containers[]?|select(.name=="praxis")|.env[]?|select(.name=="GRID_SERVING_CONFIG")|.value]
  | unique | if length==1 then .[0] else error("expected one GRID_SERVING_CONFIG path") end
' <<<"$app_pod_json")"
[[ "$serving_config_path" == /* && "$serving_config_path" != *".."* && "${serving_config_path##*/}" == serving-config.json ]] || {
  printf 'task Gateway GRID_SERVING_CONFIG path is invalid\n' >&2
  exit 2
}
serving_mount_path="${serving_config_path%/*}"
serving_volume="$(jq -er --arg mount "$serving_mount_path" '
  [.spec.containers[]?|select(.name=="praxis")|.volumeMounts[]?|select(.mountPath==$mount)|.name]
  | unique | if length==1 then .[0] else error("GRID_SERVING_CONFIG has no unique directory mount") end
' <<<"$app_pod_json")"
serving_config_map="$(jq -er --arg volume "$serving_volume" '
  [.spec.volumes[]?|select(.name==$volume)|.configMap.name]
  | unique | if length==1 then .[0] else error("GRID_SERVING_CONFIG mount is not a ConfigMap") end
' <<<"$app_pod_json")"
serving_config_map_json="$(oc get configmap "$serving_config_map" -n "$NAMESPACE" -o json)"
jq -e --arg network "$network_name" --arg gateway "$praxis_gateway_name" '
  .metadata.labels["grid.praxis.fast/network"]==$network
  and .metadata.labels["grid.praxis.fast/gateway"]==$gateway
  and (.data["serving-config.json"]|type)=="string"
  and (.metadata.annotations["grid.praxis.fast/serving-digest"]|type)=="string"
' <<<"$serving_config_map_json" >/dev/null || { printf 'operator serving ConfigMap identity or digest metadata is invalid\n' >&2; exit 2; }
tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/grid-gateway-revision.XXXXXX")"
chmod 0700 "$tmp_dir"
trap 'rm -rf "$tmp_dir"' EXIT
jq -j '.data["routing-overlay.json"]' <<<"$config_map_json" >"$tmp_dir/routing-overlay.json"
jq -j '.data["serving-config.json"]' <<<"$serving_config_map_json" >"$tmp_dir/serving-config.expected.json"
oc exec "$app_pod" -n "$NAMESPACE" -c praxis -- cat "$serving_config_path" >"$tmp_dir/serving-config.mounted.json"
serving_verification="$(python3 - "$tmp_dir/routing-overlay.json" "$tmp_dir/serving-config.expected.json" "$tmp_dir/serving-config.mounted.json" \
  "$(jq -er '.metadata.annotations["grid.praxis.fast/serving-digest"]' <<<"$serving_config_map_json")" "$EXPECTED_REVISION" <<'PY'
import hashlib
import json
import pathlib
import sys

overlay_path, expected_path, mounted_path, expected_serving_digest, expected_revision = sys.argv[1:]
overlay = json.loads(pathlib.Path(overlay_path).read_text())
expected_bytes = pathlib.Path(expected_path).read_bytes()
mounted_bytes = pathlib.Path(mounted_path).read_bytes()
serving = json.loads(expected_bytes)
envelope_revision = overlay.get("revision", {}).get("value")
if envelope_revision != expected_revision:
    raise SystemExit("Grid overlay revision changed during the Gateway check")
serving_digest = hashlib.sha256(expected_bytes).hexdigest()
if serving_digest != expected_serving_digest:
    raise SystemExit("operator serving ConfigMap digest does not match its data")
if mounted_bytes != expected_bytes:
    raise SystemExit("Gateway mounted serving config differs from the operator ConfigMap")

def identities(candidates):
    rows = []
    for candidate in candidates:
        rows.append({key: candidate.get(key) for key in ("stable_id", "kind", "name", "site", "cluster")})
    return sorted(rows, key=lambda row: (str(row["stable_id"]), str(row["name"]), str(row["cluster"])))

overlay_candidates = identities(overlay.get("overlay", {}).get("candidates", []))
serving_candidates = identities(serving.get("candidates", []))
if not overlay_candidates or overlay_candidates != serving_candidates:
    raise SystemExit("Gateway serving candidate identities do not match the current Grid overlay")
print(json.dumps({
    "revision": envelope_revision,
    "servingConfigDigest": serving_digest,
    "mountedServingConfigMatches": True,
    "overlayCandidateIdentitiesMatch": True,
    "overlayCandidateCount": len(overlay_candidates),
}, separators=(",", ":")))
PY
)"
mounted_revision="$(jq -er '.revision' <<<"$serving_verification")"
serving_config_digest="$(jq -er '.servingConfigDigest' <<<"$serving_verification")"
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
  --arg clusterSha256 "$route_cluster_sha256" --arg servingConfigDigest "$serving_config_digest" --arg phase "$PHASE" \
  --argjson apiCaVerified "$api_ca_verified" --argjson retryLimit "$(jq -r '.retryLimit' <<<"$route")" \
  --argjson backendCount "$backend_count" --argjson candidateCount "$overlay_candidate_count" --argjson counters "$counter_json" \
  '{schema:"praxis-extproc.grid-llmd-gateway-revision/v1",phase:$phase,revision:$revision,podUid:$podUid,proxyPodUid:$proxyPodUid,
    apiCaVerified:$apiCaVerified,routeRetryLimit:$retryLimit,routeBackendCount:$backendCount,routeClusterSha256:$clusterSha256,
    routeBackendServiceMatches:true,servingConfigDigest:$servingConfigDigest,mountedServingConfigMatches:true,
    overlayCandidateIdentitiesMatch:true,overlayCandidateCount:$candidateCount,
    retryAttempts:$counters.retryAttempts,upstreamRequests:$counters.upstreamRequests,
    retryCounterPresent:$counters.retryCounterPresent,requestCounterPresent:$counters.requestCounterPresent,
    counterSource:"envoy_clusters_per_host_rq_total"}' \
  >"$EVIDENCE_DIR/gateway-revision-$PHASE.json"
chmod 0600 "$EVIDENCE_DIR/gateway-revision-$PHASE.json"
cat "$EVIDENCE_DIR/gateway-revision-$PHASE.json"
