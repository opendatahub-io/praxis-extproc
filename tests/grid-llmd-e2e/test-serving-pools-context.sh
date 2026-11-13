#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import re
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
checks = (
    'POOL_A_GATEWAY_NAME="$(env_default POOL_A_GATEWAY_NAME grid-llmd-pool-a)"',
    'POOL_B_GATEWAY_NAME="$(env_default POOL_B_GATEWAY_NAME grid-llmd-pool-b)"',
    'pool_gateway_name() {',
    'gateway_name="$(pool_gateway_name "$pool")"',
    'distinct task Gateway names',
    '[[ -n "$KUBE_CONTEXT" ]] || { printf \'KUBE_CONTEXT is required for non-plan serving pool modes',
    'oc_bin="$(type -P oc)"',
    '"$oc_bin" --context="$KUBE_CONTEXT" start-build',
)
missing = [check for check in checks if check not in source]
if missing:
    print("serving pool context contract is missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

if re.search(r"(?m)^\s*oc start-build\b", source):
    print("bounded simulator build must not bypass the pinned context wrapper", file=sys.stderr)
    raise SystemExit(1)

build = source[source.index("build_simulator() {"):source.index("make_task_scheduler_config() {")]
timeout_call = re.search(r"timeout --foreground.*?build-output\.txt", build, re.S)
if not timeout_call or '"$oc_bin" --context="$KUBE_CONTEXT" start-build' not in timeout_call.group(0):
    print("timeout-wrapped simulator build must pass the explicit Kubernetes context", file=sys.stderr)
    raise SystemExit(1)

print("serving pool build context is pinned for bounded child commands: PASS")
PY
