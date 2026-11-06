#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Configure MaaS-managed authorization for the two native Kind serving pools.
set -Eeuo pipefail
umask 077

MODE="${KIND_POOL_MAAS_AUTH_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
TASK_NAMESPACE="${GRID_RESOURCE_NAMESPACE:-}"
POOL_SELECTION="${KIND_POOL_MAAS_AUTH_POOL:-both}"
TENANT_NAMESPACE="${MAAS_TENANT_NAMESPACE:-ai-tenants}"
GATEWAY_NAMESPACE="${POOL_GATEWAY_NAMESPACE:-maas-system}"
TIMEOUT="${KIND_POOL_MAAS_AUTH_TIMEOUT:-5m}"
EVIDENCE_DIR="${KIND_POOL_MAAS_AUTH_EVIDENCE_DIR:-}"
SELECTED_POOLS=()

kubectl() { command kubectl --context="$KUBE_CONTEXT" "$@"; }

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Kind MaaS pool authorization plan (no cluster calls):
  1. Select pool-a, pool-b, or both with KIND_POOL_MAAS_AUTH_POOL; default is both.
  2. Verify the exact run-owned task namespace and selected run-owned pool Gateways/routes.
  3. Create or verify MaaS AITenant resources targeting the selected pool Gateways.
  4. Wait for controller-created tenant namespaces and current-generation readiness.
  5. Attach the task KServe routes to matching tenant Gateway policies.
  6. Create a MaaSModelRef, MaaSAuthPolicy, and MaaSSubscription per selected pool.
  7. Require current-generation Ready/Active status for each selected authorization path.
No inference request is sent; this is setup and authorization configuration only.
PLAN
  exit 0
fi

for bin in kubectl jq sha256sum; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ "$MODE" =~ ^(apply|verify|collect)$ ]] || { printf 'unsupported KIND_POOL_MAAS_AUTH_MODE: %s\n' "$MODE" >&2; exit 2; }
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" && -n "$TASK_NAMESPACE" ]] || {
  printf 'EXPECTED_API_SERVER, KUBE_CONTEXT, GRID_RUN_LABEL, and GRID_RESOURCE_NAMESPACE are required\n' >&2; exit 2;
}
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'GRID_RUN_LABEL is invalid\n' >&2; exit 2; }
[[ "$TASK_NAMESPACE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || { printf 'GRID_RESOURCE_NAMESPACE is invalid\n' >&2; exit 2; }
[[ "$TENANT_NAMESPACE" == ai-tenants && "$GATEWAY_NAMESPACE" == maas-system ]] || {
  printf 'this Kind MaaS topology requires ai-tenants and maas-system namespaces\n' >&2; exit 2;
}
case "$POOL_SELECTION" in
  pool-a) SELECTED_POOLS=(pool-a) ;;
  pool-b) SELECTED_POOLS=(pool-b) ;;
  both) SELECTED_POOLS=(pool-a pool-b) ;;
  *) printf 'KIND_POOL_MAAS_AUTH_POOL must be pool-a, pool-b, or both\n' >&2; exit 2 ;;
esac
[[ "$TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'KIND_POOL_MAAS_AUTH_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
case "${TIMEOUT: -1}" in s) TIMEOUT_SECONDS="${TIMEOUT%?}" ;; m) TIMEOUT_SECONDS="$(( ${TIMEOUT%?} * 60 ))" ;; h) TIMEOUT_SECONDS="$(( ${TIMEOUT%?} * 3600 ))" ;; esac
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/evidence/kind-pool-maas-auth-$(date -u +%Y%m%dT%H%M%SZ)"
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
on_error() {
  local code="$1"
  trap - ERR INT TERM
  jq -n --arg mode "$MODE" --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:"blocked",exitCode:$exitCode,evidenceDirectory:$evidence,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
  exit "$code"
}
trap 'on_error $?' ERR
trap 'on_error 130' INT
trap 'on_error 143' TERM

