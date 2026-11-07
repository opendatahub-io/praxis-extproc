#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Send one MaaS-authorized setup probe through one pool Gateway.
set -Eeuo pipefail
umask 077
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE="${KIND_POOL_SETUP_PROBE_MODE:-plan}"
POOL="${KIND_POOL_SETUP_PROBE_POOL:-}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
TASK_NAMESPACE="${GRID_RESOURCE_NAMESPACE:-}"
EXPECTED_SIMULATOR_DIGEST="${EXPECTED_SIMULATOR_DIGEST:-}"
EXPECTED_EPP_DIGEST="${EXPECTED_EPP_DIGEST:-}"
EXPECTED_API_DIGEST="${EXPECTED_API_DIGEST:-}"
EXPECTED_RESPONSE_MODEL="${EXPECTED_RESPONSE_MODEL:-}"
PRIVATE_STATE_DIR="${KIND_POOL_SETUP_PROBE_STATE_DIR:-}"
EVIDENCE_DIR="${KIND_POOL_SETUP_PROBE_EVIDENCE_DIR:-}"
API_KEY=""
API_KEY_ID=""
API_URL=""
API_TLS_HOST=""
API_CA_FILE=""
API_LOCAL_PORT=""
API_FORWARD_PID=""
GATEWAY_LOCAL_PORT=""
GATEWAY_FORWARD_PID=""
KEY_REVOKED=false
PROBE_ID=""
CLIENT_REQUEST_ID=""
PROBE_USERNAME="kind-e2e-client"
PROBE_GROUPS='["system:authenticated"]'

kubectl() { command kubectl --context="$KUBE_CONTEXT" "$@"; }

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Kind pool Gateway setup-probe plan (no cluster calls):
  1. Verify the run-owned KServe route, LLMInferenceService, InferencePool, MaaS tenant, model reference, policies and subscription.
  2. Verify one source-pinned simulator Pod and EPP Pod per pool, with disjoint simulator UIDs.
  3. Use a separate verified-TLS port-forward to the pool MaaS API service to mint a short-lived key with a test identity.
  4. POST one /v1/chat/completions request with the MaaSModelRef's canonical publisher model in the body; trusted preprocessing supplies the internal routing header.
  5. Match the response request ID to a completion receipt from the independent simulator Pod.
  6. Revoke the ephemeral MaaS API key over the verified-TLS API connection and stop both run-local port forwards.
This is a direct pool setup probe. It does not traverse Grid, a provider proxy, the task Gateway, or ExtProc and is not qualification traffic.
Only credential provisioning uses the pool API backchannel; inference still traverses the pool Gateway's MaaS-managed authorization policy.
PLAN
  exit 0
fi

