#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source="$SCRIPT_DIR/configure-kind-pool-maas-auth.sh"

python3 - "$source" <<'PY'
from pathlib import Path
import sys

text = Path(sys.argv[1]).read_text(encoding="utf-8")
checks = (
    'KIND_POOL_MAAS_AUTH_MODE:-plan',
    'EXPECTED_API_SERVER',
    'KUBE_CONTEXT',
    'GRID_RUN_LABEL',
    'GRID_RESOURCE_NAMESPACE',
    'POOL_SELECTION="${KIND_POOL_MAAS_AUTH_POOL:-both}"',
    'pool-a) SELECTED_POOLS=(pool-a)',
    'pool-b) SELECTED_POOLS=(pool-b)',
    'both) SELECTED_POOLS=(pool-a pool-b)',
    'for pool in "${SELECTED_POOLS[@]}"; do',
    'target API guard failed',
    'metadata.labels["praxis.fast/qualification-run"]==$run',
    'grid-llmd-$pool',
    'gateway.networking.k8s.io/gateway-name',
    'port==443',
    'conditions.ready==true',
    'pool-kserve-route',
    'AITenant',
    'tenantNamespace',
    'ai-gateway.opendatahub.io/tenant',
    'MaaSModelRef',
    'kind: LLMInferenceService',
    'MaaSAuthPolicy',
    'system:authenticated',
    'MaaSSubscription',
    'tokenRateLimits',
    'observedGeneration==$g',
    'status.phase=="Ready"',
    'status.phase=="Active"',
    'app.kubernetes.io/managed-by"]=="maas-controller"',
    'any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name==$gateway and .uid==$gatewayUid)',
    'Accepted" and .status=="True"',
    'Enforced" and .status=="True"',
    'inferenceTrafficSent:false',
)
missing = [item for item in checks if item not in text]
if missing:
    print("Kind MaaS pool authorization contract missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

order = [
    text.index('kubectl get gateway "$gateway"'),
    text.index('kubectl get httproute "$route"'),
    text.index('kubectl apply -f "$aitenant_manifest"'),
    text.index('kubectl apply -f "$modelref_manifest"'),
    text.index('type=="RuntimeReady"'),
    text.index('kubectl apply -f "$manifest"'),
    text.index('status.phase=="Ready" and .status.httpRouteGatewayName==$gateway'),
    text.index('Current-generation MaaS pool authorization paths are Ready for ${SELECTED_POOLS[*]}'),
]
if order != sorted(order):
    print("Kind MaaS auth resources are not applied after their Gateway/route dependencies", file=sys.stderr)
    raise SystemExit(1)

for forbidden in ("Authorization: Bearer", "apiKey:", "token:", "privateKey:"):
    if forbidden in text:
        print(f"credential material must not be embedded in the MaaS setup helper: {forbidden}", file=sys.stderr)
        raise SystemExit(1)

print("Kind MaaS tenant, model reference, policy, subscription, and readiness contract: PASS")
PY
