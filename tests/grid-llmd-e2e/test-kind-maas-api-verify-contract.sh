#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/verify-kind-maas-api.sh" <<'PY'
from pathlib import Path
import sys
script_dir = Path(sys.argv[1]).parent
source = (script_dir / "verify-kind-maas-api.sh").read_text(encoding="utf-8")
required = [
    "MODE=platform-ready",
    "api-bootstrap",
    "platform-ready",
    'if [[ "$MODE" == api-bootstrap && "$observed" == true ]]',
    'if [[ "$MODE" == platform-ready && "$ready" == true ]]',
    "configReady:$configReady",
    "maas-controller-logs-failure.txt",
    "maas-api-logs-failure.txt",
    "payload-processing-logs-failure.txt",
]
if any(item not in source for item in required):
    raise SystemExit("MaaS API verifier must separate transport bootstrap from full platform readiness and capture failed controller logs")
required = [
    "MaaS controller-owned API",
    "kind==\"Config\" and .name==\"default\" and .uid==$uid",
    "observedGeneration==$g",
    "(.imageID|endswith(\"@\"+$digest))",
    "oneReadyServiceEndpoint",
    "routeAcceptedAndResolved",
    "capture_failure",
    "Config/default did not reach current-generation Ready",
    "AITenant and MaasTenantConfig",
    "config-readiness-polls.tsv",
    "service-readiness-polls.tsv",
    "route-readiness-polls.tsv",
    "k get aitenant models-as-a-service -n ai-tenants -o json",
    "k get maastenantconfig default-tenant -n models-as-a-service -o json",
]
if any(item not in source for item in required):
    raise SystemExit("MaaS API verifier is missing bounded default-tenant or controller-owned runtime readiness proof")
config_wait = source.index('deadline=$((SECONDS + TIMEOUT))')
config_ready_wait = source.index('while (( SECONDS < deadline ))', config_wait)
bootstrap_observation = source.index('if [[ "$MODE" == api-bootstrap && "$observed" == true ]]', config_ready_wait)
platform_ready_check = source.index('if [[ "$MODE" == platform-ready && "$ready" == true ]]', bootstrap_observation)
service_lookup = source.index('k get service maas-api', platform_ready_check)
deployment_wait = source.index('k get deployment maas-api', service_lookup)
route_wait = source.index('route-readiness-polls.tsv', deployment_wait)
if not config_wait < config_ready_wait < bootstrap_observation < platform_ready_check < service_lookup < deployment_wait < route_wait:
    raise SystemExit("MaaS API verifier must separately gate observed Config bootstrap and full Ready before checking controller operands")
if any(item in source for item in ("kubectl apply", "kubectl create", "kubectl patch", "set image", "set env")):
    raise SystemExit("MaaS API verifier must not mutate controller-owned resources")
lifecycle = (script_dir / "run-kind-lifecycle.sh").read_text(encoding="utf-8")
order = [
    lifecycle.index("run_phase maas-api-bootstrap"),
    lifecycle.index('kubectl --kubeconfig="$KUBECONFIG" --context="$KUBE_CONTEXT" apply -f "$AIGC_BUNDLE"'),
    lifecycle.index('rollout status deployment/ai-gateway-controller'),
    lifecycle.index("run_phase payload-egress"),
    lifecycle.index('apply -f "$RUN_DIR/platform/default-aitenant.yaml"'),
    lifecycle.index("run_phase maas-platform-ready"),
    lifecycle.index("SERVING_POOLS_MODE=verify"),
]
if order != sorted(order):
    raise SystemExit("Kind lifecycle must install AIGC payload prerequisites before requiring full MaaS tenant readiness")
print("MaaS API bootstrap/full readiness separation and lifecycle dependency ordering: PASS")
PY
