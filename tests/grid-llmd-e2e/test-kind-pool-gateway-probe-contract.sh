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
    'API_TLS_HOST="$API_SERVICE.maas-system.svc.cluster.local"',
    'API_CA_FILE="${KIND_POOL_SETUP_PROBE_API_CA_FILE:-$PRIVATE_STATE_DIR/maas-api-root-ca.crt}"',
    'API_ROUTE="maas-api-route-$POOL"',
    'target API guard failed',
    'ephemeral:true',
    'expiresIn:"15m"',
    'sanitize_kind_maas_error.py',
    'maas-api-key-create-error.json',
    'kubectl get httproute "$API_ROUTE" -n maas-system',
    '.path.value=="/v1/api-keys"',
    '.name==$service and .port==8443',
    'API_URL="https://$API_TLS_HOST:$API_LOCAL_PORT/v1/api-keys"',
    'command kubectl --context="$KUBE_CONTEXT" port-forward --address 127.0.0.1 -n maas-system "svc/$API_SERVICE" "$API_LOCAL_PORT:8443"',
    'wait_port_forward "$API_FORWARD_PID" "$API_LOCAL_PORT" pool-MaaS-API',
    '--cacert "$API_CA_FILE"',
    '--resolve "$API_TLS_HOST:$API_LOCAL_PORT:127.0.0.1"',
    'X-MaaS-Username: $PROBE_USERNAME',
    'X-MaaS-Group: $PROBE_GROUPS',
    'maas-api-direct-health-http-status.txt',
    'API_HEALTH_STATUS" == 200',
    'GATEWAY_LOCAL_PORT="$(python3 -c',
    'command kubectl --context="$KUBE_CONTEXT" port-forward --address 127.0.0.1 -n maas-system "svc/$GATEWAY"',
    'wait_port_forward "$GATEWAY_FORWARD_PID" "$GATEWAY_LOCAL_PORT" pool-Gateway "$EVIDENCE_DIR/pool-gateway-port-forward.log"',
    'rg -Fq "Forwarding from 127.0.0.1:$port ->" "$logfile"',
    'grid-llmd-$POOL-setup',
    'resolvedModelAlias',
    'MODEL_ALIAS="$(jq -er \'.status.resolvedModelAlias\' "$EVIDENCE_DIR/maasmodelref.json")"',
    'route_path="/v1/chat/completions"',
    '--arg path "$route_path" --arg model "$MODEL_ALIAS"',
    '.path.type=="Exact" and .path.value==$path',
    '(.name|ascii_downcase)=="x-gateway-model-name"',
    '--arg pool "$POOL-inference-pool"',
    'any(.backendRefs[]?;.kind=="InferencePool" and .name==$pool and .port==8000)',
    'modelSource:"MaaSModelRef.status.resolvedModelAlias"',
    'internalHeaderSource:"trusted-pre-extproc"',
    'clientSuppliedRoutingHeader:false',
    'requestPath:$requestPath',
    'credentialProvisioningThroughPoolGateway:false',
    'inferenceAuthorizationThroughPoolGateway:true',
    'KIND_POOL_AUTH_STABLE_TIMEOUT_SECONDS',
    'wait_for_stable_auth_policy',
    'ConflictingAuthPolicy',
    'kubectl get authpolicies.kuadrant.io -A',
    '--slurpfile policies <(printf \'%s\' "$policies")',
    'kubectl get wasmplugin "kuadrant-$GATEWAY"',
    'kubectl get envoyfilter "kuadrant-auth-$GATEWAY"',
    'stablePolls:$stablePolls',
    'routePolicies:',
    'wait_for_stable_auth_policy',
    'capture_probe_failure',
    'gateway.networking.k8s.io/gateway-name=$GATEWAY',
    'maas.opendatahub.io/tenant-instance=$API_DEPLOYMENT',
    'deployment/authorino',
    '-c istio-proxy --since=15m',
    '-c maas-api --since=15m',
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
    'API_SERVICE="maas-api-$POOL"',
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

if '--argjson policies "$policies"' in text:
    print("cluster-wide AuthPolicy JSON must not be passed through argv", file=sys.stderr)
    raise SystemExit(1)

if 'X-Gateway-Model-Name: $MODEL_ALIAS' in text:
    print("setup probe must let trusted payload preprocessing derive the routing header from the canonical body model", file=sys.stderr)
    raise SystemExit(1)

if 'port-forward --address 127.0.0.1 -n maas-system "svc/$API_SERVICE" "$API_LOCAL_PORT:8443"' not in text:
    print("ephemeral key setup must use the pool-specific MaaS API service", file=sys.stderr)
    raise SystemExit(1)

if 'kubectl port-forward --address 127.0.0.1 -n maas-system "svc/$GATEWAY"' in text:
    print("Gateway port-forward must be a direct kubectl process so its PID can be stopped", file=sys.stderr)
    raise SystemExit(1)

inference_start = text.index('gateway_url="http://127.0.0.1:$GATEWAY_LOCAL_PORT$request_path"')
if 'X-MaaS-Username' in text[inference_start:] or 'X-MaaS-Group' in text[inference_start:]:
    print("test identity headers must stay on the credential-issuance backchannel, not inference", file=sys.stderr)
    raise SystemExit(1)

if 'kubectl create token' in text or 'Authorization: Bearer $TOKEN' in text:
    print("setup credentials should not depend on a Kubernetes bearer token", file=sys.stderr)
    raise SystemExit(1)

if 'echo "$API_KEY"' in text or 'API_KEY >' in text:
    print("ephemeral credential bytes must never be written to evidence", file=sys.stderr)
    raise SystemExit(1)

if text.rindex('wait_for_stable_auth_policy') > text.index('key_result="$(curl'):
    print("MaaS caller credentials must not be minted until authorization policy state is current and stable", file=sys.stderr)
    raise SystemExit(1)

print("Kind authorized pool Gateway probe and independent receipt contract: PASS")
PY
