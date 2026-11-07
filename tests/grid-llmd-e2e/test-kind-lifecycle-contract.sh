#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIFECYCLE="$SCRIPT_DIR/run-kind-lifecycle.sh"
WORKFLOW="$SCRIPT_DIR/../../.github/workflows/kind-native-grid-llmd-e2e.yaml"
BUILDER="$SCRIPT_DIR/../shared/build_and_push.sh"

bash -n "$LIFECYCLE" "$SCRIPT_DIR/destroy-kind-environment.sh"
bash "$LIFECYCLE" --help >/dev/null

set +e
KIND_CAPACITY_PROFILE=full KIND_PRIVATE_STATE_DIR="/nonexistent-kind-e2e-state" \
  bash "$LIFECYCLE" --mode cleanup >/dev/null 2>&1
profile_status=$?
set -e
[[ "$profile_status" == 2 ]] || {
  printf 'lifecycle driver must reject a non-four-CPU workflow profile before cleanup\n' >&2
  exit 1
}

python3 - "$LIFECYCLE" "$WORKFLOW" "$BUILDER" <<'PY'
import sys
import os
import subprocess
import tempfile
from pathlib import Path

lifecycle = Path(sys.argv[1]).read_text(encoding="utf-8")
workflow = Path(sys.argv[2]).read_text(encoding="utf-8")
builder = Path(sys.argv[3]).read_text(encoding="utf-8")

ordered_phases = (
    'run_phase kind-cluster-and-registry',
    'run_phase kind-images-build',
    'run_phase base-dependencies',
    'run_phase kserve-controller',
    'run_phase maas-database',
    'run_phase pool-gateways',
    'run_phase serving-pool-a',
    'run_pool_setup_probe pool-a',
    'run_phase serving-pool-b',
    'run_pool_setup_probe pool-b',
    'run_phase grid-operator-bootstrap',
    'run_phase grid-provider-registration',
    'run_phase client-identity',
    'run_phase provider-proxies',
    'run_phase grid-operator-praxis-config',
    'run_phase extproc-filter',
    'run_phase native-routing-qualification',
    'phase_start natural-cleanup',
    'run-kind-lifecycle.sh" --mode cleanup',
)
positions = [
    lifecycle.rfind(phase) if phase == 'phase_start natural-cleanup' else lifecycle.find(phase)
    for phase in ordered_phases
]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("lifecycle phases are missing or out of dependency order")

cleanup_start = lifecycle.index('if [[ "$MODE" == cleanup ]]')
cleanup_end = lifecycle.index('export SOURCE_MODE=main', cleanup_start)
cleanup = lifecycle[cleanup_start:cleanup_end]
cleanup_order = (
    'deploy-grid-providers.sh',
    'cleanup-kind-serving.sh',
    'cleanup-grid-operator.sh',
    'cleanup-kind-task-namespace.sh',
    'destroy-kind-environment.sh',
)
cleanup_positions = [cleanup.find(component) for component in cleanup_order]
if any(position < 0 for position in cleanup_positions) or cleanup_positions != sorted(cleanup_positions):
    raise SystemExit("natural cleanup order is incomplete or unsafe")

for required in (
    'KIND_CAPACITY_PROFILE',
    'mkdir -p "$RUN_DIR" "$STATE_DIR" "$SOURCE_ROOT" "$RUN_DIR/logs" "$RUN_DIR/phase-metrics"',
    'KIND_STATE_DIR="$STATE_DIR/kind"',
    'capture_diagnostics',
    'sanitize_diagnostics.py',
    'qualification_rc',
    'GRID_LLMD_AUTH_TOKEN',
    'KIND_POOL_SETUP_PROBE_API_CA_FILE="$STATE_DIR/maas-api-root-ca.crt"',
    'natural-cleanup',
    'diskConsumedSinceFirstSampleBytes',
    'cgroupCpuUsageUsec',
):
    if required not in lifecycle:
        raise SystemExit(f"lifecycle driver is missing required behavior: {required}")

