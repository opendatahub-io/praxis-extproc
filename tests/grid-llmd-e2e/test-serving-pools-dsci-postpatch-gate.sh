#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" "$SCRIPT_DIR/cold-deploy.sh" <<'PY'
from pathlib import Path
import json
import re
import subprocess
import sys

serving = Path(sys.argv[1]).read_text(encoding="utf-8")
cold = Path(sys.argv[2]).read_text(encoding="utf-8")

wait_start = serving.index("wait_for_kserve_tls_reconcile() {")
wait_end = serving.index("wait_for_dsc_field_observed() {", wait_start)
wait = serving[wait_start:wait_end]
if '"$controller_ready" == true' not in wait or '"$observed" -ge "$generation"' not in wait:
    raise SystemExit("DSCI transport must wait for current controller observation")
if 'type=="KserveReady"' in wait:
    raise SystemExit("DSCI field observation must not hide a failed KserveReady post-patch gate")

configure_start = serving.index("configure_llmisvc_transport() {")
configure_end = serving.index("verify_llmisvc_transport() {", configure_start)
configure = serving[configure_start:configure_end]
if "if ! wait_for_kserve_tls_reconcile; then" not in configure or "restore_llmisvc_transport" not in configure:
    raise SystemExit("DSCI controller-observation timeout must attempt original-field restoration")

gate_start = serving.index("verify_post_transport_platform_gate() {")
gate_end = serving.index("restore_llmisvc_transport() {", gate_start)
gate = serving[gate_start:gate_end]
required_gate = (
    'if verify_platform "$dir"',
    'type=="KserveReady" and .status=="True"',
    'kserve-webhook-server-service',
    'llmisvc-webhook-server-service',
    'odh-model-controller-webhook-service',
    '.ready>0 and .total>0',
    'post-reconciliation-pending-pods.json',
    'length==0',
    'if restore_llmisvc_transport; then',
)
missing = [fragment for fragment in required_gate if fragment not in gate]
if missing:
    raise SystemExit("post-DSCI gate is missing readiness or rollback requirements: " + repr(missing))

webhook_path_marker = '"$dir/webhook-endpoints.json" >/dev/null'
webhook_path_position = gate.find(webhook_path_marker)
webhook_jq_position = gate.rfind("jq -e '", 0, webhook_path_position)
if webhook_path_position < 0 or webhook_jq_position < 0:
    raise SystemExit("post-DSCI webhook gate expression could not be extracted for execution")
webhook_expression_start = webhook_jq_position + len("jq -e '")
webhook_expression_end = gate.rfind("'", webhook_expression_start, webhook_path_position)
webhook_program = gate[webhook_expression_start:webhook_expression_end].strip()
ready_slices = [
    {"service": "kserve-webhook-server-service", "ready": 1, "total": 1},
    {"service": "llmisvc-webhook-server-service", "ready": 1, "total": 1},
    {"service": "odh-model-controller-webhook-service", "ready": 1, "total": 1},
]

def run_webhook_gate(slices):
    return subprocess.run(
        ["jq", "-e", webhook_program],
        input=json.dumps(slices),
        text=True,
        capture_output=True,
        check=False,
    )

positive = run_webhook_gate(ready_slices)
if positive.returncode != 0 or positive.stdout.strip() != "true":
    raise SystemExit("post-DSCI webhook jq expression must pass all three current Ready services: " + positive.stderr)
unready = [dict(item) for item in ready_slices]
unready[1]["ready"] = 0
if run_webhook_gate(unready).returncode == 0:
    raise SystemExit("post-DSCI webhook gate accepted a service with no Ready endpoints")
missing_service = ready_slices[:-1]
if run_webhook_gate(missing_service).returncode == 0:
    raise SystemExit("post-DSCI webhook gate accepted a missing required webhook Service")

error_start = serving.index("on_error() {")
error_end = serving.index("trap 'on_error $?' ERR", error_start)
error_handler = serving[error_start:error_end]
if "grep -REiq" in error_handler or "events.txt" in error_handler:
    raise SystemExit("error classification must not treat historical scheduler/events logs as current API failures")
if 'oc --context="$KUBE_CONTEXT" whoami --show-server' not in error_handler:
    raise SystemExit("error capture must verify the selected API server before classifying availability")
if 'oc --context="$KUBE_CONTEXT" get --raw=/readyz' not in error_handler:
    raise SystemExit("error capture must use a bounded live /readyz probe for API availability")

apply_start = serving.index('  apply)\n', serving.index('case "$MODE" in\n  collect)'))
apply_end = serving.index('  verify)\n', apply_start)
apply = serving[apply_start:apply_end]
ordered = (
    "verify_platform",
    "configure_llmisvc_transport",
    "verify_post_transport_platform_gate",
    "verify_simulator_image_pin",
    "apply_one_pool \"$POOL_A\"",
)
positions = [apply.index(fragment) if fragment in apply else -1 for fragment in ordered]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("post-DSCI full gate must pass before any serving resource is applied")

webhook_capture = cold[cold.index('oc -n redhat-ods-applications get endpointslice'):cold.index('  : >"$gate_file"')]
for name in ("kserve-webhook-server-service", "llmisvc-webhook-server-service", "odh-model-controller-webhook-service"):
    if name not in webhook_capture:
        raise SystemExit(f"platform evidence does not capture current webhook endpoint slices for {name}")
if "group_by(.service)" not in webhook_capture:
    raise SystemExit("webhook evidence must group current EndpointSlices by the three required Services")
if 'length==3 and ([.[].service]|sort)==["kserve-webhook-server-service","llmisvc-webhook-server-service","odh-model-controller-webhook-service"]' not in cold:
    raise SystemExit("platform gate must require all three current webhook Services")

print("post-DSCI KserveReady/webhook/Pending gate, jq execution, safe restoration, and live error classification: PASS")
PY
