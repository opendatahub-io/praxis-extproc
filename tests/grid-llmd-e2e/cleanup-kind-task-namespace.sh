#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Remove the task Gateway release and namespace after natural Grid withdrawal.
set -Eeuo pipefail
umask 077

MODE="${KIND_TASK_CLEANUP_MODE:-plan}"
CONTEXT="${KUBE_CONTEXT:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-}"
NETWORK_NAME="${GRID_NETWORK_NAME:-}"
SITE_NAME="${GRID_SITE_NAME:-}"
GATEWAY_RELEASE="${GRID_GATEWAY_RELEASE_NAME:-}"
TASK_GATEWAY="${GRID_TASK_GATEWAY_NAME:-grid-llmd-ingress}"
EPP_CLUSTER_ROLE="${GRID_EPP_METRICS_CLUSTER_ROLE_NAME:-grid-llmd-kind-epp-auth}"
WAIT_SECONDS="${KIND_TASK_CLEANUP_WAIT_SECONDS:-300}"
EVIDENCE_DIR="${KIND_TASK_CLEANUP_EVIDENCE_DIR:-}"

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Plan: verify GridNetwork/GridSite/providers/LLMIServices/AITenants are absent,
uninstall the task Gateway Helm release, wait for its Istio data plane to vanish,
delete only the run-owned EPP metrics ClusterRole/Binding, then UID-precondition
the run-owned task namespace delete. No finalizers are removed.
PLAN
  exit 0
