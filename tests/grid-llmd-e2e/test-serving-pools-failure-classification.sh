#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'api_server="$(timeout 10s oc --context="$KUBE_CONTEXT" whoami --show-server 2>&1 || true)"',
    'oc --context="$KUBE_CONTEXT" get --raw=/readyz',
    '*Unauthorized*|*"You must be logged in"*',
    'classification=\'platform/capacity (API endpoint unavailable)\'',
    'classification=\'authentication/environment\'',
    "simulator-build|simulator-image-provenance) classification=\'artifact/provenance or deployment automation\' ;;",
    'api-readyz-on-failure.txt',
    "classification:$classification",
)
missing = [fragment for fragment in required if fragment not in source]
forbidden = ("grep -REiq --include='*.txt' --include='*.log'",)
present = [fragment for fragment in forbidden if fragment in source]
if missing or present:
    print(
        "serving pool failure classification must use a current API identity/readiness snapshot "
        f"and avoid historical evidence scans; missing={missing!r}, forbidden={present!r}",
        file=sys.stderr,
    )
    raise SystemExit(1)

print("serving pool current API/readiness failure classification and report field: PASS")
PY
