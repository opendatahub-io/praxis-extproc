#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source="$SCRIPT_DIR/deploy-pool-gateways.sh"

python3 - "$source" <<'PY'
from pathlib import Path
import sys

text = Path(sys.argv[1]).read_text(encoding="utf-8")
checks = (
    '[[ "$TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]]',
    'POOL_GATEWAYS_TIMEOUT must be an integer followed by s, m, or h',
    'wait_gateway_ready() {',
    'Accepted" and .status=="True" and .observedGeneration==$g',
    'Programmed" and .status=="True" and .observedGeneration==$g',
    'sleep 5',
    'Gateway %s for %s did not become current-generation Accepted and Programmed',
    'service.beta.kubernetes.io/aws-load-balancer-internal',
    'INFRA_ANNOTATION_KEY="${POOL_GATEWAY_INFRA_ANNOTATION_KEY:-$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf networking.istio.io/service-type; else printf service.beta.kubernetes.io/aws-load-balancer-internal; fi)}"',
    'INFRA_ANNOTATION_VALUE="${POOL_GATEWAY_INFRA_ANNOTATION_VALUE:-$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf ClusterIP; else printf true; fi)}"',
    'GATEWAY_OBJECT_ANNOTATION_KEY="${POOL_GATEWAY_OBJECT_ANNOTATION_KEY:-}"',
    'CLUSTER_PLATFORM="${GRID_LLMD_CLUSTER_PLATFORM:-$CLUSTER_ENV}"',
    'if [[ "$CLUSTER_PLATFORM" == kind && -z "$GATEWAY_OBJECT_ANNOTATION_KEY" && -z "$GATEWAY_OBJECT_ANNOTATION_VALUE" ]]; then',
    'GATEWAY_OBJECT_ANNOTATION_KEY="networking.istio.io/service-type"',
    'GATEWAY_OBJECT_ANNOTATION_VALUE="ClusterIP"',
    'spec.infrastructure.annotations copies it onto the generated Service',
    'declare -A GATEWAY_PREEXISTING=()',
    'CREATED_GATEWAYS=()',
    'GATEWAY_PREEXISTING["$gateway"]=true',
    'GATEWAY_PREEXISTING["$gateway"]=false',
    'for gateway in "${CREATED_GATEWAYS[@]}"; do',
    'CREATED_GATEWAYS+=("$gateway")',
    'GATEWAY_NAMESPACE="${POOL_GATEWAY_NAMESPACE:-$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf maas-system; else printf \'%s\' "$NAMESPACE"; fi)}"',
    'GATEWAY_CLASS="${GRID_API_GATEWAY_CLASS:-$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf istio; else printf data-science-gateway-class; fi)}"',
    'POOL_GATEWAY_OBJECT_ANNOTATION_KEY and VALUE must both be valid when either is set',
    '$GATEWAY_OBJECT_ANNOTATION_BLOCK',
    'ADDRESS_MODE="${POOL_GATEWAY_ADDRESS_MODE:-$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf cluster-service-dns; else printf private-dns; fi)}"',
    'POOL_GATEWAY_ADDRESS_MODE must be private-dns or cluster-service-dns',
    'kind-cluster-service-dns-and-ready-endpoints',
    'if [[ "$host" != "$service.$GATEWAY_NAMESPACE.svc.cluster.local" || "$service_type" != ClusterIP ]]; then',
    '$GATEWAY_OBJECT_ANNOTATION_BLOCK',
    'matchLabels',
    'praxis.fast/pool-a-gateway-access: "true"',
    'praxis.fast/pool-b-gateway-access: "true"',
    'oc label namespace "$NAMESPACE"',
    'ensure_maas_gateway_alias() {',
    'port: 443',
    'targetPort: $LISTENER_PORT',
    'kind: Gateway',
    'no Ready Gateway endpoint',
    'ensure_maas_gateway_alias "$POOL_A_GATEWAY" "$POOL_A"',
    'ensure_maas_gateway_alias "$POOL_B_GATEWAY" "$POOL_B"',
    'Gateway controller did not propagate $INFRA_ANNOTATION_KEY=$INFRA_ANNOTATION_VALUE',
    'remove_run_gateways() {',
    'praxis.fast/qualification-run"]==$run',
    'wait_for_private_gateway_address() {',
    '10.0.0.0/8',
    '172.16.0.0/12',
    '192.168.0.0/16',
    'continued resolving to public addresses for 30 seconds; fail-closed cleanup will remove the run-owned pool Gateways',
    'now - public_since >= 30',
    'Gateway $gateway for $pool resolves only to RFC1918/ULA private addresses',
    'resolvedPrivateAddresses:(if $privateAddresses=="" then [] else ($privateAddresses|split(",")) end)',
    'aws-internal-annotation-and-nonpublic-dns-from-runner',
    'kubernetes.io/service-name=$service',
    'conditions.ready==true',
    'Gateway %s Service %s has no ready EndpointSlice target',
    'oc apply --dry-run=server -f "$EVIDENCE_DIR/pool-gateways.yaml"',
    'if [[ "$MODE" == install ]]; then',
    'oc apply -f "$EVIDENCE_DIR/pool-gateways.yaml"',
)
missing = [item for item in checks if item not in text]
if missing:
    print("pool Gateway bounded readiness contract missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

render_exit = text.index('if [[ "$MODE" == render ]]; then')
apply = text.index('oc apply -f "$EVIDENCE_DIR/pool-gateways.yaml"')
if render_exit > apply:
    print("render mode must exit before Gateway apply", file=sys.stderr)
    raise SystemExit(1)

print("pool Gateway render, ownership, bounded status and endpoint readiness contract: PASS")
PY
