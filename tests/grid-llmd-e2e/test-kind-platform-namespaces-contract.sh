#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
python3 - "$SCRIPT_DIR/create-kind-platform-namespaces.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = [
    "--expected-api-server) EXPECTED_API_SERVER=",
    "refusing to adopt pre-existing namespace",
    "grep -qi 'not found'",
    "praxis.kind/run: \"$RUN_ID\"",
    "istio-injection: enabled",
    "local-env.opendatahub.io/gateway-tenant: models-as-a-service",
    'k create --dry-run=server -f "$manifest"',
    'k create -f "$EVIDENCE_DIR/$namespace.yaml"',
    "existingObjectsAdopted:false",
]
if any(item not in source for item in required):
    raise SystemExit("Kind platform namespace setup must be run-owned, non-adopting, and server-dry-run validated")
for namespace in ("maas-system", "opendatahub", "ai-tenants", "models-as-a-service"):
    if namespace not in source:
        raise SystemExit(f"required MaaS/AIGC namespace is missing: {namespace}")
for forbidden in ("kubectl apply", "kubectl patch", "set -x"):
    if forbidden in source:
        raise SystemExit(f"namespace setup must create only fresh resources and not expose or mutate prior state: {forbidden}")
print("Kind platform namespace ownership, labels, and non-adoption contract: PASS")
PY
