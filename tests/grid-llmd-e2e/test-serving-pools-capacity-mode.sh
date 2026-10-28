#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" "$SCRIPT_DIR/cold-deploy.sh" "$SCRIPT_DIR/deploy-grid-operator.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
cold = Path(sys.argv[2]).read_text(encoding="utf-8")
grid_operator = Path(sys.argv[3]).read_text(encoding="utf-8")
required = (
    'RESOURCE_PROFILE="$(env_default RESOURCE_PROFILE strict)"',
    'RESOURCE_PROFILE must be strict or reduced-dev',
    'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev',
    'local args=(--mode preflight --resource-profile "$RESOURCE_PROFILE" --expected-api-server "$EXPECTED_API_SERVER")',
    'expected_status=preflight-pass-reduced-dev',
    'expected_baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE',
    'COLD_DEPLOY_MODE=preflight COLD_DEPLOY_EVIDENCE_DIR="$dir"',
    "'.status==$status and .capacityBaseline==$baseline and .resourceProfile==$profile'",
    'if [[ "$MODE" == build || "$MODE" == apply ]]; then verify_capacity_mode; fi',
)
missing = [fragment for fragment in required if fragment not in source]
if missing:
    print("serving pool capacity-mode contract is missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

preflight = source.index('if [[ "$MODE" == build || "$MODE" == apply ]]; then verify_capacity_mode; fi')
dispatch = source.index('case "$MODE" in\n  collect)')
if preflight > dispatch:
    print("capacity preflight must run before the build/apply dispatch", file=sys.stderr)
    raise SystemExit(1)

cold_required = (
    'RESOURCE_PROFILE="$(env_default RESOURCE_PROFILE strict)"',
    'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev',
    'PROMETHEUS_CPU_REQUEST=100m',
    'THANOS_CPU_REQUEST=50m',
    'PROMETHEUS_CPU_REQUEST=50m',
    'THANOS_CPU_REQUEST=10m',
    'downstreamGridOperatorCpuRequest:$gridOperator',
    'capacity-baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE',
    'strict-resource-profile-render.yaml',
    'reduced-dev-resource-profile-render.yaml',
    'monitoringstack-strict-render.yaml',
    'monitoringstack-reduced-dev-render.yaml',
    'render-monitoringstack-profile.sh',
    'CHART_PROFILE=maas',
    'apply_monitoringstack_resource_profile reduced-dev',
    '--type=merge --patch-file "$patch_file"',
    'write_monitoringstack_profile_patch "$patch_file"',
    'validate_monitoringstack_server_dry_run',
    'git -C "$MAAS_SOURCE_DIR" rev-parse --is-inside-work-tree',
    'MAAS_SOURCE_REVISION does not match the clean source worktree HEAD',
    'MaaS source worktree must be detached at the recorded SHA',
    'MaaS source worktree must be clean before running the pinned database helper',
)
missing_cold = [fragment for fragment in cold_required if fragment not in cold]
if missing_cold:
    print("cold-deploy resource-profile contract is missing: " + repr(missing_cold), file=sys.stderr)
    raise SystemExit(1)
apply_case = cold.index('  apply-platform)\n')
render_before_gate = cold.find('    render_platform_resource_profile', apply_case)
gate_after_render = cold.find('    capacity_gate', apply_case)
install_after_gate = cold.find('    install_operator', apply_case)
if min(render_before_gate, gate_after_render, install_after_gate) < 0 or not (render_before_gate < gate_after_render < install_after_gate):
    print("reduced-dev render/source validation must precede capacity gating and operator installation", file=sys.stderr)
    raise SystemExit(1)

rollback_start = cold.index('  rollback-resource-profile)')
rollback_end = cold.index('  configure-maas-db)', rollback_start)
rollback = cold[rollback_start:rollback_end]
rollback_steps = (
    '[[ "$RESOURCE_PROFILE" == strict ]] ||',
    'validate_gitops_source',
    'apply_application',
    'verify_monitoring_profile_survives_reconciliation',
)
rollback_positions = [rollback.find(step) for step in rollback_steps]
if any(position < 0 for position in rollback_positions) or rollback_positions != sorted(rollback_positions):
    print("rollback must validate and apply the strict profile, then verify reconciliation", file=sys.stderr)
    raise SystemExit(1)
if 'oc delete' in rollback:
    print("resource-profile rollback must retain the task Argo Application and shared platform resources", file=sys.stderr)
    raise SystemExit(1)
if 'profile=reduced-dev' in cold or 'profile=$RESOURCE_PROFILE' in cold:
    print("main GitOps source must not receive the unsupported reduced-dev chart profile", file=sys.stderr)
    raise SystemExit(1)
if '[[ -d "$MAAS_SOURCE_DIR/.git" ]]' in cold:
    print("MaaS source validation must support linked Git worktrees whose .git is a file", file=sys.stderr)
    raise SystemExit(1)
if 'statefulset' in cold[cold.index('apply_monitoringstack_resource_profile()'):cold.index('apply_application()')].lower():
    print("the persistent resource reduction must target the MonitoringStack API, not a StatefulSet patch", file=sys.stderr)
    raise SystemExit(1)

grid_required = (
    'RESOURCE_PROFILE="${RESOURCE_PROFILE:-strict}"',
    'strict) PROFILE_CPU_REQUEST=50m ;;',
    'PROFILE_CPU_REQUEST=25m',
    'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev',
)
missing_grid = [fragment for fragment in grid_required if fragment not in grid_operator]
if missing_grid:
    print("Grid operator resource-profile contract is missing: " + repr(missing_grid), file=sys.stderr)
    raise SystemExit(1)

print("strict-default, explicit reduced-dev selection/order, and strict rollback contract: PASS")
PY
