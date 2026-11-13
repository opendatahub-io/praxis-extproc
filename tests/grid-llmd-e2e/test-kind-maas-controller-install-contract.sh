#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-maas-controller.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
steps = [
    '--kubeconfig) KUBECONFIG_FILE=',
    'render-kind-maas-controller.py',
    'apply --dry-run=server',
    'printf \'%s\\n\' "$dry_run_status" >"$EVIDENCE_DIR/server-dry-run-exit-status.txt"',
    'if ((dry_run_status != 0))',
    'if [[ "$APPLY" == true ]]',
    'apply -f "$EVIDENCE_DIR/maas-controller-rendered.yaml"',
    'status.observedGeneration==.metadata.generation',
    'auth can-i "$verb" leases.coordination.k8s.io',
    'Successfully acquired lease',
    'verify_kind_maas_controller_lease.py',
]
positions = [source.find(step) for step in steps]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("MaaS Kind install must render, dry-run, apply, then verify RBAC and leader election")
if "patch rolebinding" in source or "kubectl patch" in source:
    raise SystemExit("MaaS Kind install must use the corrected render instead of a live RBAC patch")
if source.find('exit "$dry_run_status"') > positions[5]:
    raise SystemExit("MaaS Kind install must stop before applying a render that failed server-side dry-run")
if 'expected_holder="${pod_name}_${pod_uid}"' in source:
    raise SystemExit("MaaS Kind install must not equate controller-runtime Lease identity suffix with the Pod UID")
if 'MONITORING_NAMESPACE=""' not in source or '--monitoring-namespace "$MONITORING_NAMESPACE"' not in source:
    raise SystemExit("MaaS Kind install must explicitly pass its supported optional-observability profile")
print("MaaS Kind install render, dry-run, apply, and leader-election gate contract: PASS")
PY
