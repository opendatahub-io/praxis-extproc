#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/create-kind-task-namespace.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
steps = [
    'oc get --raw=/readyz',
    'oc get namespace "$NAMESPACE" -o json',
    'oc apply --dry-run=server',
    'oc create -f "$EVIDENCE_DIR/task-namespace.yaml" -o json',
    'oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace-after.json"',
]
positions = [source.find(step) for step in steps]
if any(pos < 0 for pos in positions) or positions != sorted(positions):
    raise SystemExit("task namespace helper must verify API and ownership before dry-run/create and final verification")
if source.find('.metadata.labels["praxis.fast/qualification-run"]==$run') > source.find('oc create -f "$EVIDENCE_DIR/task-namespace.yaml" -o json'):
    raise SystemExit("task namespace ownership must be checked before create")
if source.find("rg -qi 'notfound|not found'") > source.find('oc apply --dry-run=server'):
    raise SystemExit("namespace lookup must distinguish NotFound from API errors before create")
if 'oc label namespace' in source or 'oc delete namespace' in source:
    raise SystemExit("task namespace helper must not adopt, relabel, or delete a namespace")
print("Kind task namespace ownership/create contract: PASS")
PY