for bin in kubectl jq curl rg python3 timeout; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ "$MODE" == run ]] || { printf 'unsupported KIND_POOL_SETUP_PROBE_MODE: %s\n' "$MODE" >&2; exit 2; }
[[ "$POOL" == pool-a || "$POOL" == pool-b ]] || { printf 'KIND_POOL_SETUP_PROBE_POOL must be pool-a or pool-b\n' >&2; exit 2; }
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" && -n "$TASK_NAMESPACE" ]] || {
  printf 'EXPECTED_API_SERVER, KUBE_CONTEXT, GRID_RUN_LABEL, and GRID_RESOURCE_NAMESPACE are required\n' >&2; exit 2;
}
[[ "$EXPECTED_SIMULATOR_DIGEST" =~ ^sha256:[a-f0-9]{64}$ && "$EXPECTED_EPP_DIGEST" =~ ^sha256:[a-f0-9]{64}$ && "$EXPECTED_API_DIGEST" =~ ^sha256:[a-f0-9]{64}$ ]] || {
  printf 'fresh simulator, EPP, and MaaS API image digests are required\n' >&2; exit 2;
}
[[ -n "$EXPECTED_RESPONSE_MODEL" ]] || { printf 'EXPECTED_RESPONSE_MODEL is required\n' >&2; exit 2; }
[[ -n "$PRIVATE_STATE_DIR" && -n "$EVIDENCE_DIR" && ! -e "$EVIDENCE_DIR" ]] || {
  printf 'provide a private state directory and a new evidence directory\n' >&2; exit 2;
}
mkdir -p "$PRIVATE_STATE_DIR/tls" "$EVIDENCE_DIR"
chmod 0700 "$PRIVATE_STATE_DIR" "$PRIVATE_STATE_DIR/tls" "$EVIDENCE_DIR"
API_DEPLOYMENT="maas-api-$POOL"
API_SERVICE="maas-api-$POOL"
API_TLS_HOST="$API_SERVICE.maas-system.svc.cluster.local"
API_CA_FILE="${KIND_POOL_SETUP_PROBE_API_CA_FILE:-$PRIVATE_STATE_DIR/maas-api-root-ca.crt}"
API_ROUTE="maas-api-route-$POOL"
GATEWAY="grid-llmd-$POOL"
SIMULATOR_DEPLOYMENT="$POOL-kserve"
EPP_DEPLOYMENT="$POOL-kserve-router-scheduler"
MODELREF="$(if [[ "$POOL" == pool-a ]]; then printf 'grid-test-model'; else printf 'pool-b-grid-test-model'; fi)"
SUBSCRIPTION="$POOL-grid-test-model-subscription"
AUTH_POLICY="$POOL-grid-test-model-access"
GATEWAY_AUTH_POLICY="$GATEWAY-maas-auth"
AUTH_STABLE_TIMEOUT_SECONDS="${KIND_POOL_AUTH_STABLE_TIMEOUT_SECONDS:-120}"
[[ "$AUTH_STABLE_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || { printf 'KIND_POOL_AUTH_STABLE_TIMEOUT_SECONDS must be a positive integer\n' >&2; exit 2; }

log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
capture_probe_failure() {
  local dir pod
  [[ -n "$GATEWAY" && -n "$TASK_NAMESPACE" && -n "${TENANT_NAMESPACE:-}" ]] || return 0
  dir="$EVIDENCE_DIR/failure-diagnostics"
  mkdir -p "$dir"
  chmod 0700 "$dir"
  set +e
  kubectl get gateway "$GATEWAY" -n maas-system -o json >"$dir/pool-gateway.json" 2>&1
  kubectl get httproute "$API_ROUTE" -n maas-system -o json >"$dir/maas-api-route.json" 2>&1
  kubectl get httproute "$POOL-kserve-route" -n "$TASK_NAMESPACE" -o json >"$dir/kserve-route.json" 2>&1
  kubectl get authpolicy "$GATEWAY_AUTH_POLICY" -n maas-system -o json >"$dir/gateway-auth-policy.json" 2>&1
  kubectl get maasauthpolicy "$AUTH_POLICY" -n "$TENANT_NAMESPACE" -o json >"$dir/maas-auth-policy.json" 2>&1
  kubectl get authpolicies.kuadrant.io -A -o json \
    | jq '[.items[] | {namespace:.metadata.namespace,name:.metadata.name,targetRef:.spec.targetRef,conditions:[.status.conditions[]? | {type,status,observedGeneration,reason}]}]' \
    >"$dir/all-authpolicies.json" 2>&1
  kubectl get wasmplugin "kuadrant-$GATEWAY" -n maas-system -o json >"$dir/gateway-wasm-plugin.json" 2>&1
  kubectl get envoyfilter "kuadrant-auth-$GATEWAY" -n maas-system -o json >"$dir/gateway-auth-envoyfilter.json" 2>&1
  kubectl get pods,deployments,services,endpointslices -n maas-system -o json \
    | jq '[.items[] | {kind,metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels},status:.status,spec:{selector:.spec.selector,replicas:.spec.replicas,containers:[.spec.template.spec.containers[]? | {name,image}]}}]' \
    >"$dir/maas-routing-resources.json" 2>&1
  for pod in $(kubectl get pods -n maas-system -l "gateway.networking.k8s.io/gateway-name=$GATEWAY" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
    kubectl logs -n maas-system "pod/$pod" -c istio-proxy --since=15m >"$dir/$pod-istio-proxy.log" 2>&1
  done
  for pod in $(kubectl get pods -n maas-system -l "maas.opendatahub.io/tenant-instance=$API_DEPLOYMENT" -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null); do
    kubectl logs -n maas-system "pod/$pod" -c maas-api --since=15m >"$dir/$pod-maas-api.log" 2>&1
  done
  kubectl logs -n kuadrant-system deployment/authorino --all-containers=true --since=15m >"$dir/authorino.log" 2>&1
  for file in "$dir"/*; do
    [[ -f "$file" ]] || continue
    python3 "$SCRIPT_DIR/sanitize_diagnostics.py" <"$file" >"$file.sanitized" 2>/dev/null && mv "$file.sanitized" "$file"
  done
  chmod -R go-rwx "$dir"
  set -e
}
wait_for_stable_auth_policy() {
  local deadline previous='' fingerprint stable=0 maas_policy gateway_policy policies wasm_plugin envoy_filter snapshot
  local maas_generation gateway_generation maas_ready no_conflict gateway_current no_route_policy
  deadline=$((SECONDS + AUTH_STABLE_TIMEOUT_SECONDS))
  : >"$EVIDENCE_DIR/auth-policy-observations.jsonl"
  while (( SECONDS <= deadline )); do
    if maas_policy="$(kubectl get maasauthpolicy "$AUTH_POLICY" -n "$TENANT_NAMESPACE" -o json 2>/dev/null)" && \
      gateway_policy="$(kubectl get authpolicy "$GATEWAY_AUTH_POLICY" -n maas-system -o json 2>/dev/null)" && \
      policies="$(kubectl get authpolicies.kuadrant.io -A -o json 2>/dev/null)" && \
      wasm_plugin="$(kubectl get wasmplugin "kuadrant-$GATEWAY" -n maas-system -o json 2>/dev/null)" && \
      envoy_filter="$(kubectl get envoyfilter "kuadrant-auth-$GATEWAY" -n maas-system -o json 2>/dev/null)"; then
      maas_generation="$(jq -r '.metadata.generation // 0' <<<"$maas_policy")"
      gateway_generation="$(jq -r '.metadata.generation // 0' <<<"$gateway_policy")"
      maas_ready="$(jq -r --argjson generation "$maas_generation" '[.status.conditions[]? | select(.type=="Ready" and .status=="True" and .observedGeneration==$generation)] | length > 0' <<<"$maas_policy")"
      no_conflict="$(jq -r --argjson generation "$maas_generation" '[.status.conditions[]? | select(.type=="ConflictingAuthPolicy" and .status=="False" and .observedGeneration==$generation)] | length > 0' <<<"$maas_policy")"
      gateway_current="$(jq -r --argjson generation "$gateway_generation" '.status.observedGeneration==$generation and ([.status.conditions[]? | select((.type=="Accepted" or .type=="Enforced") and .status=="True")] | length==2)' <<<"$gateway_policy")"
      no_route_policy="$(jq -r --arg taskNs "$TASK_NAMESPACE" --arg route "$POOL-kserve-route" --arg apiRoute "$API_ROUTE" '[.items[]? | select(.spec.targetRef.kind=="HTTPRoute" and ((.metadata.namespace==$taskNs and .spec.targetRef.name==$route) or (.metadata.namespace=="maas-system" and .spec.targetRef.name==$apiRoute)))] | length==0' <<<"$policies")"
      snapshot="$(jq -cn --arg taskNs "$TASK_NAMESPACE" --arg route "$POOL-kserve-route" --arg apiRoute "$API_ROUTE" \
        --slurpfile maas <(printf '%s' "$maas_policy") \
        --slurpfile gateway <(printf '%s' "$gateway_policy") \
        --slurpfile policies <(printf '%s' "$policies") \
        --slurpfile wasm <(printf '%s' "$wasm_plugin") \
        --slurpfile filter <(printf '%s' "$envoy_filter") '
        ($maas[0]) as $m | ($gateway[0]) as $g | ($policies[0]) as $p | ($wasm[0]) as $w | ($filter[0]) as $f |
        {maasPolicy:{name:$m.metadata.name,generation:$m.metadata.generation,phase:$m.status.phase,conditions:[$m.status.conditions[]? | select(.type=="Ready" or .type=="ConflictingAuthPolicy") | {type,status,observedGeneration,reason}]},
         gatewayPolicy:{name:$g.metadata.name,generation:$g.metadata.generation,observedGeneration:$g.status.observedGeneration,conditions:[$g.status.conditions[]? | select(.type=="Accepted" or .type=="Enforced") | {type,status,reason}]},
         routePolicies:[$p.items[]? | select(.spec.targetRef.kind=="HTTPRoute" and ((.metadata.namespace==$taskNs and .spec.targetRef.name==$route) or (.metadata.namespace=="maas-system" and .spec.targetRef.name==$apiRoute))) | {namespace:.metadata.namespace,name:.metadata.name,targetRef:.spec.targetRef}],
         wasmPlugin:{name:$w.metadata.name,generation:$w.metadata.generation,resourceVersion:$w.metadata.resourceVersion},
         envoyFilter:{name:$f.metadata.name,generation:$f.metadata.generation,resourceVersion:$f.metadata.resourceVersion}}')"
      fingerprint="$(printf '%s' "$snapshot" | sha256sum | awk '{print $1}')"
      if [[ "$maas_ready" == true && "$no_conflict" == true && "$gateway_current" == true && "$no_route_policy" == true ]]; then
        if [[ "$fingerprint" == "$previous" ]]; then stable=$((stable + 1)); else stable=1; fi
        previous="$fingerprint"
      else
        stable=0
        previous="$fingerprint"
      fi
      jq -cn --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg fingerprint "$fingerprint" --argjson stablePolls "$stable" \
        --argjson ready "$( [[ "$maas_ready" == true && "$no_conflict" == true && "$gateway_current" == true && "$no_route_policy" == true ]] && printf true || printf false )" \
        --argjson snapshot "$snapshot" '{observedAt:$observedAt,fingerprint:$fingerprint,stablePolls:$stablePolls,ready:$ready,snapshot:$snapshot}' \
        >>"$EVIDENCE_DIR/auth-policy-observations.jsonl"
      if (( stable >= 2 )); then
        jq . <<<"$snapshot" >"$EVIDENCE_DIR/stable-auth-policy-state.json"
        printf 'stable_polls=%s fingerprint=%s\n' "$stable" "$fingerprint" >"$EVIDENCE_DIR/auth-policy-readiness.txt"
        return 0
      fi
    else
      stable=0
      previous=''
      printf '%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ) policy read incomplete; retrying within bounded readiness window" \
        >>"$EVIDENCE_DIR/auth-policy-readiness.txt"
    fi
    sleep 1
  done
  printf 'MaaS/Gateway authorization state did not remain current and stable for two consecutive polls within %ss\n' \
    "$AUTH_STABLE_TIMEOUT_SECONDS" >&2
  return 1
}
stop_forward() {
  local pid="$1"
  [[ -n "$pid" ]] || return 0
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}
revoke_key() {
  local revoke_result revoke_status
  [[ -n "$API_KEY_ID" && -n "$API_LOCAL_PORT" && -n "$API_URL" && -s "$API_CA_FILE" ]] || return 0
  if revoke_result="$(curl --silent --show-error --connect-timeout 3 --max-time 15 \
      --cacert "$API_CA_FILE" --resolve "$API_TLS_HOST:$API_LOCAL_PORT:127.0.0.1" \
      --header "X-MaaS-Username: $PROBE_USERNAME" --header "X-MaaS-Group: $PROBE_GROUPS" \
      --request DELETE --write-out $'\n%{http_code}' \
      "$API_URL/$API_KEY_ID" 2>"$EVIDENCE_DIR/key-revoke.stderr"
    )"; then
    revoke_status="${revoke_result##*$'\n'}"
    if [[ "$revoke_status" == 200 || "$revoke_status" == 204 ]]; then
      KEY_REVOKED=true
      API_KEY_ID=""
      API_KEY=""
      printf '%s\n' "$revoke_status" >"$EVIDENCE_DIR/key-revocation-http-status.txt"
      return 0
    fi
    printf '%s\n' "$revoke_status" >"$EVIDENCE_DIR/key-revocation-http-status.txt"
    return 1
  fi
  return 1
}
on_exit() {
  local code="$1"
  trap - EXIT INT TERM ERR
  if (( code != 0 )) && [[ -d "$EVIDENCE_DIR" ]]; then capture_probe_failure || true; fi
  if [[ -n "$API_KEY_ID" ]]; then revoke_key || code=1; fi
  stop_forward "$API_FORWARD_PID"
  stop_forward "$GATEWAY_FORWARD_PID"
  API_KEY=""
  if [[ ! -f "$EVIDENCE_DIR/RESULT.json" ]]; then
    jq -n --arg pool "$POOL" --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
      --arg keyRevoked "$KEY_REVOKED" \
      '{mode:"run",status:"blocked",pool:$pool,exitCode:$exitCode,evidenceDirectory:$evidence,ephemeralCredentialRevoked:($keyRevoked=="true"),credentialBytesRecorded:false,qualificationTraffic:false}' \
      >"$EVIDENCE_DIR/RESULT.json"
  fi
  find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
  exit "$code"
}
trap 'on_exit $?' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

server="$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
ready="$(kubectl get --raw=/readyz)"
[[ "$server" == "$EXPECTED_API_SERVER" && "$ready" == *ok* ]] || { printf 'target API guard failed\n' >&2; exit 1; }
kubectl get namespace "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/task-namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/task-namespace.json" >/dev/null || {
  printf 'task namespace is not owned by this run\n' >&2; exit 1;
}

kubectl get aitenant "$POOL" -n ai-tenants -o json >"$EVIDENCE_DIR/aitenant.json"
TENANT_NAMESPACE="$(jq -er '.status.tenantNamespace' "$EVIDENCE_DIR/aitenant.json")"
jq -e --arg run "$RUN_LABEL" --arg gateway "$GATEWAY" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and .status.gatewayRef.name==$gateway and
  (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g))
' "$EVIDENCE_DIR/aitenant.json" >/dev/null || { printf 'MaaS AITenant is not current and Ready\n' >&2; exit 1; }
kubectl get maasmodelref "$MODELREF" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/maasmodelref.json"
jq -e --arg run "$RUN_LABEL" --arg pool "$POOL" --arg gateway "$GATEWAY" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and .status.phase=="Ready" and
  .status.resolvedTenantRef==$pool and .status.httpRouteGatewayName==$gateway and
  (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g))
' "$EVIDENCE_DIR/maasmodelref.json" >/dev/null || { printf 'MaaSModelRef is not current and Ready for the selected pool\n' >&2; exit 1; }
MODEL_ALIAS="$(jq -er '.status.resolvedModelAlias' "$EVIDENCE_DIR/maasmodelref.json")"
[[ "$MODEL_ALIAS" == "publishers/$TASK_NAMESPACE/models/"* ]] || {
  printf 'MaaSModelRef resolved an unexpected publisher model alias\n' >&2; exit 1;
}
kubectl get maasauthpolicy "$AUTH_POLICY" -n "$TENANT_NAMESPACE" -o json >"$EVIDENCE_DIR/maas-auth-policy.json"
kubectl get maassubscription "$SUBSCRIPTION" -n "$TENANT_NAMESPACE" -o json >"$EVIDENCE_DIR/maas-subscription.json"
for resource in maas-auth-policy maas-subscription; do
  file="$EVIDENCE_DIR/$resource.json"
  jq -e --arg run "$RUN_LABEL" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and .status.phase=="Active" and
    (.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g))
  ' "$file" >/dev/null || { printf '%s is not current and Active\n' "$resource" >&2; exit 1; }
done
kubectl get gateway "$GATEWAY" -n maas-system -o json >"$EVIDENCE_DIR/pool-gateway.json"
kubectl get authpolicy "$GATEWAY-maas-auth" -n maas-system -o json >"$EVIDENCE_DIR/gateway-auth-policy.json"
gateway_uid="$(jq -er '.metadata.uid' "$EVIDENCE_DIR/pool-gateway.json")"
jq -e --arg run "$RUN_LABEL" --arg gateway "$GATEWAY" --arg uid "$gateway_uid" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  (.metadata.generation as $g | any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g)) and
  any(.status.listeners[]?;.name=="inference" and any(.conditions[]?;.type=="Accepted" and .status=="True") and any(.conditions[]?;.type=="Programmed" and .status=="True"))
' "$EVIDENCE_DIR/pool-gateway.json" >/dev/null || { printf 'pool Gateway is not current and Accepted/Programmed\n' >&2; exit 1; }
jq -e --arg gateway "$GATEWAY" --arg uid "$gateway_uid" '
  .metadata.labels["app.kubernetes.io/managed-by"]=="maas-controller" and
  any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name==$gateway and .uid==$uid) and
  any(.status.conditions[]?;.type=="Accepted" and .status=="True") and any(.status.conditions[]?;.type=="Enforced" and .status=="True")
' "$EVIDENCE_DIR/gateway-auth-policy.json" >/dev/null || { printf 'controller-owned Gateway AuthPolicy is not Accepted and Enforced\n' >&2; exit 1; }
kubectl get service "$GATEWAY" -n maas-system -o json >"$EVIDENCE_DIR/pool-gateway-alias-service.json"
jq -e --arg run "$RUN_LABEL" --arg gateway "$GATEWAY" --arg gatewayUid "$gateway_uid" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.labels["gateway.networking.k8s.io/gateway-name"]==$gateway and
  .spec.type=="ClusterIP" and any(.spec.ports[]?;.port==443) and any(.metadata.ownerReferences[]?;.kind=="Gateway" and .uid==$gatewayUid)
' "$EVIDENCE_DIR/pool-gateway-alias-service.json" >/dev/null || { printf 'MaaS Gateway alias Service does not match its parent\n' >&2; exit 1; }
kubectl get endpointslices -n maas-system -l "kubernetes.io/service-name=$GATEWAY" -o json >"$EVIDENCE_DIR/pool-gateway-alias-endpointslices.json"
jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$EVIDENCE_DIR/pool-gateway-alias-endpointslices.json" >/dev/null || {
  printf 'MaaS Gateway alias Service has no Ready EndpointSlice target\n' >&2; exit 1;
}
kubectl get httproute "$API_ROUTE" -n maas-system -o json >"$EVIDENCE_DIR/maas-api-httproute.json"
jq -e --arg gateway "$GATEWAY" --arg service "$API_SERVICE" '
  any(.spec.parentRefs[]?;.name==$gateway and (.namespace // "maas-system")=="maas-system") and
  any(.spec.rules[]?; any(.matches[]?;.path.type=="PathPrefix" and .path.value=="/v1/api-keys") and
    any(.backendRefs[]?;.kind=="Service" and .name==$service and .port==8443)) and
  (.metadata.generation as $g | any(.status.parents[]?; .parentRef.name==$gateway and
    any(.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and
    any(.conditions[]?;.type=="ResolvedRefs" and .status=="True" and .observedGeneration==$g)))
' "$EVIDENCE_DIR/maas-api-httproute.json" >/dev/null || {
  printf 'MaaS API key route is not current, Accepted, Resolved, and attached to the pool Gateway\n' >&2; exit 1;
}
kubectl get httproute "$POOL-kserve-route" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/kserve-httproute.json"
route_path="/v1/chat/completions"
jq -e --arg gateway "$GATEWAY" --arg path "$route_path" --arg model "$MODEL_ALIAS" --arg pool "$POOL-inference-pool" '
  any(.spec.parentRefs[]?;.name==$gateway and .namespace=="maas-system") and
  any(.spec.rules[]?;
    any(.matches[]?;.path.type=="Exact" and .path.value==$path and
      any(.headers[]?;(.name|ascii_downcase)=="x-gateway-model-name" and .type=="Exact" and .value==$model)) and
    any(.backendRefs[]?;.kind=="InferencePool" and .name==$pool and .port==8000)) and
  any(.status.parents[]?.conditions[]?;.type=="Accepted" and .status=="True") and any(.status.parents[]?.conditions[]?;.type=="ResolvedRefs" and .status=="True")
' "$EVIDENCE_DIR/kserve-httproute.json" >/dev/null || { printf 'KServe route does not accept the canonical publisher model header on /v1/chat/completions\n' >&2; exit 1; }
kubectl get llminferenceservice "$POOL" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/llminferenceservice.json"
jq -e '(.metadata.generation as $g | .status.observedGeneration==$g and any(.status.conditions[]?;.type=="Ready" and .status=="True"))' \
  "$EVIDENCE_DIR/llminferenceservice.json" >/dev/null || {
  printf 'LLMInferenceService is not Ready at its current generation\n' >&2; exit 1;
}
kubectl get inferencepool "$POOL-inference-pool" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/inferencepool.json"

capture_deployment() {
  local namespace="$1" deployment="$2" digest="$3" name="$4" json selector pods_json image rs_json raw_pods_json rs_uids
  json="$EVIDENCE_DIR/$name-deployment.json"
  pods_json="$EVIDENCE_DIR/$name-pods.json"
  rs_json="$EVIDENCE_DIR/$name-replicasets.json"
  raw_pods_json="$EVIDENCE_DIR/$name-pods-unfiltered.json"
  kubectl get deployment "$deployment" -n "$namespace" -o json >"$json"
  jq -e --argjson replicas 1 '(.metadata.generation as $g | .status.observedGeneration==$g) and .spec.replicas==$replicas and .status.availableReplicas==$replicas' "$json" >/dev/null || {
    printf 'deployment %s is not current with one Available replica\n' "$deployment" >&2; return 1;
  }
  selector="$(jq -r '.spec.selector.matchLabels|to_entries|map("\(.key)=\(.value)")|join(",")' "$json")"
  [[ -n "$selector" ]] || { printf 'deployment %s has no matchLabels selector\n' "$deployment" >&2; return 1; }
  kubectl get replicasets -n "$namespace" -o json >"$rs_json"
  rs_uids="$(jq -c --arg deployment "$deployment" '[.items[]|select(any(.metadata.ownerReferences[]?;.kind=="Deployment" and .name==$deployment and .controller==true))|.metadata.uid]' "$rs_json")"
  [[ "$rs_uids" != "[]" ]] || { printf 'deployment %s has no owning ReplicaSet\n' "$deployment" >&2; return 1; }
  kubectl get pods -n "$namespace" -l "$selector" -o json >"$raw_pods_json"
  jq --argjson replicaSets "$rs_uids" '{apiVersion,kind,metadata,items:[.items[]|select(any(.metadata.ownerReferences[]?;.kind=="ReplicaSet" and (.uid as $uid|($replicaSets|index($uid))!=null)))]}' \
    "$raw_pods_json" >"$pods_json"
  jq -e --arg digest "$digest" '
    (.items|length)==1 and
    all(.items[]; .metadata.deletionTimestamp==null and any(.status.conditions[]?;.type=="Ready" and .status=="True") and
      any(.status.containerStatuses[]?;.imageID|contains("@"+$digest)))
  ' "$pods_json" >/dev/null || { printf 'deployment %s Pod is not uniquely Ready at the expected runtime digest\n' "$deployment" >&2; return 1; }
  image="$(jq -r '[.items[].status.containerStatuses[].imageID]|join(",")' "$pods_json")"
  printf '%s\n' "$image" >"$EVIDENCE_DIR/$name-runtime-imageIDs.txt"
}

capture_deployment "$TASK_NAMESPACE" "$SIMULATOR_DEPLOYMENT" "$EXPECTED_SIMULATOR_DIGEST" "$POOL-simulator"
capture_deployment "$TASK_NAMESPACE" "$EPP_DEPLOYMENT" "$EXPECTED_EPP_DIGEST" "$POOL-epp"
peer="pool-b"
[[ "$POOL" == pool-b ]] && peer="pool-a"
peer_resource_file="$EVIDENCE_DIR/peer-llminferenceservice.json"
peer_resource_json=null
if kubectl get llminferenceservice "$peer" -n "$TASK_NAMESPACE" -o json >"$peer_resource_file" 2>"$peer_resource_file.stderr"; then
  jq -e --arg run "$RUN_LABEL" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    (.metadata.generation as $g | .status.observedGeneration==$g and any(.status.conditions[]?;.type=="Ready" and .status=="True"))
  ' "$peer_resource_file" >/dev/null || { printf 'peer LLMInferenceService is not run-owned and Ready at its current generation\n' >&2; exit 1; }
  capture_deployment "$TASK_NAMESPACE" "$peer-kserve" "$EXPECTED_SIMULATOR_DIGEST" "$peer-simulator"
  peer_resource_json="$(jq -c . "$peer_resource_file")"
  peer_pod_uids="$(jq -c '[.items[].metadata.uid]' "$EVIDENCE_DIR/$peer-simulator-pods.json")"
  peer_deployment_present=true
else
  rg -qi 'notfound|not found' "$peer_resource_file.stderr" || {
    cat "$peer_resource_file.stderr" >&2
    printf 'peer LLMInferenceService lookup failed for a reason other than NotFound\n' >&2
    exit 1
  }
  peer_pod_uids='[]'
  if kubectl get deployment "$peer-kserve" -n "$TASK_NAMESPACE" -o json >"$EVIDENCE_DIR/peer-deployment-orphan.json" 2>"$EVIDENCE_DIR/peer-deployment-orphan.stderr"; then
    peer_deployment_present=true
  else
    rg -qi 'notfound|not found' "$EVIDENCE_DIR/peer-deployment-orphan.stderr" || {
      cat "$EVIDENCE_DIR/peer-deployment-orphan.stderr" >&2
      printf 'peer Deployment lookup failed for a reason other than NotFound\n' >&2
      exit 1
    }
    peer_deployment_present=false
  fi
fi
target_pod_uids="$(jq -c '[.items[].metadata.uid]' "$EVIDENCE_DIR/$POOL-simulator-pods.json")"
jq -n --arg pool "$POOL" --arg peer "$peer" --arg run "$RUN_LABEL" \
  --argjson peerResource "$peer_resource_json" --argjson peerDeploymentPresent "$peer_deployment_present" \
  --argjson targetPodUIDs "$target_pod_uids" --argjson peerPodUIDs "$peer_pod_uids" \
  '{pool:$pool,peerPool:$peer,runLabel:$run,peerResource:$peerResource,peerDeploymentPresent:$peerDeploymentPresent,targetPodUIDs:$targetPodUIDs,peerPodUIDs:$peerPodUIDs}' \
  | python3 "$SCRIPT_DIR/verify_kind_probe_peer.py" >"$EVIDENCE_DIR/peer-pool-state.json"
target_uid="$(jq -r '.[0]' <<<"$target_pod_uids")"
peer_uid="$(jq -r '.peerPodUIDs[0] // ""' "$EVIDENCE_DIR/peer-pool-state.json")"
simulator_pod="$(jq -r '.items[0].metadata.name' "$EVIDENCE_DIR/$POOL-simulator-pods.json")"
simulator_container="$(jq -r --arg digest "$EXPECTED_SIMULATOR_DIGEST" '.items[0].spec.containers[]|select(.image|contains("@"+$digest))|.name' "$EVIDENCE_DIR/$POOL-simulator-pods.json")"
kubectl get service "$API_SERVICE" -n maas-system -o json >"$EVIDENCE_DIR/maas-api-service.json"
capture_deployment maas-system "$API_DEPLOYMENT" "$EXPECTED_API_DIGEST" maas-api
wait_for_stable_auth_policy

wait_port_forward() {
  local pid="$1" port="$2" name="$3" logfile="$4" deadline
  deadline=$((SECONDS + 30))
  while (( SECONDS <= deadline )); do
    kill -0 "$pid" 2>/dev/null || { printf '%s port-forward exited before becoming ready\n' "$name" >&2; return 1; }
    if rg -Fq "Forwarding from 127.0.0.1:$port ->" "$logfile" && \
      timeout 1 bash -c "exec 3<>/dev/tcp/127.0.0.1/$port" 2>/dev/null; then
      return 0
    fi
    (( SECONDS < deadline )) || break
    sleep 1
  done
  printf '%s port-forward did not bind local port %s within 30s\n' "$name" "$port" >&2
  return 1
}

[[ -s "$API_CA_FILE" ]] || { printf 'run-owned MaaS API CA bundle is missing\n' >&2; exit 1; }
API_LOCAL_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
command kubectl --context="$KUBE_CONTEXT" port-forward --address 127.0.0.1 -n maas-system "svc/$API_SERVICE" "$API_LOCAL_PORT:8443" \
  >"$EVIDENCE_DIR/maas-api-port-forward.log" 2>&1 &
API_FORWARD_PID="$!"
wait_port_forward "$API_FORWARD_PID" "$API_LOCAL_PORT" pool-MaaS-API "$EVIDENCE_DIR/maas-api-port-forward.log"
API_URL="https://$API_TLS_HOST:$API_LOCAL_PORT/v1/api-keys"
API_HEALTH_STATUS="$(curl --silent --show-error --connect-timeout 3 --max-time 15 \
  --cacert "$API_CA_FILE" --resolve "$API_TLS_HOST:$API_LOCAL_PORT:127.0.0.1" \
  --output /dev/null --write-out '%{http_code}' "https://$API_TLS_HOST:$API_LOCAL_PORT/health" \
  2>"$EVIDENCE_DIR/maas-api-health.stderr")"
printf '%s\n' "$API_HEALTH_STATUS" >"$EVIDENCE_DIR/maas-api-direct-health-http-status.txt"
[[ "$API_HEALTH_STATUS" == 200 ]] || { printf 'pool MaaS API failed verified TLS health check (HTTP %s)\n' "$API_HEALTH_STATUS" >&2; exit 1; }
jq -n --arg service "$API_SERVICE" --arg serverName "$API_TLS_HOST" --arg status "$API_HEALTH_STATUS" \
  '{service:$service,serverName:$serverName,caBundle:"private-state/maas-api-root-ca.crt",healthHttpStatus:$status,tlsVerification:"CA and DNS/SNI verified",credentialProvisioningThroughPoolGateway:false,inferenceAuthorizationThroughPoolGateway:true}' \
  >"$EVIDENCE_DIR/maas-api-credential-provisioning-path.json"

key_name="grid-llmd-$POOL-setup-$(date -u +%Y%m%dT%H%M%SZ)"
key_body="$(jq -cn --arg name "$key_name" --arg sub "$SUBSCRIPTION" --arg run "$RUN_LABEL" \
  '{name:$name,subscription:$sub,ephemeral:true,expiresIn:"15m",labels:{"qualification-run":$run,purpose:"kind-pool-setup-probe"}}')"
key_result="$(curl --silent --show-error --connect-timeout 3 --max-time 30 \
  --cacert "$API_CA_FILE" --resolve "$API_TLS_HOST:$API_LOCAL_PORT:127.0.0.1" \
  --header 'Content-Type: application/json' --header "X-MaaS-Username: $PROBE_USERNAME" \
  --header "X-MaaS-Group: $PROBE_GROUPS" --data-binary "$key_body" \
  --write-out $'\n%{http_code}' "$API_URL" 2>"$EVIDENCE_DIR/maas-api-key-create.stderr")"
key_status="${key_result##*$'\n'}"
key_json="${key_result%$'\n'*}"
printf '%s\n' "$key_status" >"$EVIDENCE_DIR/maas-api-key-create-http-status.txt"
if [[ "$key_status" != 200 && "$key_status" != 201 ]]; then
  printf '%s' "$key_json" | python3 "$SCRIPT_DIR/sanitize_kind_maas_error.py" --http-status "$key_status" \
    >"$EVIDENCE_DIR/maas-api-key-create-error.json"
  printf 'MaaS API did not issue the requested ephemeral subscription key (HTTP %s)\n' "$key_status" >&2
  exit 1
fi
API_KEY_ID="$(jq -er '.id' <<<"$key_json")"
API_KEY="$(jq -er '.key' <<<"$key_json")"
issued_subscription="$(jq -er '.subscription' <<<"$key_json")"
ephemeral="$(jq -er '.ephemeral' <<<"$key_json")"
expires_at="$(jq -r '.expiresAt // ""' <<<"$key_json")"
unset key_result key_json
[[ "$issued_subscription" == "$SUBSCRIPTION" && "$ephemeral" == true ]] || {
  printf 'MaaS API key is not bound to the expected ephemeral subscription\n' >&2; exit 1;
}
jq -n --arg id "$API_KEY_ID" --arg subscription "$issued_subscription" --arg expiresAt "$expires_at" \
  --arg identity "$PROBE_USERNAME" --arg group "$PROBE_GROUPS" \
  '{keyId:$id,subscription:$subscription,ephemeral:true,expiresAt:$expiresAt,keyBytesRecorded:false,issuedThrough:"verified pool MaaS API service port-forward",testIdentity:$identity,testGroups:$group}' \
  >"$EVIDENCE_DIR/maas-ephemeral-key-summary.json"

# The setup identity only provisions this short-lived test key. The inference
# request below still enters through the pool Gateway and its MaaS AuthPolicy.
GATEWAY_LOCAL_PORT="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
command kubectl --context="$KUBE_CONTEXT" port-forward --address 127.0.0.1 -n maas-system "svc/$GATEWAY" "$GATEWAY_LOCAL_PORT:443" \
  >"$EVIDENCE_DIR/pool-gateway-port-forward.log" 2>&1 &
GATEWAY_FORWARD_PID="$!"
wait_port_forward "$GATEWAY_FORWARD_PID" "$GATEWAY_LOCAL_PORT" pool-Gateway "$EVIDENCE_DIR/pool-gateway-port-forward.log"

PROBE_ID="kind-setup-$POOL-$(date -u +%Y%m%dT%H%M%SZ)-$(cat /proc/sys/kernel/random/uuid)"
CLIENT_REQUEST_ID="client-$POOL-$(cat /proc/sys/kernel/random/uuid)"
jq -n --arg model "$MODEL_ALIAS" --arg prompt "$PROBE_ID" \
  '{model:$model,messages:[{role:"user",content:$prompt}],max_tokens:16,stream:false}' \
  >"$EVIDENCE_DIR/request.json"
jq -n --arg model "$MODEL_ALIAS" --arg path "$route_path" \
  '{requestModel:$model,requestPath:$path,modelSource:"MaaSModelRef.status.resolvedModelAlias",internalHeaderSource:"trusted-pre-extproc",clientSuppliedRoutingHeader:false}' \
  >"$EVIDENCE_DIR/request-routing-contract.json"
chmod 0600 "$EVIDENCE_DIR/request.json"
request_path="$route_path"
gateway_url="http://127.0.0.1:$GATEWAY_LOCAL_PORT$request_path"
response_status="$(curl --silent --show-error --connect-timeout 3 --max-time 60 --request POST \
  --header 'Content-Type: application/json' --data-binary "@$EVIDENCE_DIR/request.json" \
  --dump-header "$EVIDENCE_DIR/response-headers-raw.txt" --output "$EVIDENCE_DIR/response.json" \
  --write-out '%{http_code}' --config - "$gateway_url" 2>"$EVIDENCE_DIR/gateway-request.stderr" <<EOF
header = "Authorization: Bearer $API_KEY"
header = "X-Request-ID: $CLIENT_REQUEST_ID"
EOF
)"
printf '%s\n' "$response_status" >"$EVIDENCE_DIR/response-http-status.txt"
python3 - "$EVIDENCE_DIR/response-headers-raw.txt" "$EVIDENCE_DIR/response-headers.txt" <<'PY'
from pathlib import Path
import sys

raw, sanitized = map(Path, sys.argv[1:])
allow = {"x-request-id", "content-type", "x-ai-inference-provider", "x-ai-demo-provider-gateway", "x-ext-auth-reason"}
lines = []
for line in raw.read_text(encoding="utf-8", errors="replace").splitlines():
    if line.startswith("HTTP/"):
        lines.append(line)
        continue
    name, sep, value = line.partition(":")
    if sep and name.strip().lower() in allow:
        lines.append(f"{name.strip().lower()}: {value.strip()}")
sanitized.write_text("\n".join(lines) + "\n", encoding="utf-8")
raw.unlink(missing_ok=True)
PY
[[ "$response_status" == 200 ]] || { printf 'pool Gateway setup request returned HTTP %s\n' "$response_status" >&2; exit 1; }
response_request_id="$(awk 'tolower($1)=="x-request-id:" {gsub("\r", "", $2); print $2; exit}' "$EVIDENCE_DIR/response-headers.txt")"
[[ -n "$response_request_id" ]] || { printf 'pool Gateway response omitted its response-visible request ID\n' >&2; exit 1; }
jq -e --arg model "$EXPECTED_RESPONSE_MODEL" 'type=="object" and .model==$model and (.choices|type=="array" and length==1)' \
  "$EVIDENCE_DIR/response.json" >/dev/null || { printf 'pool Gateway response identity did not match %s\n' "$EXPECTED_RESPONSE_MODEL" >&2; exit 1; }

kubectl logs "$simulator_pod" -n "$TASK_NAMESPACE" -c "$simulator_container" --since=5m \
  >"$EVIDENCE_DIR/simulator-recent.log" 2>"$EVIDENCE_DIR/simulator-log.stderr"
if ! rg -F "$response_request_id" "$EVIDENCE_DIR/simulator-recent.log" | rg -F 'Finished processing request' >"$EVIDENCE_DIR/simulator-completion-receipt.txt"; then
  printf 'independent simulator log did not contain a completion record for response request ID %s\n' "$response_request_id" >&2
  exit 1
fi
receipt_lines="$(wc -l <"$EVIDENCE_DIR/simulator-completion-receipt.txt" | tr -d '[:space:]')"
[[ "$receipt_lines" == 1 ]] || { printf 'expected exactly one simulator completion receipt, found %s\n' "$receipt_lines" >&2; exit 1; }

revoke_key || { printf 'ephemeral MaaS key could not be revoked after the setup probe\n' >&2; exit 1; }
jq -n --arg pool "$POOL" --arg run "$RUN_LABEL" --arg probeId "$PROBE_ID" \
  --arg clientRequestId "$CLIENT_REQUEST_ID" --arg responseRequestId "$response_request_id" \
  --arg responseModel "$EXPECTED_RESPONSE_MODEL" --arg modelAlias "$MODEL_ALIAS" --arg requestPath "$request_path" \
  --arg simPod "$simulator_pod" --arg simUID "$target_uid" \
  --arg peerPodUID "$peer_uid" --arg simImageID "$(cat "$EVIDENCE_DIR/$POOL-simulator-runtime-imageIDs.txt")" \
  --arg peerState "$(jq -r '.peerState' "$EVIDENCE_DIR/peer-pool-state.json")" \
  --argjson pairwiseDisjointVerified "$(jq -r '.pairwiseDisjointVerified' "$EVIDENCE_DIR/peer-pool-state.json")" \
  --arg apiTlsHost "$API_TLS_HOST" --arg apiHealthStatus "$API_HEALTH_STATUS" \
  --arg receiptLines "$receipt_lines" --arg evidence "$EVIDENCE_DIR" \
  '{mode:"run",status:"passed",setupProbe:true,pool:$pool,run:$run,probeId:$probeId,clientRequestId:$clientRequestId,responseRequestId:$responseRequestId,httpStatus:200,responseModel:$responseModel,modelAlias:$modelAlias,requestPath:$requestPath,independentSimulatorReceipt:{matchedRequestId:$responseRequestId,pod:$simPod,podUID:$simUID,receiptLines:($receiptLines|tonumber)},peerSimulatorPodUID:$peerPodUID,peerPoolVerification:{state:$peerState,pairwiseDisjointVerified:$pairwiseDisjointVerified},simulatorImageID:$simImageID,maasEphemeralKeyRevoked:true,credentialBytesRecorded:false,credentialProvisioning:{path:"pool-specific MaaS API service over verified TLS port-forward",serviceServerName:$apiTlsHost,healthHttpStatus:($apiHealthStatus|tonumber)},inferenceAuthorization:"pool Gateway MaaS AuthPolicy",requestPathScope:"pool Gateway to KServe InferencePool/EPP/simulator",gridTaskGatewayProviderProxyAndExtProcTraversed:false,qualificationTraffic:false,evidenceDirectory:$evidence}' \
  >"$EVIDENCE_DIR/RESULT.json"
log "$POOL authorized Gateway setup probe passed with an independent simulator completion receipt; qualification traffic remains unstarted"
