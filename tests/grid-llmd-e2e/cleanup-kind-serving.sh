#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Remove this run's KServe/MaaS serving graphs before natural Grid withdrawal.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CONTEXT="${KUBE_CONTEXT:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
KIND_CLUSTER_NAME="${KIND_CLUSTER_NAME:-}"
E2E_RUN_ID="${E2E_RUN_ID:-}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
EVIDENCE_DIR="${KIND_SERVING_CLEANUP_EVIDENCE_DIR:-}"
WAIT_SECONDS="${KIND_SERVING_CLEANUP_WAIT_SECONDS:-600}"
CLEANUP_MODE="${KIND_SERVING_CLEANUP_MODE:-fresh}"
POOL_A_GATEWAY="${POOL_A_GATEWAY_NAME:-grid-llmd-pool-a}"
POOL_B_GATEWAY="${POOL_B_GATEWAY_NAME:-grid-llmd-pool-b}"

[[ -n "$CONTEXT" && -n "$EXPECTED_SERVER" && -n "$NAMESPACE" && -n "$RUN_LABEL" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, EXPECTED_API_SERVER, GRID_RESOURCE_NAMESPACE, GRID_RUN_LABEL, and KIND_SERVING_CLEANUP_EVIDENCE_DIR are required\n' >&2
  exit 2
}
[[ "$WAIT_SECONDS" =~ ^[1-9][0-9]{0,3}$ ]] || { printf 'KIND_SERVING_CLEANUP_WAIT_SECONDS must be 1..9999\n' >&2; exit 2; }
[[ "$CLEANUP_MODE" == fresh || "$CLEANUP_MODE" == resume ]] || { printf 'KIND_SERVING_CLEANUP_MODE must be fresh or resume\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'cleanup evidence path already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

oc() { command oc --context="$CONTEXT" --request-timeout=20s "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
fail() { log "BLOCKED: $*"; exit 1; }

MAAS_CLEANUP_STATE_FILE="${KIND_MAAS_CLEANUP_STATE_FILE:-}"
MAAS_CLEANUP_PREPARED=false
MAAS_CLEANUP_RESTORE_PENDING=false

run_maas_cleanup_helper() {
  local mode="$1" evidence_dir="$2"
  KIND_MAAS_CLEANUP_MODE="$mode" \
    KUBE_CONTEXT="$CONTEXT" \
    EXPECTED_API_SERVER="$EXPECTED_SERVER" \
    E2E_RUN_ID="${E2E_RUN_ID:-$RUN_LABEL}" \
    GRID_RUN_LABEL="$RUN_LABEL" \
    KIND_CLUSTER_NAME="$KIND_CLUSTER_NAME" \
    KIND_MAAS_CLEANUP_STATE_FILE="$MAAS_CLEANUP_STATE_FILE" \
    KIND_MAAS_CLEANUP_EVIDENCE_DIR="$evidence_dir" \
    "$SCRIPT_DIR/prepare-kind-maas-cleanup.sh"
}

prepare_maas_cleanup_if_needed() {
  [[ "$CLUSTER_ENV" == kind ]] || return 0
  [[ -n "$KIND_CLUSTER_NAME" ]] || fail 'KIND_CLUSTER_NAME is required for Kind MaaS cleanup'
  [[ -n "$MAAS_CLEANUP_STATE_FILE" ]] || fail 'KIND_MAAS_CLEANUP_STATE_FILE must reuse the private state journal created by the Kind MaaS preparation step'
  local tenants state
  tenants="$(oc get aitenants.maas.opendatahub.io -n ai-tenants -o json | jq --arg run "$RUN_LABEL" \
    '[.items[]|select((.metadata.name=="pool-a" or .metadata.name=="pool-b") and .metadata.labels["praxis.fast/qualification-run"]==$run)]|length')"
  if [[ -f "$MAAS_CLEANUP_STATE_FILE" ]]; then
    state="$(cat "$MAAS_CLEANUP_STATE_FILE")"
    jq -e --arg run "${E2E_RUN_ID:-$RUN_LABEL}" \
      '.schema=="praxis-extproc.kind-maas-cleanup-state/v2" and .run==$run and .prepared==true' <<<"$state" >/dev/null ||
      fail 'existing MaaS cleanup state belongs to another run, is incomplete, or uses an unsupported schema'
    if jq -e '.restored==false' <<<"$state" >/dev/null; then
      MAAS_CLEANUP_RESTORE_PENDING=true
      [[ "$tenants" == 0 ]] || MAAS_CLEANUP_PREPARED=true
      log 'Reusing the un-restored run-scoped MaaS cleanup journal'
    elif [[ "$tenants" != 0 ]]; then
      fail 'MaaS cleanup was already restored while run-owned AITenants remain'
    fi
  elif [[ "$tenants" != 0 ]]; then
    run_maas_cleanup_helper prepare "$EVIDENCE_DIR/maas-cleanup-prepare"
    MAAS_CLEANUP_PREPARED=true
    MAAS_CLEANUP_RESTORE_PENDING=true
  fi
}

restore_maas_cleanup_if_needed() {
  [[ "$CLUSTER_ENV" == kind && "$MAAS_CLEANUP_RESTORE_PENDING" == true ]] || return 0
  [[ "$MAAS_CLEANUP_PREPARED" == true || -f "$MAAS_CLEANUP_STATE_FILE" ]] ||
    fail 'MaaS cleanup state is missing after tenant finalization'
  run_maas_cleanup_helper restore "$EVIDENCE_DIR/maas-cleanup-restore"
  MAAS_CLEANUP_RESTORE_PENDING=false
}

server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_SERVER" ]] || fail 'API server identity mismatch'
[[ "$(oc get --raw=/readyz)" == *ok* ]] || fail 'API /readyz failed'
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace.json"
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"
' "$EVIDENCE_DIR/task-namespace.json" >/dev/null || fail 'task namespace is not owned by this exact run'
jq '{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels}' "$EVIDENCE_DIR/task-namespace.json" >"$EVIDENCE_DIR/task-namespace-summary.json"

snapshot_owned() {
  local resource="$1" namespace="$2" file="$3"
  local -a ns_args=()
  [[ -z "$namespace" ]] || ns_args=(-n "$namespace")
  oc get "$resource" "${ns_args[@]}" -o json | jq --arg run "$RUN_LABEL" '
    [.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run)|
      {name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,
       labels:.metadata.labels,finalizers:.metadata.finalizers,ownerReferences:.metadata.ownerReferences}]
  ' >"$file"
}

snapshot_owned_pool_gateways() {
  local namespace="$1" file="$2"
  oc get gateways.gateway.networking.k8s.io -n "$namespace" -o json | jq --arg run "$RUN_LABEL" \
    --arg a "$POOL_A_GATEWAY" --arg b "$POOL_B_GATEWAY" '
    [.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and
      (.metadata.name==$a or .metadata.name==$b))|
      {name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,
       generation:.metadata.generation,labels:.metadata.labels,finalizers:.metadata.finalizers}]
  ' >"$file"
}

delete_owned() {
  local resource="$1" name="$2" namespace="$3" required_pool="${4:-}" object file
  local -a ns_args=()
  [[ -z "$namespace" ]] || ns_args=(-n "$namespace")
  file="$EVIDENCE_DIR/$(printf '%s' "$namespace-$resource-$name" | tr '/.' '__').json"
  if ! object="$(oc get "$resource" "$name" "${ns_args[@]}" -o json 2>/dev/null)"; then
    log "$resource $namespace/$name is already absent"
    return 0
  fi
  jq -e --arg run "$RUN_LABEL" --arg pool "$required_pool" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    ($pool=="" or .metadata.labels["praxis.fast/simulator-pool"]==$pool)
  ' <<<"$object" >/dev/null || fail "$resource $namespace/$name is not positively owned by this run"
  jq '{apiVersion,kind,metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,
      generation:.metadata.generation,labels:.metadata.labels,finalizers:.metadata.finalizers,
      ownerReferences:.metadata.ownerReferences}}' <<<"$object" >"$file"
  log "Deleting run-owned $resource $namespace/$name and waiting for natural completion"
  oc delete "$resource" "$name" "${ns_args[@]}" --wait=true --timeout="${WAIT_SECONDS}s" >"$file.delete.txt"
  local remaining
  remaining="$(oc get "$resource" "$name" "${ns_args[@]}" --ignore-not-found -o name)"
  [[ -z "$remaining" ]] || fail "$resource $namespace/$name remains after deletion"
}

