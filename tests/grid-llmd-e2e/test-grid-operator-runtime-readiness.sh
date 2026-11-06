#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-grid-operator.sh" "$SCRIPT_DIR/single-ready-pinned-pod.jq" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
gate_path = Path(sys.argv[2])
gate = gate_path.read_text(encoding="utf-8")
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
    'GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME="${GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME:-${GRID_GATEWAY_DEPLOYMENT_NAME:-grid-gateway}}"',
    '[[ "$RUNTIME_READY_TIMEOUT_SECONDS" =~ ^[1-9][0-9]{0,3}$ ]]',
    'wait_for_operator_deployment "$deployment_json"',
    "wait_for_operator_pod_image",
    'deadline=$((SECONDS + RUNTIME_READY_TIMEOUT_SECONDS))',
    '.status.observedGeneration==.metadata.generation',
    '(.status.readyReplicas // 0)==1',
    'while ((SECONDS < deadline)); do',
    'operator-pod-readiness-attempts.jsonl',
    'specContainers:[.spec.containers[]?|{name,image}]',
    '-f "$SCRIPT_DIR/single-ready-pinned-pod.jq" "$pod_json"',
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

if '.[0].specContainers[0].image == $image' not in gate or '(.imageID | endswith($digest))' not in gate:
    print("runtime image gate must compare the configured Pod spec to the image pin and the runtime imageID to its digest", file=sys.stderr)
    raise SystemExit(1)
if '.[0].containers[0].image == $image' in gate:
    print("runtime status image is not the configured pull reference and must not be compared to it", file=sys.stderr)
    raise SystemExit(1)

import json
import subprocess

image = "registry.invalid/operator@sha256:" + "a" * 64
digest = "sha256:" + "a" * 64
good = [{
    "deletionTimestamp": None,
    "phase": "Running",
    "specContainers": [{"name": "operator", "image": image}],
    "containers": [{"name": "operator", "image": "sha256:" + "b" * 64,
                    "imageID": "registry.invalid/operator@" + digest, "ready": True}],
}]
def accepts(pods):
    result = subprocess.run(
        ["jq", "-e", "--arg", "image", image, "--arg", "digest", digest, "-f", str(gate_path)],
        input=json.dumps(pods), text=True, capture_output=True, check=False,
    )
    return result.returncode == 0

if not accepts(good):
    print("valid pinned imageID must pass even when runtime image config ID differs from the pull reference", file=sys.stderr)
    raise SystemExit(1)
bad_spec = json.loads(json.dumps(good))
bad_spec[0]["specContainers"][0]["image"] = "registry.invalid/operator:stale"
if accepts(bad_spec):
    print("wrong configured image reference must fail the runtime image gate", file=sys.stderr)
    raise SystemExit(1)
bad_digest = json.loads(json.dumps(good))
bad_digest[0]["containers"][0]["imageID"] = "registry.invalid/operator@sha256:" + "c" * 64
if accepts(bad_digest):
    print("wrong runtime image digest must fail the runtime image gate", file=sys.stderr)
    raise SystemExit(1)
terminating = json.loads(json.dumps(good))
terminating[0]["deletionTimestamp"] = "2026-11-02T00:00:00Z"
if accepts(terminating):
    print("terminating operator Pod must not pass readiness", file=sys.stderr)
    raise SystemExit(1)

print("operator Deployment and pinned-image Pod readiness use bounded polling: PASS")
PY