fi
[[ "$MODE" == fresh || "$MODE" == resume ]] || { printf 'KIND_TASK_CLEANUP_MODE must be plan, fresh, or resume\n' >&2; exit 2; }
for bin in oc helm jq curl; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ -n "$CONTEXT" && -n "$EXPECTED_SERVER" && -n "$RUN_LABEL" && -n "$NAMESPACE" && -n "$NETWORK_NAME" && -n "$SITE_NAME" && -n "$GATEWAY_RELEASE" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, EXPECTED_API_SERVER, GRID_RUN_LABEL, GRID_RESOURCE_NAMESPACE, GRID_NETWORK_NAME, GRID_SITE_NAME, GRID_GATEWAY_RELEASE_NAME, and KIND_TASK_CLEANUP_EVIDENCE_DIR are required\n' >&2
  exit 2
}
[[ "$WAIT_SECONDS" =~ ^[1-9][0-9]{0,3}$ ]] || { printf 'KIND_TASK_CLEANUP_WAIT_SECONDS must be 1..9999\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

oc() { command oc --context="$CONTEXT" --request-timeout=20s "$@"; }
helm() { command helm --kube-context="$CONTEXT" "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
fail() { log "BLOCKED: $*"; exit 1; }
API_PROXY_PID=""
API_PROXY_PORT=""
stop_api_proxy() {
  if [[ -n "$API_PROXY_PID" ]]; then
    kill "$API_PROXY_PID" 2>/dev/null || true
    wait "$API_PROXY_PID" 2>/dev/null || true
    API_PROXY_PID=""
  fi
}
start_api_proxy() {
  [[ -n "$API_PROXY_PID" ]] && return 0
  command oc --context="$CONTEXT" proxy --address=127.0.0.1 --port=0 >"$EVIDENCE_DIR/api-proxy.log" 2>&1 &
  API_PROXY_PID=$!
  for _ in $(seq 1 50); do
    API_PROXY_PORT="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "$EVIDENCE_DIR/api-proxy.log" | head -n 1)"
    if [[ -n "$API_PROXY_PORT" ]] && curl -fsS --max-time 1 "http://127.0.0.1:$API_PROXY_PORT/readyz" >/dev/null 2>&1; then return 0; fi
    sleep 0.2
  done
  fail 'loopback API proxy did not become ready'
}
uid_precondition_delete() {
  local api_path="$1" kind="$2" name="$3" expected_uid="$4" object_file="$5" response_file="$6"
  local live_uid http_status object
  object="$(oc get "$kind" "$name" -n "$NAMESPACE" --ignore-not-found -o json)" || fail "could not read $kind/$name before UID-preconditioned deletion"
  if [[ -z "$object" ]]; then
    printf '{"alreadyAbsent":true,"kind":"%s","name":"%s"}\n' "$kind" "$name" >"$response_file"
    return 0
  fi
  printf '%s\n' "$object" >"$object_file"
  jq -e --arg run "$RUN_LABEL" --arg uid "$expected_uid" '.metadata.uid==$uid and .metadata.labels["praxis.fast/qualification-run"]==$run' \
    "$object_file" >/dev/null || fail "$kind/$name UID or run ownership changed before delete"
  live_uid="$(jq -r '.metadata.uid' "$object_file")"
  [[ "$live_uid" == "$expected_uid" ]] || fail "$kind/$name UID changed before delete"
  jq -n --arg uid "$expected_uid" '{apiVersion:"v1",kind:"DeleteOptions",preconditions:{uid:$uid}}' >"$response_file.options.json"
  http_status="$(curl -sS --max-time 30 -o "$response_file" -w '%{http_code}' \
    -H 'Content-Type: application/json' -X DELETE --data-binary "@$response_file.options.json" \
    "http://127.0.0.1:$API_PROXY_PORT$api_path")" || fail "UID-preconditioned $kind/$name deletion failed"
  [[ "$http_status" == 200 || "$http_status" == 202 ]] || fail "$kind/$name delete returned HTTP $http_status"
  jq -e --arg uid "$expected_uid" 'if .metadata.uid then .metadata.uid==$uid else .kind=="Status" and .status=="Success" end' \
    "$response_file" >/dev/null || fail "$kind/$name deletion response was invalid"
}
trap stop_api_proxy EXIT

server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_SERVER" ]] || fail 'API server identity mismatch'
[[ "$(oc get --raw=/readyz)" == *ok* ]] || fail 'API /readyz is not ok'
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace-before.json" || fail 'task namespace is absent'
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"' \
  "$EVIDENCE_DIR/task-namespace-before.json" >/dev/null || fail 'task namespace ownership check failed'
NAMESPACE_UID="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/task-namespace-before.json")"

for kind_name in "gridnetwork $NETWORK_NAME" "gridsite $SITE_NAME"; do
  read -r kind name <<<"$kind_name"
  if oc get "$kind" "$name" -o name >"$EVIDENCE_DIR/remaining-$kind.txt" 2>/dev/null; then
    fail "$kind/$name remains; natural Grid cleanup must finish first"
  fi
done
for resource in inferenceproviders.grid.praxis.fast llminferenceservices.serving.kserve.io aitenants.maas.opendatahub.io; do
  oc get "$resource" -A -l "praxis.fast/qualification-run=$RUN_LABEL" -o json >"$EVIDENCE_DIR/$resource.json" || fail "could not list $resource"
  jq -e '.items|length==0' "$EVIDENCE_DIR/$resource.json" >/dev/null || fail "run-owned $resource objects remain"
done

# Metadata-only inventory. Secret and ConfigMap values are deliberately excluded.
for resource in configmaps secrets serviceaccounts roles rolebindings services deployments replicasets pods gateways.gateway.networking.k8s.io httproutes.gateway.networking.k8s.io envoyfilters.networking.istio.io; do
  file="$(printf '%s' "$resource" | tr '/.' '__')"
  oc get "$resource" -n "$NAMESPACE" -l "praxis.fast/qualification-run=$RUN_LABEL" -o json 2>/dev/null |
    jq '[.items[]|{kind,namespace:.metadata.namespace,name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,
      labels:.metadata.labels,annotations:.metadata.annotations,finalizers:.metadata.finalizers,ownerReferences:.metadata.ownerReferences,
      images:[.spec.template.spec.containers[]?.image],dataKeys:(if .kind=="Secret" or .kind=="ConfigMap" then ((.data//{})|keys) else null end)}]' \
    >"$EVIDENCE_DIR/$file.json" || fail "could not snapshot $resource"
done

gateway_release_was_present=false
helm list -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/helm-releases-before.json" || fail 'could not list Helm releases'
if jq -e --arg release "$GATEWAY_RELEASE" 'any(.[];.name==$release)' "$EVIDENCE_DIR/helm-releases-before.json" >/dev/null; then
  gateway_release_was_present=true
  jq -e --arg release "$GATEWAY_RELEASE" 'any(.[];.name==$release and .status=="deployed" and (.chart|startswith("praxis-gateway-")))' \
    "$EVIDENCE_DIR/helm-releases-before.json" >/dev/null || fail 'task Gateway release has the wrong chart or status'
  helm status "$GATEWAY_RELEASE" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gateway-release-status-before.json" || fail 'Gateway release status is unreadable'
  log "Uninstalling task Gateway release $GATEWAY_RELEASE after GridNetwork withdrawal"
  helm uninstall "$GATEWAY_RELEASE" -n "$NAMESPACE" --wait --timeout="${WAIT_SECONDS}s" >"$EVIDENCE_DIR/gateway-helm-uninstall.txt" || fail 'task Gateway Helm uninstall failed'
  if helm status "$GATEWAY_RELEASE" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gateway-release-status-after.json" 2>/dev/null; then
    fail 'task Gateway Helm release remains after uninstall'
  fi
elif [[ "$MODE" == fresh ]]; then
  fail 'task Gateway Helm release is absent in fresh cleanup mode'
else
  printf '{"status":"already-absent","release":"%s"}\n' "$GATEWAY_RELEASE" >"$EVIDENCE_DIR/gateway-release-status-before.json"
fi

# The Gateway and HTTPRoute are applied by deploy-gateway-path.sh, not owned by
# the Helm release. Delete those exact run-owned objects with UID preconditions
# so the Gateway controller can naturally remove its generated Istio data plane.
GATEWAY_SNAPSHOT="$EVIDENCE_DIR/gateways_gateway_networking_k8s_io.json"
ROUTE_SNAPSHOT="$EVIDENCE_DIR/httproutes_gateway_networking_k8s_io.json"
[[ -s "$GATEWAY_SNAPSHOT" && -s "$ROUTE_SNAPSHOT" ]] || fail 'Gateway API ownership snapshots are missing'
GATEWAY_SNAPSHOT_COUNT="$(jq 'length' "$GATEWAY_SNAPSHOT")"
TASK_GATEWAY_UID="$(jq -r --arg name "$TASK_GATEWAY" '.[]|select(.name==$name)|.uid' "$GATEWAY_SNAPSHOT")"
TASK_ROUTE_COUNT="$(jq --arg run "$RUN_LABEL" '[.[]|select(.labels["praxis.fast/qualification-run"]==$run)]|length' "$ROUTE_SNAPSHOT")"
if [[ "$MODE" == resume && "$GATEWAY_SNAPSHOT_COUNT" == 0 && "$TASK_ROUTE_COUNT" == 0 ]]; then
  # Resume cleanup may skip Gateway deletion only after proving the task never
  # installed its Gateway, route, or controller-generated data plane.
  LIVE_GATEWAY_COUNT="$(oc get gateways.gateway.networking.k8s.io -n "$NAMESPACE" -o json |
    jq --arg name "$TASK_GATEWAY" '[.items[]|select(.metadata.name==$name)]|length')" || fail 'could not inspect live task Gateway objects'
  LIVE_TASK_ROUTE_COUNT="$(oc get httproutes.gateway.networking.k8s.io -n "$NAMESPACE" -o json |
    jq --arg run "$RUN_LABEL" --arg gateway "$TASK_GATEWAY" \
      '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run or any(.spec.parentRefs[]?;.name==$gateway))]|length')" || fail 'could not inspect live task HTTPRoutes'
  LIVE_GENERATED_COUNT="$(oc get services,deployments,pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$TASK_GATEWAY" -o json |
    jq '.items|length')" || fail 'could not inspect generated task Gateway data-plane resources'
  ((LIVE_GATEWAY_COUNT == 0 && LIVE_TASK_ROUTE_COUNT == 0 && LIVE_GENERATED_COUNT == 0)) ||
    fail 'resume-without-Gateway state is inconsistent with live Gateway, HTTPRoute, or data-plane resources'
  jq -n --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg namespace "$NAMESPACE" --arg taskGateway "$TASK_GATEWAY" \
    --argjson runGatewaySnapshotCount "$GATEWAY_SNAPSHOT_COUNT" --argjson runRouteSnapshotCount "$TASK_ROUTE_COUNT" \
    --argjson liveGatewayCount "$LIVE_GATEWAY_COUNT" --argjson liveTaskRouteCount "$LIVE_TASK_ROUTE_COUNT" \
    --argjson liveGeneratedDataPlaneCount "$LIVE_GENERATED_COUNT" \
    '{mode:"resume-no-gateway-installed",observedAt:$observedAt,namespace:$namespace,taskGateway:$taskGateway,
      runGatewaySnapshotCount:$runGatewaySnapshotCount,runRouteSnapshotCount:$runRouteSnapshotCount,
      liveGatewayCount:$liveGatewayCount,liveTaskRouteCount:$liveTaskRouteCount,
      liveGeneratedDataPlaneCount:$liveGeneratedDataPlaneCount}' >"$EVIDENCE_DIR/gateway-cleanup-mode.json"
  printf '{"observedAt":"%s","gatewayObjects":0,"runRoutes":0,"generatedDataPlaneObjects":0,"mode":"resume-no-gateway-installed"}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"$EVIDENCE_DIR/gateway-controller-resource-wait.jsonl"
  log 'No task Gateway was installed; live Gateway, HTTPRoute, and generated data-plane inventories are empty'