server="$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
ready="$(kubectl get --raw=/readyz)"
[[ "$server" == "$EXPECTED_API_SERVER" && "$ready" == *ok* ]] || { printf 'target API guard failed\n' >&2; exit 1; }
kubectl get namespace "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/task-namespace.json" >/dev/null || {
  printf 'task namespace is not owned by run %s\n' "$RUN_LABEL" >&2; exit 1;
}
kubectl get namespace "$TENANT_NAMESPACE" -o json >"$EVIDENCE_DIR/tenant-admin-namespace.json"
kubectl get namespace "$GATEWAY_NAMESPACE" -o json >"$EVIDENCE_DIR/gateway-namespace.json"

wait_resource() {
  local resource="$1" name="$2" namespace="$3" query="$4" snapshot="$5" expected_gateway="${6:-}" expected_pool="${7:-}" expected_gateway_uid="${8:-}" deadline
  deadline=$((SECONDS + TIMEOUT_SECONDS))
  while (( SECONDS <= deadline )); do
    if kubectl get "$resource" "$name" -n "$namespace" -o json >"$snapshot" 2>"$snapshot.stderr" && \
      jq -e --arg run "$RUN_LABEL" --arg gateway "$expected_gateway" --arg gatewayNs "$GATEWAY_NAMESPACE" --arg pool "$expected_pool" \
        --arg gatewayUid "$expected_gateway_uid" "$query" "$snapshot" >/dev/null 2>&1; then
      return 0
    fi
    (( SECONDS < deadline )) || break
    sleep 3
  done
  kubectl describe "$resource" "$name" -n "$namespace" >"$snapshot.describe" 2>&1 || true
  kubectl get events -n "$namespace" --sort-by=.lastTimestamp >"$snapshot.events" 2>&1 || true
  printf '%s %s/%s did not satisfy its bounded current-generation readiness predicate\n' "$resource" "$namespace" "$name" >&2
  return 1
}

