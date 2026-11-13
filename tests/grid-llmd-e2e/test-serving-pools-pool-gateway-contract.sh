#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'POOL_A_GATEWAY_NAME="$(env_default POOL_A_GATEWAY_NAME grid-llmd-pool-a)"',
    'POOL_B_GATEWAY_NAME="$(env_default POOL_B_GATEWAY_NAME grid-llmd-pool-b)"',
    'LLMISVC_GATEWAY_SECTION_NAME="$(env_default LLMISVC_GATEWAY_SECTION_NAME inference)"',
    '"$POOL_A") printf \'%s\' "$POOL_A_GATEWAY_NAME" ;;',
    '"$POOL_B") printf \'%s\' "$POOL_B_GATEWAY_NAME" ;;',
    'verify_pool_gateways_ready() {',
    '.metadata.labels["praxis.fast/qualification-run"]==$run',
    '.metadata.labels["praxis.fast/simulator-pool"]==$pool',
    'any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g)',
    'any(.status.conditions[]?;.type=="Programmed" and .status=="True" and .observedGeneration==$g)',
    '.metadata.annotations[$key]==$value',
    'kubernetes.io/service-name=$service',
    'conditions.ready==true',
)
missing = [item for item in required if item not in source]
forbidden = ('${POOL_A_GATEWAY_NAME:-$LLMISVC_GATEWAY_NAME}', '${POOL_B_GATEWAY_NAME:-$LLMISVC_GATEWAY_NAME}')
present = [item for item in forbidden if item in source]
if missing or present:
    print(f"pool Gateway binding contract incomplete: missing={missing!r}, fallback={present!r}", file=sys.stderr)
    raise SystemExit(1)

apply_start = source.index("  apply)\n", source.index('case "$MODE" in', source.index("api_access\n")))
apply_end = source.index("  verify)\n", apply_start)
apply = source[apply_start:apply_end]
ordered = (
    "verify_platform",
    "verify_pool_gateways_ready",
    "ensure_no_llmisvc_before_transport_apply",
    "configure_llmisvc_transport",
    'apply_one_pool "$POOL_A"',
)
positions = [apply.index(item) for item in ordered]
if positions != sorted(positions):
    print("pool Gateways must pass ownership/readiness checks before the DSCI change and pool apply", file=sys.stderr)
    raise SystemExit(1)

print("serving pools require distinct ready run-owned internal Gateways before applying LLMInferenceServices: PASS")
PY