else
  [[ -n "$TASK_GATEWAY_UID" && "$TASK_GATEWAY_UID" != null ]] || fail 'task Gateway was not present in the pre-cleanup ownership snapshot'
  [[ "$TASK_ROUTE_COUNT" =~ ^[1-9][0-9]*$ ]] || fail 'run-owned HTTPRoute was not present in the pre-cleanup ownership snapshot'
  start_api_proxy
  while IFS=$'\t' read -r route_name route_uid; do
    [[ -n "$route_name" && -n "$route_uid" && "$route_uid" != null ]] || fail 'invalid task HTTPRoute ownership snapshot'
    uid_precondition_delete "/apis/gateway.networking.k8s.io/v1/namespaces/$NAMESPACE/httproutes/$route_name" \
      httproute "$route_name" "$route_uid" "$EVIDENCE_DIR/httproute-$route_name-before-delete.json" "$EVIDENCE_DIR/httproute-$route_name-delete.json"
  done < <(jq -r --arg run "$RUN_LABEL" '.[]|select(.labels["praxis.fast/qualification-run"]==$run)|[.name,.uid]|@tsv' "$ROUTE_SNAPSHOT")
  uid_precondition_delete "/apis/gateway.networking.k8s.io/v1/namespaces/$NAMESPACE/gateways/$TASK_GATEWAY" \
    gateway "$TASK_GATEWAY" "$TASK_GATEWAY_UID" "$EVIDENCE_DIR/task-gateway-before-delete.json" "$EVIDENCE_DIR/task-gateway-delete.json"

  deadline=$((SECONDS + WAIT_SECONDS))
  : >"$EVIDENCE_DIR/gateway-controller-resource-wait.jsonl"
  while ((SECONDS < deadline)); do
    gateways="$(oc get gateways.gateway.networking.k8s.io -n "$NAMESPACE" -o json | jq --arg name "$TASK_GATEWAY" '[.items[]|select(.metadata.name==$name)]|length')"
    routes="$(oc get httproutes.gateway.networking.k8s.io -n "$NAMESPACE" -o json | jq --arg run "$RUN_LABEL" '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run)]|length')"
    generated="$(oc get services,deployments,pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$TASK_GATEWAY" -o json | jq '.items|length')"
    printf '{"observedAt":"%s","gatewayObjects":%s,"runRoutes":%s,"generatedDataPlaneObjects":%s}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$gateways" "$routes" "$generated" >>"$EVIDENCE_DIR/gateway-controller-resource-wait.jsonl"
    ((gateways + routes + generated == 0)) && break
    sleep 2
  done
  ((gateways + routes + generated == 0)) || fail 'task Gateway or Istio data-plane resources remain'
