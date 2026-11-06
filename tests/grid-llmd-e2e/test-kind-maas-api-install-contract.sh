#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-maas-api.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = [
    "maas-api/deploy/overlays/xks/kustomization.yaml",
    "secretName: opendatahub-ca",
    "MaaS xKS overlay no longer replaces the OpenShift CA volume",
    "k get gateway maas-default-gateway",
    "run-owned MaaS Gateway must be current-generation Accepted and Programmed",
    '"$SCRIPT_DIR/verify-kind-maas-api.sh"',
    "controller-owned-verify/RESULT.json",
    "controllerOperandsMutated:false",
    "failure-events.json",
]
if any(item not in source for item in required):
    raise SystemExit("MaaS API gate must validate the xKS overlay and wait for controller-owned operands")
for forbidden in (
    "deployment/base/maas-api/overlays/tls\"\n[[ -f",
    "k create -f",
    "k apply -f",
    "kubectl patch",
    "set image",
    "set -x",
    "--from-literal",
    "render_kind_maas_api.py",
):
    if forbidden in source:
        raise SystemExit(f"MaaS API gate must not statically apply controller-owned or OpenShift-only resources: {forbidden}")
print("MaaS API Kind controller ownership, xKS source, and read-only gate contract: PASS")
PY