if 'docker system prune' in lifecycle or 'finalizers=' in lifecycle:
    raise SystemExit("lifecycle must not prune shared Docker state or alter finalizers")

if 'disable-task-gateway-retries.sh' in lifecycle:
    raise SystemExit("lifecycle must verify per-request upstream/retry counters instead of patching the generated Gateway route")

for required in (
    'workflow_dispatch:',
    'runs-on: ubuntu-24.04',
    'KIND_CAPACITY_PROFILE: gha-standard-4cpu',
    'export PATH="$tool_dir:$HOME/.local/bin:$PATH"',
    'timeout --signal=TERM',
    'if: always()',
    'actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9',
    'Remove private state after artifact upload',
    'workflow-cleanup-retry.log',
    'KIND_CLEANUP_CONTEXT=workflow-retry KIND_PRIVATE_STATE_DIR="$state_dir"',
    'python3 tests/grid-llmd-e2e/sanitize_diagnostics.py',
    'apt-get install -y podman skopeo jq iproute2 ripgrep shellcheck python3-pip',
    'dpkg-query -W -f=\'${binary:Package}=${Version}\\n\' podman skopeo jq iproute2 ripgrep shellcheck python3-pip',
):
    if required not in workflow:
        raise SystemExit(f"workflow contract is missing: {required}")

if 'ACTIVE_PHASE="$name"' not in lifecycle or lifecycle.index('ACTIVE_PHASE="$name"') < lifecycle.index('if ((rc != 0)); then'):
    raise SystemExit("lifecycle failure records must preserve the failed phase name")
if 'record_unclosed_failure_phase "$rc"' not in lifecycle:
    raise SystemExit("direct lifecycle command failures must be appended to PHASES.jsonl before diagnostics capture")
if 'AIGC_CONTROLLER_KUSTOMIZATION="$AIGC_SOURCE/config/self/default"' not in lifecycle:
    raise SystemExit("AIGC bootstrap must use the supported config/self/default kustomization from current main")
if 'kustomize build "$AIGC_SOURCE/config/default"' in lifecycle:
    raise SystemExit("AIGC bootstrap must not use the removed config/default path")
platform_readiness = lifecycle.split('SERVING_POOLS_MODE=platform', 1)
if len(platform_readiness) != 2 or 'SERVING_POOLS_MODE=verify SERVING_POOL_SELECTION=both SERVING_POOLS_EVIDENCE_DIR="$RUN_DIR/platform/platform-gate"' in lifecycle:
    raise SystemExit("platform readiness must use platform-only verification before creating the task namespace")
if 'KIND_CLEANUP_CONTEXT=qualification KIND_PRIVATE_STATE_DIR="$STATE_DIR"' not in lifecycle:
    raise SystemExit("in-run cleanup must explicitly be recorded as qualification natural cleanup")
if 'OPERATOR_NAMESPACE="$TASK_NAMESPACE"' not in lifecycle:
    raise SystemExit("Kind Grid operator and its GridNetwork/Gateway must share the run-owned namespace required by current TLS ownership contracts")
bootstrap_gateway = lifecycle.split('phase_start task-gateway-bootstrap', 1)[1].split('phase_end 0', 1)[0]
if ('GRID_GATEWAY_MOUNT_RECONCILIATION_ENABLED=true' not in bootstrap_gateway or
        'GRID_GATEWAY_RELEASE_HELM_MOUNTS=true' not in bootstrap_gateway):
    raise SystemExit("bootstrap Gateway must opt into Grid mount reconciliation before the operator configuration handoff")
if ('GRID_GATEWAY_PRAXIS_CONFIG_MAP_NAME=grid-llmd-praxis-config' not in bootstrap_gateway or
        'GRID_PRAXIS_CONFIG_MAP_NAME=grid-llmd-praxis-config' not in lifecycle or
        lifecycle.count('GRID_GATEWAY_PRAXIS_CONFIG_MAP_NAME=grid-llmd-praxis-config') != 2):
    raise SystemExit("bootstrap, generated Praxis config, and final Gateway must use the same ConfigMap source")
