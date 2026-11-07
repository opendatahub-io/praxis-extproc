#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    "wait_for_kind_pending_pods()",
    "timeout_seconds=120 poll_seconds=2",
    "Failed\" or .phase==\"Unknown",
    '.podScheduled.reason==\"Unschedulable\"',
    "pending-pod-polls.tsv",
    "pending-convergence-result.json",
    "events-on-pending-failure.json",
    "wait_for_kind_platform_deployments()",
    "timeout_seconds=180 poll_seconds=2",
    "platform-deployment-polls.tsv",
    "platform-deployment-readiness-result.json",
    "events-on-deployment-timeout.json",
    "kserve/llmisvc-controller-manager",
    "maas-system/payload-pre-processing",
    "maas-system/payload-processing",
    "wait_for_kind_platform_deployments \"$dir/deployment-convergence\"",
    "wait_for_kind_pending_pods \"$dir/pending-convergence\"",
    "wait_for_kind_pending_pods \"$dir/pending-convergence\"",
)
missing = [item for item in required if item not in source]
if missing:
    raise SystemExit("Kind bounded platform convergence contract missing: " + repr(missing))

capacity = source.split("verify_kind_capacity() {", 1)[1].split("\n}", 1)[0]
if capacity.index('wait_for_kind_pending_pods "$dir/pending-convergence"') > capacity.index('oc get pods -A -o json'):
    raise SystemExit("capacity preflight must wait for scheduled Pending Pods before its final zero-Pending snapshot")

platform = source.split("verify_kind_platform() {", 1)[1].split("\n}", 1)[0]
if platform.index('wait_for_kind_platform_deployments "$dir/deployment-convergence"') > platform.index('for dep in'):
    raise SystemExit("Kind platform Deployments must converge before the final platform assertion")

print("Kind bounded Pending-Pod and platform Deployment convergence contracts: PASS")
PY
