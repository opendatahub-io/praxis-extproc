#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/cold-deploy.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
start = source.index("apply_monitoringstack_resource_profile()")
end = source.index("apply_application()", start)
function = source[start:end]
required = (
    'local profile="$1"',
    'deadline=$((SECONDS + PLATFORM_TIMEOUT_SECONDS))',
    'oc get crd monitoringstacks.monitoring.rhobs -o json',
    '.type=="Established" and .status=="True"',
    'timed out waiting for the MonitoringStack CRD to become Established',
    'waiting for the operator-owned MonitoringStack',
    'write_monitoringstack_profile_patch "$patch_file"',
    '--type=merge --dry-run=server --patch-file "$patch_file"',
    '--type=merge --patch-file "$patch_file"',
)
missing = [fragment for fragment in required if fragment not in function]
if missing:
    print("MonitoringStack profile wait contract is missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

crd_wait = function.index('oc get crd monitoringstacks.monitoring.rhobs -o json')
resource_wait = function.index('get monitoringstack data-science-monitoringstack')
dry_run = function.index('--type=merge --dry-run=server --patch-file "$patch_file"')
apply = function.index('--type=merge --patch-file "$patch_file"')
if not crd_wait < resource_wait < dry_run < apply:
    print("the CRD and operator-owned resource must be established before dry-run/apply", file=sys.stderr)
    raise SystemExit(1)

print("MonitoringStack CRD wait precedes owner check, server dry-run, and profile apply: PASS")
PY