fi

# Delete only this run's metrics authorization role and binding.
for resource in clusterrole clusterrolebinding; do
  file="$EVIDENCE_DIR/$resource-$EPP_CLUSTER_ROLE.json"
  if object="$(oc get "$resource" "$EPP_CLUSTER_ROLE" -o json 2>/dev/null)"; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' <<<"$object" >/dev/null || fail "$resource/$EPP_CLUSTER_ROLE is not run-owned"
    jq '{kind,metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,finalizers:.metadata.finalizers}}' <<<"$object" >"$file"
    oc delete "$resource" "$EPP_CLUSTER_ROLE" --wait=true --timeout="${WAIT_SECONDS}s" >"$file.delete.txt" || fail "could not delete $resource/$EPP_CLUSTER_ROLE"
  else
    printf '{"absent":true}\n' >"$file"
  fi
done

# Submit a UID-preconditioned DELETE through a loopback API proxy.
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace-before-delete.json" || fail 'task namespace disappeared before final deletion'
jq -e --arg run "$RUN_LABEL" --arg uid "$NAMESPACE_UID" '.metadata.uid==$uid and .metadata.labels["praxis.fast/qualification-run"]==$run' \
  "$EVIDENCE_DIR/task-namespace-before-delete.json" >/dev/null || fail 'task namespace UID or ownership changed before deletion'
