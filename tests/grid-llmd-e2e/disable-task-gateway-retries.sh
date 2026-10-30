#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Disable retries only on the run-owned Grid inference HTTPRoute.
set -Eeuo pipefail
umask 077

MODE="${GRID_GATEWAY_RETRY_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
INGRESS_GATEWAY="${GRID_LLMD_INGRESS_GATEWAY_NAME:-grid-llmd-ingress}"
GRID_GATEWAY_SERVICE="${GRID_LLMD_GRID_GATEWAY_SERVICE:-grid-llmd-e2e/grid-gateway}"
HOST="${GRID_LLMD_GATEWAY_HOST:-grid-llmd-site.grid.internal}"
FILTER_NAME="${GRID_LLMD_NO_RETRIES_ENVOYFILTER_NAME:-praxis-grid-llmd-no-retries}"
EVIDENCE_DIR="${GRID_GATEWAY_RETRY_EVIDENCE_DIR:-}"

oc() {
  command oc --context="$KUBE_CONTEXT" "$@"
}

if [[ "$MODE" == plan ]]; then
  printf '%s\n' 'Plan: inspect the task Gateway Envoy route, server-side dry-run a run-labeled EnvoyFilter with num_retries=0, apply it, and verify the effective route config.'
  exit 0
fi

for binary in oc jq python3; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ "$MODE" == render || "$MODE" == install || "$MODE" == verify ]] || {
  printf 'unsupported GRID_GATEWAY_RETRY_MODE: %s\n' "$MODE" >&2
  exit 2
}
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" ]] || {
  printf 'EXPECTED_API_SERVER, KUBE_CONTEXT, and GRID_RUN_LABEL are required\n' >&2
  exit 2
}
[[ "$HOST" =~ ^[A-Za-z0-9.-]+$ && "$INGRESS_GATEWAY" =~ ^[a-z0-9-]+$ && "$FILTER_NAME" =~ ^[a-z0-9-]+$ ]] || {
  printf 'Gateway hostname or resource name has unsupported characters\n' >&2
  exit 2
}
[[ "$GRID_GATEWAY_SERVICE" =~ ^[a-z0-9-]+/[a-z0-9-]+$ ]] || {
  printf 'GRID_LLMD_GRID_GATEWAY_SERVICE must be namespace/service\n' >&2
  exit 2
}
server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 2; }
[[ "$(oc get --raw=/readyz)" == *ok* ]] || { printf 'API /readyz failed\n' >&2; exit 2; }
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
if [[ "$insecure" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
    printf 'insecure API TLS requires explicit ephemeral-dev opt-in\n' >&2
    exit 2
  }
fi

[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="evidence/gateway-retries-$(date -u +%Y%m%dT%H%M%SZ)"
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

namespace_json="$(oc get namespace "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' <<<"$namespace_json" >/dev/null || {
  printf 'namespace is not owned by this run\n' >&2
  exit 2
}
route_json="$(oc get httproute grid-llmd-inference -n "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" --arg host "$HOST" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  (.spec.hostnames|index($host))!=null and
  any(.status.parents[]?.conditions[]?; .type=="Accepted" and .status=="True") and
  any(.status.parents[]?.conditions[]?; .type=="ResolvedRefs" and .status=="True")
' <<<"$route_json" >/dev/null || { printf 'HTTPRoute ownership/readiness gate failed\n' >&2; exit 2; }
gateway_json="$(oc get gateway "$INGRESS_GATEWAY" -n "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  any(.status.conditions[]?; .type=="Accepted" and .status=="True") and
  any(.status.conditions[]?; .type=="Programmed" and .status=="True")
' <<<"$gateway_json" >/dev/null || { printf 'Gateway ownership/readiness gate failed\n' >&2; exit 2; }

service_ns="${GRID_GATEWAY_SERVICE%%/*}"
service_name="${GRID_GATEWAY_SERVICE#*/}"
[[ "$service_ns" == "$NAMESPACE" ]] || { printf 'Grid Gateway Service must be in the task namespace\n' >&2; exit 2; }

gateway_pod="$(oc get pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$INGRESS_GATEWAY" -o json | jq -er '
  [.items[] | select(.status.phase=="Running" and any(.status.conditions[]?; .type=="Ready" and .status=="True"))]
  | if length == 1 then .[0].metadata.name else error("expected one ready task Gateway Pod") end
')"
route_proto="$(oc exec "$gateway_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET /config_dump | jq -cer --arg host "$HOST:443" --arg service "$service_name.$service_ns.svc.cluster.local" '
  [.configs[]? | .dynamic_route_configs[]?.route_config as $rc | $rc.virtual_hosts[]? as $vh | $vh.routes[]?
   | select($vh.name==$host and ((.route.cluster // "") | contains($service)))
   | {routeConfiguration:$rc.name,vhost:$vh.name,route:.}]
  | if length == 1 then .[0] else error("expected exactly one route to the task Grid Gateway Service") end
')"
route_config="$(jq -c '{routeConfiguration,vhost,route:.route.name,cluster:.route.route.cluster,numRetries:(.route.route.retry_policy.num_retries // null)}' <<<"$route_proto")"
for field in routeConfiguration vhost; do
  field_value="$(jq -r --arg field "$field" '.[$field]' <<<"$route_proto")"
  [[ "$field_value" =~ ^[A-Za-z0-9_.:-]+$ ]] || { printf 'unsafe Envoy route match value for %s\n' "$field" >&2; exit 2; }
done
printf '%s\n' "$route_config" >"$EVIDENCE_DIR/route-before.json"
printf '%s\n' "$route_proto" | jq '.route' >"$EVIDENCE_DIR/route-proto-before.json"
route_name="$(jq -r '.route.name' <<<"$route_proto")"
[[ "$route_name" =~ ^[A-Za-z0-9_.:-]+$ ]] || { printf 'unsafe Envoy route name\n' >&2; exit 2; }
route_payload="$(jq -c '.route | .route.retry_policy.num_retries=0' <<<"$route_proto")"
[[ "$(jq -r '.route.retry_policy.num_retries' <<<"$route_payload")" == 0 ]] || { printf 'could not set the Envoy route retry count\n' >&2; exit 2; }

filter_path="$EVIDENCE_DIR/$FILTER_NAME.yaml"
jq -n \
  --arg namespace "$NAMESPACE" --arg name "$FILTER_NAME" --arg run "$RUN_LABEL" \
  --arg routeConfig "$(jq -r '.routeConfiguration' <<<"$route_proto")" \
  --arg vhost "$(jq -r '.vhost' <<<"$route_proto")" --arg routeName "$route_name" \
  --argjson route "$route_payload" \
  '{apiVersion:"networking.istio.io/v1alpha3",kind:"EnvoyFilter",metadata:{name:$name,namespace:$namespace,labels:{"app.kubernetes.io/managed-by":"grid-llmd-e2e-run","app.kubernetes.io/part-of":"grid-llmd-e2e","praxis.fast/qualification-run":$run}},spec:{priority:10,workloadSelector:{labels:{"gateway.networking.k8s.io/gateway-name":"grid-llmd-ingress"}},configPatches:[{applyTo:"HTTP_ROUTE",match:{context:"GATEWAY",routeConfiguration:{name:$routeConfig,vhost:{name:$vhost,route:{name:$routeName}}}},patch:{operation:"REPLACE",value:$route}}]}}' \
  >"$filter_path"

if oc get envoyfilter "$FILTER_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/existing-filter.json" 2>/dev/null; then
  jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/existing-filter.json" >/dev/null || {
    printf 'existing EnvoyFilter does not have matching run ownership\n' >&2
    exit 2
  }
fi
oc apply --dry-run=server -f "$filter_path" -o json >"$EVIDENCE_DIR/server-dry-run.json"
if [[ "$MODE" == render ]]; then
  jq -n --arg status rendered --arg filter "$FILTER_NAME" --arg evidence "$EVIDENCE_DIR" '{status:$status,filter:$filter,evidenceDirectory:$evidence}' >"$EVIDENCE_DIR/RESULT.json"
  exit 0
fi
if [[ "$MODE" == install ]]; then
  oc apply -f "$filter_path" >"$EVIDENCE_DIR/apply.txt"
fi

deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  route_config_after="$(oc exec "$gateway_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET /config_dump | jq -cer --arg host "$HOST:443" --arg service "$service_name.$service_ns.svc.cluster.local" '
    [.configs[]? | .dynamic_route_configs[]?.route_config as $rc | $rc.virtual_hosts[]? as $vh | $vh.routes[]?
     | select($vh.name==$host and ((.route.cluster // "") | contains($service)))
     | {routeConfiguration:$rc.name,vhost:$vh.name,route:.name,cluster:.route.cluster,numRetries:(.route.retry_policy.num_retries // null)}]
    | if length == 1 then .[0] else error("task route missing or ambiguous") end
  ' 2>/dev/null || true)"
  if [[ -n "$route_config_after" ]] && jq -e '.numRetries==0' <<<"$route_config_after" >/dev/null; then
    printf '%s\n' "$route_config_after" >"$EVIDENCE_DIR/route-after.json"
    jq -n --arg status PASS --arg filter "$FILTER_NAME" --arg run "$RUN_LABEL" --arg route "$(jq -r '.route' <<<"$route_config_after")" --argjson numRetries 0 --arg evidence "$EVIDENCE_DIR" \
      '{status:$status,filter:$filter,runLabel:$run,route:$route,numRetries:$numRetries,evidenceDirectory:$evidence,sharedGatewayOrRouteModified:false}' >"$EVIDENCE_DIR/RESULT.json"
    printf 'task HTTPRoute retry policy is disabled; evidence=%s\n' "$EVIDENCE_DIR"
    exit 0
  fi
  sleep 2
done
[[ -n "${route_config_after:-}" ]] && printf '%s\n' "$route_config_after" >"$EVIDENCE_DIR/route-after-last-observation.json"
jq -n --arg status BLOCKED --arg filter "$FILTER_NAME" --arg evidence "$EVIDENCE_DIR" '{status:$status,filter:$filter,evidenceDirectory:$evidence,sharedGatewayOrRouteModified:false}' >"$EVIDENCE_DIR/RESULT.json"
printf 'timed out waiting for the task HTTPRoute to show num_retries=0\n' >&2
exit 1
