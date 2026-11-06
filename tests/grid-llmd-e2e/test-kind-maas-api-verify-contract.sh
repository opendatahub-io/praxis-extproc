#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/verify-kind-maas-api.sh" <<'PY'
from pathlib import Path
import sys
source = Path(sys.argv[1]).read_text(encoding="utf-8")
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
config_ready_check = source.index('config_ready=true', config_ready_wait)
service_lookup = source.index('k get service maas-api', config_ready_check)
deployment_wait = source.index('k get deployment maas-api', service_lookup)
route_wait = source.index('route-readiness-polls.tsv', deployment_wait)
if not config_wait < config_ready_wait < config_ready_check < service_lookup < deployment_wait < route_wait:
    raise SystemExit("MaaS API verification must wait within one bounded deadline for Config, Service, runtime, and route readiness")
if any(item in source for item in ("kubectl apply", "kubectl create", "kubectl patch", "set image", "set env")):
    raise SystemExit("MaaS API verifier must not mutate controller-owned resources")
print("MaaS API controller ownership and read-only verification contract: PASS")
PY
