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
]
if any(item not in source for item in required):
    raise SystemExit("MaaS API verifier is missing controller ownership or runtime readiness proof")
if any(item in source for item in ("kubectl apply", "kubectl create", "kubectl patch", "set image", "set env")):
    raise SystemExit("MaaS API verifier must not mutate controller-owned resources")
print("MaaS API controller ownership and read-only verification contract: PASS")
PY