for pool in "${SELECTED_POOLS[@]}"; do
  gateway="grid-llmd-$pool"
  kubectl get gateway "$gateway" -n "$GATEWAY_NAMESPACE" -o json >"$EVIDENCE_DIR/$pool-gateway.json"
  jq -e --arg run "$RUN_LABEL" --arg listener inference '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g |
      any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and
      any(.status.listeners[]?;.name==$listener and any(.conditions[]?;.type=="Accepted" and .status=="True") and any(.conditions[]?;.type=="Programmed" and .status=="True")))
  ' "$EVIDENCE_DIR/$pool-gateway.json" >/dev/null || { printf 'Gateway %s is not run-owned and current-ready\n' "$gateway" >&2; exit 1; }
  kubectl get service "$gateway" -n "$GATEWAY_NAMESPACE" -o json >"$EVIDENCE_DIR/$pool-gateway-alias.json"
  jq -e --arg run "$RUN_LABEL" --arg gateway "$gateway" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .metadata.labels["gateway.networking.k8s.io/gateway-name"]==$gateway and .spec.type=="ClusterIP" and
    any(.spec.ports[]?;.port==443) and any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name==$gateway)
  ' "$EVIDENCE_DIR/$pool-gateway-alias.json" >/dev/null || { printf 'MaaS Gateway alias %s is not run-owned/ready-shaped\n' "$gateway" >&2; exit 1; }
  kubectl get endpointslices -n "$GATEWAY_NAMESPACE" -l "kubernetes.io/service-name=$gateway" -o json >"$EVIDENCE_DIR/$pool-gateway-alias-endpointslices.json"
  jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$EVIDENCE_DIR/$pool-gateway-alias-endpointslices.json" >/dev/null || {
    printf 'MaaS Gateway alias %s has no Ready endpoint\n' "$gateway" >&2; exit 1;
  }

  route="$pool-kserve-route"
  kubectl get httproute "$route" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/$pool-kserve-route.json"
  jq -e --arg gateway "$gateway" --arg gatewayNs "$GATEWAY_NAMESPACE" '
    any(.spec.parentRefs[]?;.name==$gateway and (.namespace // $gatewayNs)==$gatewayNs) and
    any(.status.parents[]?.conditions[]?;.type=="Accepted" and .status=="True") and
    any(.status.parents[]?.conditions[]?;.type=="ResolvedRefs" and .status=="True")
  ' "$EVIDENCE_DIR/$pool-kserve-route.json" >/dev/null || { printf 'KServe route %s is not accepted by its matching MaaS Gateway\n' "$route" >&2; exit 1; }

  aitenant_manifest="$EVIDENCE_DIR/$pool-aitenant.yaml"
  cat >"$aitenant_manifest" <<YAML
apiVersion: maas.opendatahub.io/v1alpha1
kind: AITenant
metadata:
  name: $pool
  namespace: $TENANT_NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  gateway:
    name: $gateway
YAML
  chmod 0600 "$aitenant_manifest"
  if kubectl get aitenant "$pool" -n "$TENANT_NAMESPACE" -o json >"$EVIDENCE_DIR/$pool-aitenant-before.json" 2>/dev/null; then
    jq -e --arg run "$RUN_LABEL" --arg gateway "$gateway" '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gateway.name==$gateway' \
      "$EVIDENCE_DIR/$pool-aitenant-before.json" >/dev/null || { printf 'refusing to modify non-run-owned AITenant %s\n' "$pool" >&2; exit 1; }
  fi
  if [[ "$MODE" == apply ]]; then
    kubectl apply --dry-run=server -f "$aitenant_manifest" >"$EVIDENCE_DIR/$pool-aitenant-dry-run.txt"
    kubectl apply -f "$aitenant_manifest" >"$EVIDENCE_DIR/$pool-aitenant-apply.txt"
  fi
  wait_resource aitenant "$pool" "$TENANT_NAMESPACE" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g)) and
    (.status.tenantNamespace|type=="string" and length>0) and .status.gatewayRef.name==$gateway and .status.gatewayRef.namespace==$gatewayNs
  ' "$EVIDENCE_DIR/$pool-aitenant-live.json" "$gateway"
  # Recheck these values after the wait, so the jq predicate is bound to this pool.
  jq -e --arg run "$RUN_LABEL" --arg gateway "$gateway" --arg gatewayNs "$GATEWAY_NAMESPACE" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g)) and
    (.status.tenantNamespace|type=="string" and length>0) and .status.gatewayRef.name==$gateway and .status.gatewayRef.namespace==$gatewayNs
  ' "$EVIDENCE_DIR/$pool-aitenant-live.json" >/dev/null
  tenant_ns="$(jq -r '.status.tenantNamespace' "$EVIDENCE_DIR/$pool-aitenant-live.json")"
  kubectl get namespace "$tenant_ns" -o json >"$EVIDENCE_DIR/$pool-tenant-namespace-before.json"
  jq -e --arg pool "$pool" '.metadata.labels["ai-gateway.opendatahub.io/tenant"]==$pool' "$EVIDENCE_DIR/$pool-tenant-namespace-before.json" >/dev/null || {
    printf 'AITenant %s tenant namespace does not carry its controller-owned tenant identity\n' "$pool" >&2; exit 1;
  }
  if [[ "$MODE" == apply ]]; then kubectl label namespace "$TASK_NAMESPACE" "praxis.fast/$pool-gateway-access=true" --overwrite >"$EVIDENCE_DIR/$pool-task-route-label.txt"; fi
  kubectl get namespace "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace-after-$pool-label.json"
  jq -e --arg key "praxis.fast/$pool-gateway-access" '.metadata.labels[$key]=="true"' "$EVIDENCE_DIR/task-namespace-after-$pool-label.json" >/dev/null || {
    printf 'task namespace is not admitted by the %s MaaS Gateway listener selector\n' "$pool" >&2; exit 1;
  }
  if [[ "$MODE" == apply ]]; then kubectl label namespace "$tenant_ns" "praxis.fast/$pool-gateway-access=true" --overwrite >"$EVIDENCE_DIR/$pool-tenant-route-label.txt"; fi
  kubectl get namespace "$tenant_ns" -o json >"$EVIDENCE_DIR/$pool-tenant-namespace-live.json"
  jq -e --arg key "praxis.fast/$pool-gateway-access" '.metadata.labels[$key]=="true"' "$EVIDENCE_DIR/$pool-tenant-namespace-live.json" >/dev/null || {
    printf 'tenant namespace %s is not admitted by Gateway %s listener selector\n' "$tenant_ns" "$gateway" >&2; exit 1;
  }

  modelref="$(if [[ "$pool" == pool-a ]]; then printf 'grid-test-model'; else printf 'pool-b-grid-test-model'; fi)"
  modelref_manifest="$EVIDENCE_DIR/$pool-maasmodelref.yaml"
  cat >"$modelref_manifest" <<YAML