for phase in ('phase_start task-gateway-bootstrap', 'phase_start task-gateway-grid-config'):
    phase = lifecycle.split(phase, 1)[1].split('phase_end 0', 1)[0]
    if ('GRID_GATEWAY_TLS_SECRET_NAME=grid-gateway-site-identity' not in phase or
            'GRID_GATEWAY_CA_SECRET_NAME=grid-gateway-ca' not in phase):
        raise SystemExit("co-located Kind Grid/Gateway must use separate run-owned TLS and CA copies")
if 'naturalCleanup:$cleanupStatus' in lifecycle:
    raise SystemExit("final result must not encode the cleanup status string as the natural-cleanup value")
if 'naturalCleanup:$naturalCleanup' not in lifecycle:
    raise SystemExit("final result must record natural cleanup as a verified boolean")
cleanup_start = lifecycle.index('if [[ "$MODE" == cleanup ]]')
cleanup_end = lifecycle.index('export SOURCE_MODE=main', cleanup_start)
cleanup = lifecycle[cleanup_start:cleanup_end]
if "'{status:\"PASS\",runId:$run,cluster:$cluster,naturalCleanup:true}'" in cleanup:
    raise SystemExit("cleanup must not claim natural finalization unconditionally")
if 'gridNetworkWithdrawalCompleted:$gridNetworkWithdrawalCompleted' not in cleanup or 'taskNamespaceRemoved:$taskNamespaceRemoved' not in cleanup:
    raise SystemExit("cleanup result must carry GridNetwork withdrawal and task-namespace removal evidence")
if 'phase_start workflow-cleanup-retry' not in cleanup or 'if [[ "$CLEANUP_CONTEXT" == qualification ]]' not in cleanup:
    raise SystemExit("failure cleanup retry must be recorded separately from natural qualification cleanup")
aigc_start = lifecycle.index('phase_start aigc-controller-bootstrap')
aigc_kustomize = lifecycle.index('kustomize build "$AIGC_CONTROLLER_KUSTOMIZATION"')
aigc_end = lifecycle.index('phase_end 0', aigc_kustomize)
if not aigc_start < aigc_kustomize < aigc_end:
    raise SystemExit("AIGC source rendering and install must be inside a separately reported phase")
phase_end_function = lifecycle.split('phase_end() {', 1)[1].split('record_unclosed_failure_phase()', 1)[0]
if 'ACTIVE_PHASE=""' not in phase_end_function or 'PHASE_START=""' not in phase_end_function:
    raise SystemExit("completed lifecycle phases must clear their active state")
phase_functions = lifecycle[lifecycle.index('phase_log() {'):lifecycle.index('run_phase() {')]
with tempfile.TemporaryDirectory(prefix="kind-phase-contract-") as temp_dir:
    env = os.environ.copy()
    env["RUN_DIR"] = temp_dir
    phase_harness = f'''set -Eeuo pipefail
ACTIVE_PHASE=""
PHASE_START=""
{phase_functions}
mkdir -p "$RUN_DIR"
: >"$RUN_DIR/RUN-LOG.md"
: >"$RUN_DIR/PHASES.jsonl"
phase_start completed-phase
phase_end 0
[[ -z "$ACTIVE_PHASE" && -z "$PHASE_START" ]]
phase_start direct-command-failure
record_unclosed_failure_phase 9
[[ "$(jq -s 'length' "$RUN_DIR/PHASES.jsonl")" == 2 ]]
jq -es '.[1].phase=="direct-command-failure" and .[1].status=="FAIL" and .[1].exitCode==9' "$RUN_DIR/PHASES.jsonl" >/dev/null
phase_start dispatched-command-failure
phase_end 7 || true
ACTIVE_PHASE=dispatched-command-failure
record_unclosed_failure_phase 7
[[ "$(jq -s 'length' "$RUN_DIR/PHASES.jsonl")" == 3 ]]
'''
    subprocess.run(["bash"], input=phase_harness, text=True, env=env, check=True)
