#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIFECYCLE="$SCRIPT_DIR/run-kind-lifecycle.sh"
WORKFLOW="$SCRIPT_DIR/../../.github/workflows/kind-native-grid-llmd-e2e.yaml"

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

python3 - "$LIFECYCLE" "$WORKFLOW" <<'PY'
import sys
from pathlib import Path

lifecycle = Path(sys.argv[1]).read_text(encoding="utf-8")
workflow = Path(sys.argv[2]).read_text(encoding="utf-8")

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
    'natural-cleanup',
    'diskConsumedSinceFirstSampleBytes',
    'cgroupCpuUsageUsec',
):
    if required not in lifecycle:
        raise SystemExit(f"lifecycle driver is missing required behavior: {required}")

if 'docker system prune' in lifecycle or 'finalizers=' in lifecycle:
    raise SystemExit("lifecycle must not prune shared Docker state or alter finalizers")

for required in (
    'workflow_dispatch:',
    'runs-on: ubuntu-24.04',
    'KIND_CAPACITY_PROFILE: gha-standard-4cpu',
    'export PATH="$tool_dir:$HOME/.local/bin:$PATH"',
    'timeout --signal=TERM',
    'if: always()',
    'actions/upload-artifact@cf430e030ddbb5b0abf93d22962f4752f3646cd9',
    'Remove private state after artifact upload',
    'apt-get install -y podman skopeo jq iproute2 ripgrep shellcheck python3-pip',
    'dpkg-query -W -f=\'${binary:Package}=${Version}\\n\' podman skopeo jq iproute2 ripgrep shellcheck python3-pip',
):
    if required not in workflow:
        raise SystemExit(f"workflow contract is missing: {required}")

if 'ACTIVE_PHASE="$name"' not in lifecycle or lifecycle.index('ACTIVE_PHASE="$name"') < lifecycle.index('if ((rc != 0)); then'):
    raise SystemExit("lifecycle failure records must preserve the failed phase name")

print("Kind lifecycle orchestration and workflow contract passed")
PY

printf 'Kind lifecycle contract passed\n'