apiVersion: maas.opendatahub.io/v1alpha1
kind: MaaSModelRef
metadata:
  name: $modelref
  namespace: $TASK_NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  modelRef:
    kind: LLMInferenceService
    name: $pool
YAML
  chmod 0600 "$modelref_manifest"
  if kubectl get maasmodelref "$modelref" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/$pool-maasmodelref-before.json" 2>/dev/null; then
    jq -e --arg run "$RUN_LABEL" --arg model "$pool" '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.modelRef.kind=="LLMInferenceService" and .spec.modelRef.name==$model' \
      "$EVIDENCE_DIR/$pool-maasmodelref-before.json" >/dev/null || { printf 'refusing to modify non-run-owned MaaSModelRef %s\n' "$modelref" >&2; exit 1; }
  fi
  if [[ "$MODE" == apply ]]; then
    kubectl apply --dry-run=server -f "$modelref_manifest" >"$EVIDENCE_DIR/$pool-maasmodelref-dry-run.txt"
    kubectl apply -f "$modelref_manifest" >"$EVIDENCE_DIR/$pool-maasmodelref-apply.txt"
  fi
  wait_resource maasmodelref "$modelref" "$TASK_NAMESPACE" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="RuntimeReady" and .status=="True" and .observedGeneration==$g)) and
    .status.httpRouteGatewayName==$gateway and .status.resolvedTenantRef==$pool
  ' "$EVIDENCE_DIR/$pool-maasmodelref-runtime-live.json" "$gateway" "$pool"
  jq -e --arg run "$RUN_LABEL" --arg gateway "$gateway" --arg pool "$pool" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="RuntimeReady" and .status=="True" and .observedGeneration==$g)) and
    .status.httpRouteGatewayName==$gateway and .status.resolvedTenantRef==$pool
  ' "$EVIDENCE_DIR/$pool-maasmodelref-runtime-live.json" >/dev/null

  auth_name="$pool-grid-test-model-access"
  auth_manifest="$EVIDENCE_DIR/$pool-maasauthpolicy.yaml"
  cat >"$auth_manifest" <<YAML
apiVersion: maas.opendatahub.io/v1alpha1
kind: MaaSAuthPolicy
metadata:
  name: $auth_name
  namespace: $tenant_ns
  labels:
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  modelRefs:
  - name: $modelref
    namespace: $TASK_NAMESPACE
  subjects:
    groups:
    - name: system:authenticated
YAML
  sub_name="$pool-grid-test-model-subscription"
  sub_manifest="$EVIDENCE_DIR/$pool-maassubscription.yaml"
  cat >"$sub_manifest" <<YAML
apiVersion: maas.opendatahub.io/v1alpha1
kind: MaaSSubscription
metadata:
  name: $sub_name
  namespace: $tenant_ns
  labels:
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  owner:
    groups:
    - name: system:authenticated
  modelRefs:
  - name: $modelref
    namespace: $TASK_NAMESPACE
    tokenRateLimits:
    - limit: 1000
      window: 1m
  priority: 10
