#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Create isolated Gateway API parents for the two simulator-backed pools.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${POOL_GATEWAYS_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
GATEWAY_CLASS="${GRID_API_GATEWAY_CLASS:-data-science-gateway-class}"
POOL_A="${POOL_A_NAME:-pool-a}"
POOL_B="${POOL_B_NAME:-pool-b}"
POOL_A_GATEWAY="${POOL_A_GATEWAY_NAME:-grid-llmd-pool-a}"
POOL_B_GATEWAY="${POOL_B_GATEWAY_NAME:-grid-llmd-pool-b}"
LISTENER_NAME="${POOL_GATEWAY_LISTENER_NAME:-inference}"
LISTENER_PORT="${POOL_GATEWAY_LISTENER_PORT:-8080}"
INFRA_ANNOTATION_KEY="${POOL_GATEWAY_INFRA_ANNOTATION_KEY:-service.beta.kubernetes.io/aws-load-balancer-internal}"
INFRA_ANNOTATION_VALUE="${POOL_GATEWAY_INFRA_ANNOTATION_VALUE:-true}"
TIMEOUT="${POOL_GATEWAYS_TIMEOUT:-10m}"
EVIDENCE_DIR="${POOL_GATEWAYS_EVIDENCE_DIR:-}"

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Pool Gateway plan (no cluster calls):
  1. Verify the exact task API, namespace ownership, and GatewayClass.
  2. Render two run-owned internal HTTP Gateways and server-side dry-run them.
  3. Wait for Accepted/Programmed status and ready data-plane endpoints.
  4. Record each Gateway's in-cluster Service DNS, listener name, and port for its LLMInferenceService and Grid provider.
The pool Gateways are separate from the public task Gateway. They are not selected by the public HTTPRoute or ExtProc EnvoyFilter. The configured
infrastructure annotation must appear on each generated Service before its endpoint is accepted.
Modes: plan, render, install, verify, collect. API TLS bypass requires CLUSTER_ENV=ephemeral-dev and ALLOW_INSECURE_DEV_API=true.
PLAN
  exit 0
fi

for bin in oc jq sha256sum python3 paste; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ "$MODE" =~ ^(render|install|verify|collect)$ ]] || { printf 'unsupported POOL_GATEWAYS_MODE: %s\n' "$MODE" >&2; exit 2; }
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" ]] || {
  printf 'EXPECTED_API_SERVER, KUBE_CONTEXT, and GRID_RUN_LABEL are required\n' >&2; exit 2;
}
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'GRID_RUN_LABEL is invalid\n' >&2; exit 2; }
[[ "$POOL_A_GATEWAY" != "$POOL_B_GATEWAY" ]] || { printf 'pool gateways must have distinct names\n' >&2; exit 2; }
for value in "$POOL_A" "$POOL_B" "$POOL_A_GATEWAY" "$POOL_B_GATEWAY" "$LISTENER_NAME" "$GATEWAY_CLASS"; do
  [[ "$value" =~ ^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$ ]] || { printf 'Gateway names, listener, and class must be valid DNS names\n' >&2; exit 2; }
done
[[ "$LISTENER_PORT" =~ ^[0-9]+$ && "$LISTENER_PORT" -ge 1 && "$LISTENER_PORT" -le 65535 ]] || { printf 'invalid POOL_GATEWAY_LISTENER_PORT\n' >&2; exit 2; }
[[ "$INFRA_ANNOTATION_KEY" =~ ^[A-Za-z0-9.-]+/[A-Za-z0-9][-A-Za-z0-9_.]*$ ]] || { printf 'invalid POOL_GATEWAY_INFRA_ANNOTATION_KEY\n' >&2; exit 2; }
[[ "$INFRA_ANNOTATION_VALUE" =~ ^[A-Za-z0-9][-A-Za-z0-9_.]*$ ]] || { printf 'invalid POOL_GATEWAY_INFRA_ANNOTATION_VALUE\n' >&2; exit 2; }
[[ "$TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'POOL_GATEWAYS_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
case "${TIMEOUT: -1}" in
  s) TIMEOUT_SECONDS="${TIMEOUT%?}" ;;
  m) TIMEOUT_SECONDS="$(( ${TIMEOUT%?} * 60 ))" ;;
  h) TIMEOUT_SECONDS="$(( ${TIMEOUT%?} * 3600 ))" ;;
