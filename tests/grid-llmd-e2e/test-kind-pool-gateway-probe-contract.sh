#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source="$SCRIPT_DIR/probe-kind-pool-gateway.sh"
peer_validator="$SCRIPT_DIR/verify_kind_probe_peer.py"

python3 - "$source" "$peer_validator" <<'PY'
from pathlib import Path
import sys

text = Path(sys.argv[1]).read_text(encoding="utf-8")
peer_text = Path(sys.argv[2]).read_text(encoding="utf-8")
checks = (
    'SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"',
    'KIND_POOL_SETUP_PROBE_MODE:-plan',
    'KIND_POOL_SETUP_PROBE_POOL:-',
    'EXPECTED_API_SERVER',
    'KUBE_CONTEXT',
    'GRID_RUN_LABEL',
    'GRID_RESOURCE_NAMESPACE',
    'EXPECTED_SIMULATOR_DIGEST',
    'EXPECTED_EPP_DIGEST',
    'EXPECTED_API_DIGEST',
    'API_ROUTE="maas-api-route-$POOL"',
    'target API guard failed',
    'automountServiceAccountToken: false',
    'kubectl create token',
    '--duration=10m',
    '--audience=https://kubernetes.default.svc',
    'ephemeral:true',
    'expiresIn:"15m"',
    'sanitize_kind_maas_error.py',
    'maas-api-key-create-error.json',
    'kubectl get httproute "$API_ROUTE" -n maas-system',
    '.path.value=="/v1/api-keys"',
    '.name==$service and .port==8443',
    'API_URL="http://127.0.0.1:$GATEWAY_LOCAL_PORT/v1/api-keys"',
    'kubectl port-forward --address 127.0.0.1 -n maas-system "svc/$GATEWAY"',
    'grid-llmd-$POOL-setup',
    'resolvedModelAlias',
    'MODEL_ALIAS="$(jq -er \'.status.resolvedModelAlias\' "$EVIDENCE_DIR/maasmodelref.json")"',
    'route_path="/v1/chat/completions"',
    '(.name|ascii_downcase)=="x-gateway-model-name"',
    '--arg model "$MODEL_ALIAS"',
    '"X-Gateway-Model-Name: $MODEL_ALIAS"',
    'requestPath:$requestPath',
    'kubectl get llminferenceservice',
    '.status.observedGeneration==$g',
    'kubectl get inferencepool',
    'capture_deployment "$TASK_NAMESPACE" "$SIMULATOR_DEPLOYMENT"',
    'capture_deployment "$TASK_NAMESPACE" "$EPP_DEPLOYMENT"',
    'kubectl get llminferenceservice "$peer" -n "$TASK_NAMESPACE"',
    'peer-deployment-orphan.json',
    'python3 "$SCRIPT_DIR/verify_kind_probe_peer.py"',
    'kubectl get replicasets -n "$namespace" -o json',
    '.kind=="Deployment" and .name==$deployment and .controller==true',
    '.kind=="ReplicaSet"',
    'serviceaccount "$PROBE_SERVICE_ACCOUNT"',
    'API_SERVICE="maas-api-$POOL"',
    'Authorization: Bearer $TOKEN',
    'Authorization: Bearer $API_KEY',
    'response_request_id',
    'rg -F "$response_request_id"',
    "rg -F 'Finished processing request'",
    '"$receipt_lines" == 1',
    'revoke_key ||',
    'keyBytesRecorded:false',
    'credentialBytesRecorded:false',
    'gridTaskGatewayProviderProxyAndExtProcTraversed:false',
    'qualificationTraffic:false',
)
missing = [item for item in checks if item not in text]
if missing:
    print("Kind pool Gateway setup-probe contract missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

peer_checks = (
    'if pool != "pool-a"',
    'peer_deployment_present or peer_uids',
    'pairwiseDisjointVerified',
    'pairwiseDisjointDeferred',
    'set(target_uids).intersection(peer_uids)',
)
missing_peer = [item for item in peer_checks if item not in peer_text]
if missing_peer:
    print("Kind pool staged peer validation contract missing: " + repr(missing_peer), file=sys.stderr)
    raise SystemExit(1)

if 'curl -k' in text or 'insecure' in text or 'skip-tls' in text:
    print("setup probe must not disable TLS verification on TLS-enabled paths", file=sys.stderr)
    raise SystemExit(1)

if 'port-forward --address 127.0.0.1 -n maas-system "svc/$API_SERVICE"' in text:
    print("MaaS API key operations must traverse the pool Gateway AuthPolicy", file=sys.stderr)
    raise SystemExit(1)

if 'X-MaaS-Username' in text or 'X-MaaS-Identity' in text:
    print("probe must not spoof Authorino-trusted identity headers", file=sys.stderr)
    raise SystemExit(1)

if 'echo "$TOKEN"' in text or 'echo "$API_KEY"' in text or 'TOKEN >' in text or 'API_KEY >' in text:
    print("ephemeral credential bytes must never be written to evidence", file=sys.stderr)
    raise SystemExit(1)

print("Kind authorized pool Gateway probe and independent receipt contract: PASS")
PY
