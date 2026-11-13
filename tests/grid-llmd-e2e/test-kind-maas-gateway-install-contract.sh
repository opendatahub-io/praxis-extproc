#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-maas-gateway.sh" "$SCRIPT_DIR/render_kind_maas_gateway.py" <<'PY'
from pathlib import Path
import sys
installer = Path(sys.argv[1]).read_text(encoding="utf-8")
renderer = Path(sys.argv[2]).read_text(encoding="utf-8")
required = [
    "test/kind-env/manifests/30-gateway.yaml",
    "AIGC worktree is not clean at the locked detached SHA",
    "refusing adoption",
    "current-generation Accepted/Programmed",
    "lacks the source-supported Kind internal address or current listener readiness",
    "listeners[]?.conditions",
    "kind-status-address.json",
    "--subresource=status",
    "install-kind-maas-default-gateway.sh",
    "capture_failure",
]
if any(item not in installer for item in required):
    raise SystemExit("MaaS Gateway installer lacks source lock, ownership, readiness, Kind adaptation, or diagnostics")
listener = installer.find("Gateway listener not current-generation")
endpoint = installer.find("Gateway Istio Service has no Ready HTTP endpoint")
status_patch = installer.find("--subresource=status")
programmed = installer.find("lacks the source-supported Kind internal address or current listener readiness")
alias = installer.find("install-kind-maas-default-gateway.sh")
if not (0 <= listener < endpoint < status_patch < programmed < alias):
    raise SystemExit("Gateway status address must follow Ready listener/endpoint and precede final Programmed and alias gates")
if "'{status:{addresses:[{type:\"Hostname\",value:$host}]}}'" not in installer:
    raise SystemExit("Kind Gateway status adaptation must patch status.addresses, not an unknown top-level field")
if "status.code()==" in renderer or "spec.get(\"addresses\")" in renderer:
    raise SystemExit("Gateway renderer must preserve the AIGC source listener and address ownership")
for item in ("gatewayClassName"):
    if item not in renderer:
        raise SystemExit("Gateway renderer must validate the source-provided Istio GatewayClass")
print("MaaS Gateway source lock, Kind readiness adaptation, ownership, and diagnostics contract: PASS")
PY
