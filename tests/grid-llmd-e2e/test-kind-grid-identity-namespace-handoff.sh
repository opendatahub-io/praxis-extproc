#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/run-kind-lifecycle.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
for phase in ("task-gateway-bootstrap", "task-gateway-grid-config"):
    start = source.index(f"phase_start {phase}")
    end = source.index("phase_end 0", start)
    block = source[start:end]
    required = 'GRID_IDENTITY_NAMESPACE="$OPERATOR_NAMESPACE"'
    if required not in block:
        raise SystemExit(f"{phase} does not pass the run-owned Grid identity namespace")
print("Kind Gateway phases pass the run-owned Grid identity namespace")
PY
