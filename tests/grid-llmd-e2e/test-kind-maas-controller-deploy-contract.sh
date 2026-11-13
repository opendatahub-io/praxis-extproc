#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/deploy-kind-maas-controller.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = [
    '"$SOURCE_DIR/deployment/base/maas-controller/overlays/xks"',
    'kustomize build --load-restrictor LoadRestrictionsNone',
    'render_kind_maas_controller_deployment.py',
    '--controller-image "$CONTROLLER_IMAGE"',
    '--api-image "$API_IMAGE"',
    '--payload-image "$PAYLOAD_IMAGE"',
    'install-kind-maas-controller.sh',
    '--monitoring-namespace ""'
]
if any(token not in source for token in required):
    raise SystemExit("Kind MaaS deployment must render the locked xKS overlay, pin runtime images, and use the readiness installer")
if "kubectl patch" in source or 'k apply patch' in source:
    raise SystemExit("Kind MaaS deployment must not patch live workloads or suppress required checks")
if 'sourceSha:$sourceSha' not in source or 'xksOverlay:true' not in source or 'resourcesChanged:false' not in source:
    raise SystemExit("Kind MaaS deployment evidence must include source identity and xKS overlay use")
print("MaaS Kind xKS source-render, image pin, and readiness-installer contract: PASS")
PY