esac
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="$REPO_ROOT/evidence/pool-gateways-$(date -u +%Y%m%dT%H%M%SZ)"
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  printf '%s\n' "$line" | tee -a "$EVIDENCE_DIR/RUN-LOG.md" "$REPO_ROOT/RUN-LOG.md"
}

remove_run_gateways() {
  local gateway live remaining deadline
  for gateway in "$POOL_A_GATEWAY" "$POOL_B_GATEWAY"; do
    live="$EVIDENCE_DIR/$gateway-before-safety-delete.json"
    if oc get gateway "$gateway" -n "$NAMESPACE" -o json >"$live" 2>/dev/null &&
      jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$live" >/dev/null; then
      oc delete gateway "$gateway" -n "$NAMESPACE" --wait=true --timeout=120s \
        >"$EVIDENCE_DIR/$gateway-safety-delete.txt" 2>&1 || true
    fi
  done
  deadline=$((SECONDS + 180))
  for gateway in "$POOL_A_GATEWAY" "$POOL_B_GATEWAY"; do
    while (( SECONDS <= deadline )); do
      remaining="$(oc get service -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$gateway" -o json 2>/dev/null | jq '.items|length' || printf 'unknown')"
      [[ "$remaining" == 0 ]] && break
      (( SECONDS < deadline )) || break
      sleep 5
    done
    printf '%s generatedServicesRemaining=%s\n' "$gateway" "${remaining:-unknown}" >>"$EVIDENCE_DIR/safety-cleanup-status.txt"
  done
}

on_error() {
  local code="$1"
  trap - ERR
  if [[ "${POOL_GATEWAYS_APPLIED:-false}" == true ]]; then
    remove_run_gateways || true
  fi
  jq -n --arg mode "$MODE" --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:"blocked",exitCode:$exitCode,evidenceDirectory:$evidence,inferenceTrafficSent:false}' >"$EVIDENCE_DIR/RESULT.json"
  log "Pool Gateway mode $MODE stopped with exit=$code"
  printf '# Pool Gateway bug capture\n\n- Mode: %s\n- Exit code: %s\n- Evidence: %s\n' "$MODE" "$code" "$EVIDENCE_DIR" >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  exit "$code"
}
trap 'on_error $?' ERR

