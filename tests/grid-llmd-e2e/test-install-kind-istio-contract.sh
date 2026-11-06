#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
helper="$SCRIPT_DIR/install-kind-istio.sh"
python3 - "$helper" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    "SUPPORT_GATEWAY_API_INFERENCE_EXTENSION=true",
    "ENABLE_GATEWAY_API_INFERENCE_EXTENSION=true",
    'rollout status deployment/istiod',
    'wait gatewayclass/istio --for=condition=Accepted',
    'get --raw=/readyz',
    'istioctl-sha256.txt',
    'get namespace istio-system -o json',
    'printf \'{}\\n\' >"$EVIDENCE_DIR/istiod-before.json"',
    'installing namespace and control plane',
)
missing = [item for item in required if item not in source]
if missing:
    raise SystemExit(f"Kind Istio setup omits requirements: {missing}")
if "kubectl patch" in source or "kubectl set env" in source:
    raise SystemExit("Kind Istio feature setup must use supported istioctl values")
print("Kind Istio inference extension installation contract: PASS")
PY