if 'if [[ "${cmd[0]}" == "$SCRIPT_DIR/"*.sh ]]; then' not in lifecycle or 'cmd=(bash "${cmd[@]}")' not in lifecycle:
    raise SystemExit("lifecycle dispatcher must invoke repository shell helpers through Bash regardless of file mode")
if 'originalLifecycleResultPreserved:true' not in lifecycle or 'cluster or kubeconfig unavailable before application cleanup' not in lifecycle or 'ACTIVE_PHASE=cleanup-retry' not in lifecycle:
    raise SystemExit("failure cleanup must preserve the primary failure and state whether natural cleanup was possible")
if 'stage:$stage' not in lifecycle or 'CLEANUP_STAGE=kind-registry-network-destroy' not in lifecycle or 'Cleanup retry failed at stage %s' not in lifecycle:
    raise SystemExit("cleanup retry failures must record a sanitized, actionable failure stage")
if '[[ -n "$chart_file" && -f "$chart_file" ]] || return 0' not in lifecycle or 'GRID_OPERATOR_CHART_SHA256="$(chart_sha256 "$GRID_OPERATOR_CHART_FILE")"' not in lifecycle or 'PRAXIS_GATEWAY_CHART_SHA256="$(chart_sha256 "$PRAXIS_GATEWAY_CHART_FILE")"' not in lifecycle:
    raise SystemExit("cleanup retry must tolerate absent optional charts without tripping the ERR trap")
if '[[ -f "$RUN_DIR/platform/task-namespace/RESULT.json" && -f "$RUN_DIR/grid/operator-bootstrap/RESULT.json" ]]' not in cleanup:
    raise SystemExit("full task cleanup must run only after Grid operator installation, when Grid CRDs and natural withdrawal can be verified")
if 'pre-grid-task-namespace-not-applicable' not in cleanup or 'naturalGridWithdrawalVerified:false' not in cleanup:
    raise SystemExit("pre-Grid failure cleanup must record task namespace removal by cluster teardown without claiming natural Grid cleanup")
if lifecycle.count('GRID_GATEWAY_MODE=install PRAXIS_GATEWAY_CHART_FILE="$PRAXIS_GATEWAY_CHART_FILE"') != 2 or lifecycle.count('PRAXIS_GATEWAY_CHART_SHA256="$PRAXIS_GATEWAY_CHART_SHA256"') != 2:
    raise SystemExit("both task Gateway installs must pass the chart archive and digest under the helper's PRAXIS_GATEWAY_* inputs")
if 'task_cleanup_mode=fresh' not in cleanup or 'if [[ "$CLEANUP_CONTEXT" == workflow-retry ]]; then' not in cleanup or 'task_cleanup_mode=resume' not in cleanup or 'KIND_TASK_CLEANUP_MODE="$task_cleanup_mode"' not in cleanup:
    raise SystemExit("workflow failure cleanup must use resume mode so an uninstalled task Gateway is safely inventoried and cleaned")
if 'GRID_SOURCE_REVISION="$(source_value grid)"' not in lifecycle or 'export GRID_SOURCE_REVISION GRID_GATEWAY_SOURCE_REVISION' not in lifecycle:
    raise SystemExit("provider registration must receive the exact Grid SHA from this run's source lock")
if 'GRID_OPERATOR_IMAGE="$(image_ref grid-operator)"' not in lifecycle or not any(
    line.startswith("export ") and "GRID_OPERATOR_SOURCE_REVISION" in line and "GRID_OPERATOR_IMAGE" in line
    for line in lifecycle.splitlines()
):
    raise SystemExit("provider registration must receive the exact immutable Grid operator image from this run's image lock")
if ('export GRID_LLMD_GRID_OPERATOR_DEPLOYMENT="$OPERATOR_NAMESPACE/grid-operator"' not in lifecycle or
        'export GRID_LLMD_GRID_OPERATOR_DEPLOYMENT="$OPERATOR_NAMESPACE/deployment/grid-operator"' in lifecycle):
    raise SystemExit("qualification must pass the downstream Grid operator as namespace/name, not namespace/deployment/name")