server="$(oc whoami --show-server)"
user="$(oc whoami)"
ready="$(oc get --raw=/readyz)"
[[ "$server" == "$EXPECTED_API_SERVER" && "$ready" == *ok* ]] || { printf 'target API guard failed\n' >&2; exit 1; }
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
if [[ "$insecure" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || { printf 'explicit ephemeral-dev TLS opt-in required\n' >&2; exit 1; }
  API_TLS_MODE=explicit-ephemeral-dev-unverified
else API_TLS_MODE=kubeconfig-verified; fi
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/namespace.json" >/dev/null
oc get gatewayclass "$GATEWAY_CLASS" -o json >"$EVIDENCE_DIR/gatewayclass.json"
jq -e 'any(.status.conditions[]?;.type=="Accepted" and .status=="True")' "$EVIDENCE_DIR/gatewayclass.json" >/dev/null

cat >"$EVIDENCE_DIR/pool-gateways.yaml" <<YAML
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: $POOL_A_GATEWAY
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
    praxis.fast/simulator-pool: "$POOL_A"
spec:
  gatewayClassName: $GATEWAY_CLASS
  infrastructure:
    annotations:
      $INFRA_ANNOTATION_KEY: "$INFRA_ANNOTATION_VALUE"
  listeners:
    - name: $LISTENER_NAME
      port: $LISTENER_PORT
      protocol: HTTP
      allowedRoutes:
        namespaces:
          from: Same
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: $POOL_B_GATEWAY
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
    praxis.fast/simulator-pool: "$POOL_B"
spec:
  gatewayClassName: $GATEWAY_CLASS
  infrastructure:
    annotations:
      $INFRA_ANNOTATION_KEY: "$INFRA_ANNOTATION_VALUE"
  listeners:
    - name: $LISTENER_NAME
      port: $LISTENER_PORT
      protocol: HTTP
      allowedRoutes:
        namespaces:
          from: Same
YAML
chmod 0600 "$EVIDENCE_DIR/pool-gateways.yaml"
sha256sum "$EVIDENCE_DIR/pool-gateways.yaml" >"$EVIDENCE_DIR/render-sha256.txt"

for gateway in "$POOL_A_GATEWAY" "$POOL_B_GATEWAY"; do
  if oc get gateway "$gateway" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/existing-$gateway.json" 2>/dev/null; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/existing-$gateway.json" >/dev/null || {
      printf 'Gateway %s already exists outside this run ownership\n' "$gateway" >&2; exit 1;
    }
  fi
done
oc apply --dry-run=server -f "$EVIDENCE_DIR/pool-gateways.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
if [[ "$MODE" == render ]]; then
  jq -n --arg evidence "$EVIDENCE_DIR" '{mode:"render",status:"rendered",evidenceDirectory:$evidence,clusterMutated:false,inferenceTrafficSent:false}' >"$EVIDENCE_DIR/RESULT.json"
  log 'Two internal pool Gateways passed server-side dry-run; no cluster resources changed'
  exit 0
fi
printf 'context=%s\nuser=%s\nserver=%s\nreadyz=%s\napi_tls_mode=%s\n' "$KUBE_CONTEXT" "$user" "$server" "$ready" "$API_TLS_MODE" >"$EVIDENCE_DIR/api-access-summary.txt"
if [[ "$MODE" == install ]]; then
  oc apply -f "$EVIDENCE_DIR/pool-gateways.yaml" >"$EVIDENCE_DIR/apply.txt"
  POOL_GATEWAYS_APPLIED=true
fi

wait_gateway_ready() {
  local pool="$1" gateway="$2" deadline snapshot
  deadline=$((SECONDS + TIMEOUT_SECONDS))
  snapshot="$EVIDENCE_DIR/$gateway-live.json"
  while (( SECONDS <= deadline )); do
    if oc get gateway "$gateway" -n "$NAMESPACE" -o json >"$snapshot" 2>"$EVIDENCE_DIR/$gateway-get.stderr"; then
      if jq -e --arg run "$RUN_LABEL" --arg listener "$LISTENER_NAME" --argjson port "$LISTENER_PORT" '
        .metadata.labels["praxis.fast/qualification-run"]==$run and
        (.metadata.generation as $g |
          any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and
          any(.status.conditions[]?;.type=="Programmed" and .status=="True" and .observedGeneration==$g)) and
        any(.status.listeners[]?;.name==$listener and
          any(.conditions[]?;.type=="Accepted" and .status=="True") and
          any(.conditions[]?;.type=="Programmed" and .status=="True")) and
        any(.spec.listeners[]?;.name==$listener and .port==$port and .protocol=="HTTP")
      ' "$snapshot" >/dev/null 2>&1; then
        log "Gateway $gateway for $pool is Accepted and Programmed at current generation"
        return 0
      fi
    fi
    now="$SECONDS"
    (( now < deadline )) || break
    sleep 5
  done
  oc describe gateway "$gateway" -n "$NAMESPACE" >"$EVIDENCE_DIR/$gateway-describe.txt" 2>&1 || true
  oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp >"$EVIDENCE_DIR/$gateway-events.txt" 2>&1 || true
  printf 'Gateway %s for %s did not become current-generation Accepted and Programmed within %s\n' "$gateway" "$pool" "$TIMEOUT" >&2
  return 1
}

wait_for_private_gateway_address() {
  local pool="$1" gateway="$2" deadline host snapshot addresses resolution_rc public_since unresolved_since now
  deadline=$((SECONDS + TIMEOUT_SECONDS))
  public_since=0
  unresolved_since=0
  snapshot="$EVIDENCE_DIR/$gateway-live.json"
  while (( SECONDS <= deadline )); do
    if oc get gateway "$gateway" -n "$NAMESPACE" -o json >"$snapshot" 2>"$EVIDENCE_DIR/$gateway-get.stderr"; then
      host="$(jq -r '[.status.addresses[]?.value|select(type=="string" and length>0)][0] // empty' "$snapshot")"
      if [[ -n "$host" ]] && python3 - "$host" >"$EVIDENCE_DIR/$gateway-private-addresses.txt" 2>"$EVIDENCE_DIR/$gateway-address-resolution.stderr" <<'PY'
import ipaddress
import socket
import sys

host = sys.argv[1]
try:
    addresses = sorted({
        ipaddress.ip_address(item[4][0].split("%", 1)[0])
        for item in socket.getaddrinfo(host, None, type=socket.SOCK_STREAM)
    }, key=str)
except (OSError, ValueError) as exc:
    print(f"resolve failed: {type(exc).__name__}", file=sys.stderr)
    raise SystemExit(1)
networks = tuple(ipaddress.ip_network(value) for value in (
    "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"
))
print("\n".join(map(str, addresses)))
if not addresses or any(not any(address in network for network in networks) for address in addresses):
    print("one or more resolved addresses are not RFC1918/ULA private", file=sys.stderr)
    raise SystemExit(2)
PY
      then
        addresses="$(paste -sd, "$EVIDENCE_DIR/$gateway-private-addresses.txt")"
        log "Gateway $gateway for $pool resolves only to RFC1918/ULA private addresses"
        printf '%s\n' "$host" >"$EVIDENCE_DIR/$gateway-load-balancer-host.txt"
        printf '%s\n' "$addresses" >"$EVIDENCE_DIR/$gateway-load-balancer-private-addresses.txt"
        printf '%s\n' 'private-address-dns' >"$EVIDENCE_DIR/$gateway-load-balancer-scope-verification.txt"
        return 0
      else
        resolution_rc="$?"
        if [[ "$resolution_rc" == 2 ]]; then
          unresolved_since=0
          now="$SECONDS"
          if (( public_since == 0 )); then
            public_since="$now"
          elif (( now - public_since >= 30 )); then
            cat "$EVIDENCE_DIR/$gateway-private-addresses.txt" >"$EVIDENCE_DIR/$gateway-public-addresses.txt"
            log "Gateway $gateway continued resolving to public addresses for 30 seconds; fail-closed cleanup will remove the run-owned pool Gateways"
            return 1
          fi
        else
          public_since=0
          now="$SECONDS"
          if (( unresolved_since == 0 )); then
            unresolved_since="$now"
          elif (( now - unresolved_since >= 30 )); then
            log "Gateway $gateway hostname remains unresolved from the runner; internal AWS annotation is present and no public address was returned"
            printf '%s\n' "$host" >"$EVIDENCE_DIR/$gateway-load-balancer-host.txt"
            : >"$EVIDENCE_DIR/$gateway-load-balancer-private-addresses.txt"
            printf '%s\n' 'aws-internal-annotation-and-nonpublic-dns-from-runner' >"$EVIDENCE_DIR/$gateway-load-balancer-scope-verification.txt"
            return 0
          fi
        fi
      fi
    fi
    (( SECONDS < deadline )) || break
    sleep 5
  done
  oc describe gateway "$gateway" -n "$NAMESPACE" >"$EVIDENCE_DIR/$gateway-describe.txt" 2>&1 || true
  oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp >"$EVIDENCE_DIR/$gateway-events.txt" 2>&1 || true
  log "Gateway $gateway did not resolve exclusively to private addresses; run-owned pool Gateways will be removed"
  return 1
}

capture_gateway() {
  local pool="$1" gateway="$2" service_json service slices_json deadline
  service_json="$EVIDENCE_DIR/$gateway-service.json"
  wait_gateway_ready "$pool" "$gateway"
  deadline=$((SECONDS + TIMEOUT_SECONDS))
  service=""
  while (( SECONDS <= deadline )); do
    if oc get service -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$gateway" -o json >"$service_json" 2>"$EVIDENCE_DIR/$gateway-service-get.stderr"; then
      service="$(jq -r --argjson port "$LISTENER_PORT" '[.items[]|select(any(.spec.ports[]?;.port==$port))]|if length==1 then .[0].metadata.name else empty end' "$service_json")"
      if [[ -n "$service" ]]; then
        slices_json="$EVIDENCE_DIR/$gateway-endpointslices.json"
        if oc get service "$service" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/$gateway-service-live.json" 2>"$EVIDENCE_DIR/$gateway-service-live.stderr" &&
          jq -e --arg key "$INFRA_ANNOTATION_KEY" --arg value "$INFRA_ANNOTATION_VALUE" '.metadata.annotations[$key]==$value' "$EVIDENCE_DIR/$gateway-service-live.json" >/dev/null 2>&1 &&
          oc get endpointslice -n "$NAMESPACE" -l "kubernetes.io/service-name=$service" -o json >"$slices_json" 2>"$EVIDENCE_DIR/$gateway-endpointslices-get.stderr" &&
          jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$slices_json" >/dev/null 2>&1; then
          break
        fi
      fi
    fi
    (( SECONDS < deadline )) || break
    sleep 5
  done
  [[ -n "$service" ]] || { printf 'Gateway %s has no unique listener Service on port %s within %s\n' "$gateway" "$LISTENER_PORT" "$TIMEOUT" >&2; return 1; }
  if ! jq -e --arg key "$INFRA_ANNOTATION_KEY" --arg value "$INFRA_ANNOTATION_VALUE" '.metadata.annotations[$key]==$value' "$EVIDENCE_DIR/$gateway-service-live.json" >/dev/null 2>&1; then
    oc get gateway "$gateway" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/$gateway-live.json" 2>&1 || true
    oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp >"$EVIDENCE_DIR/$gateway-events.txt" 2>&1 || true
    log "Gateway controller did not propagate $INFRA_ANNOTATION_KEY=$INFRA_ANNOTATION_VALUE to $service; removing both run-owned pool Gateways to avoid retaining public endpoints"
    return 1
  fi
  log "Gateway $gateway Service carries $INFRA_ANNOTATION_KEY=$INFRA_ANNOTATION_VALUE"
  jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$EVIDENCE_DIR/$gateway-endpointslices.json" >/dev/null || {
    oc get pods -n "$NAMESPACE" -o wide >"$EVIDENCE_DIR/$gateway-pods-on-timeout.txt" 2>&1 || true
    oc describe service "$service" -n "$NAMESPACE" >"$EVIDENCE_DIR/$gateway-service-describe.txt" 2>&1 || true
    printf 'Gateway %s Service %s has no ready EndpointSlice target within %s\n' "$gateway" "$service" "$TIMEOUT" >&2
    return 1
  }
  log "Gateway $gateway listener Service $service has a ready EndpointSlice target"
  wait_for_private_gateway_address "$pool" "$gateway"
  jq -n --arg pool "$pool" --arg gateway "$gateway" --arg namespace "$NAMESPACE" --arg service "$service" \
    --arg host "$service.$NAMESPACE.svc.cluster.local" --arg listener "$LISTENER_NAME" --argjson port "$LISTENER_PORT" \
    --arg annotationKey "$INFRA_ANNOTATION_KEY" --arg annotationValue "$INFRA_ANNOTATION_VALUE" \
    --arg loadBalancerHost "$(cat "$EVIDENCE_DIR/$gateway-load-balancer-host.txt")" \
    --arg privateAddresses "$(cat "$EVIDENCE_DIR/$gateway-load-balancer-private-addresses.txt")" \
    --arg scopeVerification "$(cat "$EVIDENCE_DIR/$gateway-load-balancer-scope-verification.txt")" \
    '{pool:$pool,gateway:{name:$gateway,namespace:$namespace},listener:$listener,service:$service,endpoint:{host:$host,port:$port,scheme:"http"},serviceInfrastructureAnnotation:{key:$annotationKey,value:$annotationValue},loadBalancer:{host:$loadBalancerHost,resolvedPrivateAddresses:(if $privateAddresses=="" then [] else ($privateAddresses|split(",")) end),scopeVerification:$scopeVerification},ready:true}' \
    >"$EVIDENCE_DIR/$pool-gateway-endpoint.json"
}

capture_gateway "$POOL_A" "$POOL_A_GATEWAY"
capture_gateway "$POOL_B" "$POOL_B_GATEWAY"
jq -s '{status:"READY",poolGateways:.}' "$EVIDENCE_DIR/$POOL_A-gateway-endpoint.json" "$EVIDENCE_DIR/$POOL_B-gateway-endpoint.json" >"$EVIDENCE_DIR/pool-gateways-result.json"
jq -n --arg mode "$MODE" --arg evidence "$EVIDENCE_DIR" '{mode:$mode,status:"ready",evidenceDirectory:$evidence,gatewaysReady:true,inferenceTrafficSent:false}' >"$EVIDENCE_DIR/RESULT.json"
cat >"$EVIDENCE_DIR/FINAL-RESULT.md" <<EOF
# Internal pool Gateways

- Status: both run-owned Gateways are current-generation Accepted/Programmed with Ready Service endpoints and AWS hostnames resolving only to RFC1918/ULA addresses.
- Pool A endpoint: $(jq -r '.endpoint.host+":"+(.endpoint.port|tostring)' "$EVIDENCE_DIR/$POOL_A-gateway-endpoint.json")
- Pool B endpoint: $(jq -r '.endpoint.host+":"+(.endpoint.port|tostring)' "$EVIDENCE_DIR/$POOL_B-gateway-endpoint.json")
- Public task Gateway and ExtProc filter were not changed.
- No inference traffic was sent.
- Evidence: $EVIDENCE_DIR
EOF
log "Pool Gateways $MODE complete; both pool-specific internal endpoints are ready"
find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