start_api_proxy
jq -n --arg uid "$NAMESPACE_UID" '{apiVersion:"v1",kind:"DeleteOptions",preconditions:{uid:$uid}}' >"$EVIDENCE_DIR/namespace-delete-options.json"
http_status="$(curl -sS --max-time 30 -o "$EVIDENCE_DIR/namespace-delete-response.json" -w '%{http_code}' \
  -H 'Content-Type: application/json' -X DELETE --data-binary "@$EVIDENCE_DIR/namespace-delete-options.json" \
  "http://127.0.0.1:$API_PROXY_PORT/api/v1/namespaces/$NAMESPACE")" || fail 'UID-preconditioned namespace deletion failed'
[[ "$http_status" == 200 || "$http_status" == 202 ]] || fail "namespace delete returned HTTP $http_status"
printf '%s\n' "$http_status" >"$EVIDENCE_DIR/namespace-delete-http-status.txt"
jq -e --arg uid "$NAMESPACE_UID" 'if .metadata.uid then .metadata.uid==$uid else .kind=="Status" and .status=="Success" end' \
  "$EVIDENCE_DIR/namespace-delete-response.json" >/dev/null || fail 'namespace deletion response did not match the task UID'
log "UID-preconditioned deletion submitted for task namespace $NAMESPACE"

deadline=$((SECONDS + WAIT_SECONDS))
: >"$EVIDENCE_DIR/namespace-delete-wait.jsonl"
while ((SECONDS < deadline)); do
  if oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace-watch.raw.json" 2>/dev/null; then
    jq --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" '{observedAt:$observedAt,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp,finalizers:(.metadata.finalizers//[])}' \
      "$EVIDENCE_DIR/task-namespace-watch.raw.json" >>"$EVIDENCE_DIR/namespace-delete-wait.jsonl"
    jq -e --arg uid "$NAMESPACE_UID" '.metadata.uid==$uid and .metadata.deletionTimestamp!=null' "$EVIDENCE_DIR/task-namespace-watch.raw.json" >/dev/null || fail 'namespace identity changed during deletion'
  else
    break
  fi
  sleep 2
done
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace-after.json" 2>/dev/null && fail 'task namespace remains after natural deletion wait'
kill "$API_PROXY_PID" 2>/dev/null || true
wait "$API_PROXY_PID" 2>/dev/null || true
jq -n --arg run "$RUN_LABEL" --arg namespace "$NAMESPACE" --arg uid "$NAMESPACE_UID" --arg completedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --argjson gatewayReleaseWasPresent "$gateway_release_was_present" \
  '{status:"task-gateway-and-namespace-removed",runLabel:$run,namespace:$namespace,namespaceUID:$uid,completedAt:$completedAt,
    taskGatewayReleaseWasPresent:$gatewayReleaseWasPresent,naturalGridWithdrawalWasVerifiedBeforeCleanup:true,manualFinalizerRemoval:false}' >"$EVIDENCE_DIR/RESULT.json"
if [[ "$gateway_release_was_present" == true ]]; then
  log 'Task Gateway release, task namespace, and run-owned EPP metrics role/binding were removed; platform dependencies remain until Kind cluster deletion'
else
  log 'Task Gateway release was absent and the live Gateway/HTTPRoute/data-plane inventory was empty; task namespace and run-owned EPP metrics role/binding were removed'
fi