wait_pool_children_absent() {
  local deadline=$((SECONDS + WAIT_SECONDS)) remaining pools routes pods
  while ((SECONDS < deadline)); do
    pools="$(oc get inferencepools.inference.networking.k8s.io -n "$NAMESPACE" -o json | jq '[.items[]|select(.metadata.name=="pool-a-inference-pool" or .metadata.name=="pool-b-inference-pool")]|length')"
    routes="$(oc get httproutes.gateway.networking.k8s.io -n "$NAMESPACE" -o json | jq '[.items[]|select(.metadata.name=="pool-a" or .metadata.name=="pool-b")]|length')"
    pods="$(oc get pods -n "$NAMESPACE" -o json | jq '[.items[]|select(any(.spec.containers[]?;.image|test("llm-d-(inference-sim|router-endpoint-picker)@sha256:"))) ]|length')"
    remaining=$((pools + routes + pods))
    printf '{"observedAt":"%s","inferencePools":%s,"poolRoutes":%s,"eppOrSimulatorPods":%s}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$pools" "$routes" "$pods" >>"$EVIDENCE_DIR/serving-child-wait.jsonl"
    ((remaining == 0)) && break
    sleep 3
  done
  ((remaining == 0)) || fail 'KServe did not naturally withdraw both InferencePools, pool routes, EPPs, and simulator Pods'
  log 'KServe-owned InferencePools, pool HTTPRoutes, EPP Pods, and simulator Pods are absent'
}