qualification = lifecycle.split('phase_start native-routing-qualification', 1)[1].split('phase_end 0', 1)[0]
for required_input in (
    'export GRID_LLMD_GRID_SITE_RESOURCE="$TASK_NAMESPACE/gridsite/$GRID_SITE"',
    'export GRID_LLMD_SITE_IDENTITY_SECRET="$TASK_NAMESPACE/grid-site-identity"',
    'export GRID_LLMD_GRID_CA_SECRET="$TASK_NAMESPACE/grid-ca"',
    'export GRID_LLMD_EXPECTED_SITE_SAN="$SITE_HOSTNAME"',
    'export GRID_LLMD_IPP_STATE=absent',
    'export GRID_LLMD_DEPLOYMENTS="maas-system/maas-controller"',
):
    if required_input not in qualification:
        raise SystemExit(f"qualification is missing a required evidence input: {required_input}")
if ('GRID_LLMD_RUNTIME_IMAGE_DIGESTS="praxis-ai-grid-operator=$(runtime_image_digest grid-operator)' not in qualification or
        'GRID_LLMD_RUNTIME_IMAGE_DIGESTS' not in qualification.split('export GRID_LLMD_SOURCE_REVISIONS', 1)[1].split('\n', 1)[0]):
    raise SystemExit("qualification must pass an immutable runtime manifest digest for every image pin")

runtime_digest_body = lifecycle.split('runtime_image_digest() {', 1)[1].split('\n}', 1)[0]
with tempfile.TemporaryDirectory(prefix="kind-runtime-image-digest-") as temp_dir:
    image_dir = Path(temp_dir) / "images" / "grid-gateway"
    image_dir.mkdir(parents=True)
    (image_dir / "image-ref.txt").write_text("registry.invalid/grid-gateway@sha256:" + "a" * 64 + "\n", encoding="utf-8")
    digest_harness = f'''set -Eeuo pipefail
RUN_DIR={str(Path(temp_dir))!r}
image_ref() {{ cat "$RUN_DIR/images/$1/image-ref.txt"; }}
runtime_image_digest() {{{runtime_digest_body}
}}
[[ "$(runtime_image_digest grid-gateway)" == "sha256:{'a' * 64}" ]]
printf 'registry.invalid/grid-gateway:latest\\n' >"$RUN_DIR/images/grid-gateway/image-ref.txt"
if runtime_image_digest grid-gateway >/dev/null 2>&1; then exit 9; fi
'''
    subprocess.run(["bash"], input=digest_harness, text=True, check=True)
if 'serving_cleanup_mode=fresh' not in cleanup or 'serving_cleanup_mode=resume' not in cleanup or 'KIND_SERVING_CLEANUP_MODE="$serving_cleanup_mode"' not in cleanup:
    raise SystemExit("workflow failure cleanup must resume partial serving cleanup instead of requiring both pools")

source_validation = builder.index('[[ "$actual_head" == "$SOURCE_SHA" && -z "$actual_branch" ]]')
permission_fix = builder.index('make_build_context_readable "$BUILD_CONTEXT_DIR"')
container_build = builder.index('"$CONTAINER_ENGINE" "${build_args[@]}" >"$EVIDENCE_DIR/build-$COMPONENT.log" 2>&1')
if not source_validation < permission_fix < container_build:
    raise SystemExit("build-context readability must be restored only after source validation and before image build")
if 'find "$context_dir" -name .git -prune -o -type d -exec chmod a+rx {} +' not in builder or 'find "$context_dir" -name .git -prune -o -type f -exec chmod a+r {} +' not in builder:
    raise SystemExit("build-context readability must include directories and files while excluding Git metadata")

print("Kind lifecycle orchestration and workflow contract passed")
PY

printf 'Kind lifecycle contract passed\n'
