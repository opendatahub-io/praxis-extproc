#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-grid-operator.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
verify_start = source.index("verify_runtime() {")
verify_end = source.index("wait_for_operator_deployment() {", verify_start)
verify = source[verify_start:verify_end]
deployment_start = verify_end
deployment_end = source.index("wait_for_operator_pod_image() {", deployment_start)
deployment_wait = source[deployment_start:deployment_end]
pod_start = deployment_end
pod_end = source.index("wait_for_praxis_config() {", pod_start)
pod_wait = source[pod_start:pod_end]
required = (
    'RUNTIME_READY_TIMEOUT_SECONDS="${GRID_OPERATOR_RUNTIME_TIMEOUT_SECONDS:-300}"',
    '[[ "$RUNTIME_READY_TIMEOUT_SECONDS" =~ ^[1-9][0-9]{0,3}$ ]]',
    'wait_for_operator_deployment "$deployment_json"',
    "wait_for_operator_pod_image",
    'deadline=$((SECONDS + RUNTIME_READY_TIMEOUT_SECONDS))',
    '.status.observedGeneration==.metadata.generation',
    '(.status.readyReplicas // 0)==1',
    'while ((SECONDS < deadline)); do',
    'operator-pod-readiness-attempts.jsonl',
    'all(.[0].containers[];.ready==true and .image==$image and (.imageID|endswith($digest)))',
)
missing = [fragment for fragment in required if fragment not in source]
if missing:
    print("operator runtime readiness retry contract is incomplete: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

if not (verify.index('wait_for_operator_deployment') < verify.index('wait_for_operator_pod_image') < verify.index('grid-crd-readiness.json')):
    print("operator and exact-image readiness must pass before later runtime checks", file=sys.stderr)
    raise SystemExit(1)
if "while ((SECONDS < deadline)); do" not in deployment_wait or "sleep 5" not in deployment_wait:
    print("Deployment readiness must poll current-generation Available state", file=sys.stderr)
    raise SystemExit(1)
if "while ((SECONDS < deadline)); do" not in pod_wait or "sleep 5" not in pod_wait:
    print("Pod readiness must poll instead of failing on a transient image pull/probe delay", file=sys.stderr)
    raise SystemExit(1)

print("operator Deployment and pinned-image Pod readiness use bounded polling: PASS")
PY