YAML
  chmod 0600 "$auth_manifest" "$sub_manifest"
  for item in "maasauthpolicy:$auth_name:$auth_manifest" "maassubscription:$sub_name:$sub_manifest"; do
    IFS=: read -r resource name manifest <<<"$item"
    if kubectl get "$resource" "$name" -n "$tenant_ns" -o json >"$EVIDENCE_DIR/$pool-$resource-before.json" 2>/dev/null; then
      jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/$pool-$resource-before.json" >/dev/null || {
        printf 'refusing to modify non-run-owned %s %s/%s\n' "$resource" "$tenant_ns" "$name" >&2; exit 1;
      }
    fi
    if [[ "$MODE" == apply ]]; then
      kubectl apply --dry-run=server -f "$manifest" >"$EVIDENCE_DIR/$pool-$resource-dry-run.txt"
      kubectl apply -f "$manifest" >"$EVIDENCE_DIR/$pool-$resource-apply.txt"
    fi
  done
  log "$pool MaaS tenant, Gateway policy, model reference and subscription objects are installed or being verified"
done

for pool in "${SELECTED_POOLS[@]}"; do
  tenant_ns="$(kubectl get aitenant "$pool" -n "$TENANT_NAMESPACE" -o jsonpath='{.status.tenantNamespace}')"
  gateway="grid-llmd-$pool"
  modelref="$(if [[ "$pool" == pool-a ]]; then printf 'grid-test-model'; else printf 'pool-b-grid-test-model'; fi)"
  wait_resource "maasauthpolicy" "$pool-grid-test-model-access" "$tenant_ns" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g)) and .status.phase=="Active"
  ' "$EVIDENCE_DIR/$pool-maasauthpolicy-live.json"
  wait_resource "maassubscription" "$pool-grid-test-model-subscription" "$tenant_ns" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g)) and .status.phase=="Active"
  ' "$EVIDENCE_DIR/$pool-maassubscription-live.json"
  wait_resource maasmodelref "$modelref" "$TASK_NAMESPACE" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g)) and
    .status.phase=="Ready" and .status.httpRouteGatewayName==$gateway and .status.resolvedTenantRef==$pool
  ' "$EVIDENCE_DIR/$pool-maasmodelref-live.json" "$gateway" "$pool"
  gateway_maas_auth="${gateway}-maas-auth"
  wait_resource authpolicy "$gateway_maas_auth" "$GATEWAY_NAMESPACE" '
    .metadata.labels["app.kubernetes.io/managed-by"]=="maas-controller" and
    any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name==$gateway and .uid==$gatewayUid) and
    any(.status.conditions[]?;.type=="Accepted" and .status=="True") and
    any(.status.conditions[]?;.type=="Enforced" and .status=="True")
  ' "$EVIDENCE_DIR/$pool-gateway-authpolicy-live.json" "$gateway" "" "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/$pool-gateway.json")"
  gateway_uid="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/$pool-gateway.json")"
  jq -e --arg gateway "$gateway" --arg gatewayUid "$gateway_uid" '
    .metadata.labels["app.kubernetes.io/managed-by"]=="maas-controller" and
    any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name==$gateway and .uid==$gatewayUid) and
    any(.status.conditions[]?;.type=="Accepted" and .status=="True") and
    any(.status.conditions[]?;.type=="Enforced" and .status=="True")
  ' "$EVIDENCE_DIR/$pool-gateway-authpolicy-live.json" >/dev/null || { printf 'Gateway AuthPolicy %s is not Accepted and Enforced\n' "$gateway_maas_auth" >&2; exit 1; }
  jq -n --arg pool "$pool" --arg tenantNamespace "$tenant_ns" --arg gateway "$gateway" --arg modelRef "$modelref" \
    '{pool:$pool,tenantNamespace:$tenantNamespace,gateway:$gateway,modelRef:$modelRef,modelRefReady:true,authPolicyReady:true,subscriptionReady:true,gatewayAuthPolicyAcceptedAndEnforced:true,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/$pool-authorization-result.json"
done

find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
pool_json="$(printf '%s\n' "${SELECTED_POOLS[@]}" | jq -R . | jq -s .)"
jq -n --arg mode "$MODE" --arg evidence "$EVIDENCE_DIR" --argjson pools "$pool_json" \
  '{mode:$mode,status:"ready",evidenceDirectory:$evidence,pools:$pools,inferenceTrafficSent:false}' >"$EVIDENCE_DIR/RESULT.json"
log "Current-generation MaaS pool authorization paths are Ready for ${SELECTED_POOLS[*]}; no inference traffic was sent"
