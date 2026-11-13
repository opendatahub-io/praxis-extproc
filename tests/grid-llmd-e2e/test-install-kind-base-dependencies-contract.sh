#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-base-dependencies.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = {
    "explicit API identity": 'server" == "$EXPECTED_API_SERVER"',
    "Gateway API digest pin": 'GATEWAY_API_SHA256=',
    "cert-manager chart digest pin": 'CERT_MANAGER_CHART_SHA256=',
    "Istio binary version and digest": 'ISTIOCTL_SHA256=',
    "Kuadrant chart digest pin": 'KUADRANT_CHART_SHA256=',
    "Gateway API CRD establishment": 'gateway-api-crd-waits.txt',
    "cert-manager readiness": 'cert-manager deployments are not all current and Available',
    "Kuadrant readiness": 'Kuadrant and managed authorization deployments are not current and Available',
    "Authorino readiness": "Authorino is not Ready",
    "zero Pending Pods": "Pending Pods remain after the bounded dependency convergence wait",
    "failure diagnostics": "failure-workloads.yaml",
}
missing = [label for label, fragment in required.items() if fragment not in source]
if missing:
    raise SystemExit("missing Kind dependency guarantees: " + ", ".join(missing))

ordered = [
    'k apply -f "$EVIDENCE_DIR/gateway-api.yaml"',
    'h upgrade --install cert-manager',
    'install-kind-istio.sh',
    'h upgrade --install kuadrant-operator',
    'k apply -f "$EVIDENCE_DIR/kuadrant-instance.yaml"',
]
positions = [source.index(fragment) for fragment in ordered]
if positions != sorted(positions):
    raise SystemExit("Kind dependency order must be Gateway API, cert-manager, Istio, then Kuadrant")
if "rollout restart deployment/kuadrant-operator-controller-manager" in source:
    raise SystemExit("dependency installer must not repair Kuadrant discovery with a manual restart")
print("Kind base-dependency install contract passed")
PY