wait_gateway_services_absent() {
  local namespace="$1" gateway="$2" deadline=$((SECONDS + WAIT_SECONDS)) count
  while ((SECONDS < deadline)); do
    count="$(oc get services -n "$namespace" -l "gateway.networking.k8s.io/gateway-name=$gateway" -o json | jq '.items|length')"
    printf '{"observedAt":"%s","namespace":"%s","gateway":"%s","generatedServices":%s}\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$namespace" "$gateway" "$count" >>"$EVIDENCE_DIR/gateway-service-wait.jsonl"
    ((count == 0)) && return 0
    sleep 3
  done
  fail "Gateway $namespace/$gateway generated Services did not disappear naturally"
}

snapshot_owned llminferenceservices.serving.kserve.io "$NAMESPACE" "$EVIDENCE_DIR/llmiservices-before.json"
llmisvc_count="$(jq 'length' "$EVIDENCE_DIR/llmiservices-before.json")"
if [[ "$CLEANUP_MODE" == fresh ]]; then
  [[ "$llmisvc_count" == 2 ]] || fail 'fresh cleanup requires exactly the two run-owned LLMInferenceServices before cleanup'
else
  [[ "$llmisvc_count" -le 2 ]] || fail 'resume cleanup found more than two run-owned LLMInferenceServices'
  log "Resume mode found $llmisvc_count remaining run-owned LLMInferenceServices"
fi
for pool in pool-a pool-b; do
  delete_owned llminferenceservice "$pool" "$NAMESPACE"
done
wait_pool_children_absent

delete_owned maasmodelref grid-test-model "$NAMESPACE"
delete_owned maasmodelref pool-b-grid-test-model "$NAMESPACE"
# MaaS AITenant finalizers reconcile tenant-owned auth resources through the
# configured pool Gateway. Keep every run-owned pool Gateway until both tenants
# have naturally completed deletion; only then remove the Gateway parents.
prepare_maas_cleanup_if_needed
delete_owned aitenant pool-a ai-tenants
delete_owned aitenant pool-b ai-tenants

# This CA-only Secret is created by the Kind MaaS compatibility setup so the
# AITenant revocation Job can verify the run-local MaaS API certificate. Keep
# it until both tenant cleanup finalizers have completed. The Kind helper owns
# restoring the MaaS controller and UID-preconditioned removal of its Secret.
if [[ "$CLUSTER_ENV" == kind && "$MAAS_CLEANUP_RESTORE_PENDING" == true ]]; then
  restore_maas_cleanup_if_needed
else
  delete_owned secret opendatahub-ca maas-system
fi

for ref in \
  "$NAMESPACE/$POOL_A_GATEWAY/pool-a" \
  "$NAMESPACE/$POOL_B_GATEWAY/pool-b" \
  "maas-system/$POOL_A_GATEWAY/pool-a" \
  "maas-system/$POOL_B_GATEWAY/pool-b"; do
  ns="${ref%%/*}"; remainder="${ref#*/}"; name="${remainder%%/*}"; pool="${remainder#*/}"
  delete_owned gateway "$name" "$ns" "$pool"
  wait_gateway_services_absent "$ns" "$name"
done

snapshot_owned llminferenceservices.serving.kserve.io "$NAMESPACE" "$EVIDENCE_DIR/llmiservices-after.json"
jq -e 'length==0' "$EVIDENCE_DIR/llmiservices-after.json" >/dev/null || fail 'run-owned LLMInferenceServices remain'
resource="gateways.gateway.networking.k8s.io"
for ns in "$NAMESPACE" maas-system; do
  gateway_snapshot="$EVIDENCE_DIR/$(printf '%s-%s-after.json' "$ns" "$resource" | tr '/.' '__')"
  snapshot_owned_pool_gateways "$ns" "$gateway_snapshot"
  jq -e 'length==0' "$gateway_snapshot" >/dev/null || fail "run-owned pool Gateways remain in $ns after cleanup"
done
jq -n --arg run "$RUN_LABEL" --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  --arg mode "$CLEANUP_MODE" '{status:"serving-resources-removed",mode:$mode,runLabel:$run,observedAt:$observedAt,
    providersMustAlreadyBeRemoved:true,gridNetworkPreservedForNaturalWithdrawal:true}' >"$EVIDENCE_DIR/RESULT.json"
log 'Run-owned serving graphs, tenant auth resources, and pool Gateways were removed; task GridNetwork and task Gateway remain for ordered natural Grid cleanup'
