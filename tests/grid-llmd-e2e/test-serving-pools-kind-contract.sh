#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
checks = (
    'CLUSTER_PLATFORM="$(env_default GRID_LLMD_CLUSTER_PLATFORM openshift)"',
    'if [[ "$CLUSTER_PLATFORM" == kind ]]; then',
    'verify_kind_platform "$dir"',
    'if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf not-used; else printf 1106cd57746be7be9a37805d9f48e48518cc31b9; fi',
    'if [[ "$CLUSTER_PLATFORM" != kind ]]; then',
    'kserve/llmisvc-controller-manager maas-system/maas-controller maas-system/maas-api',
    'maas-system/postgres maas-system/payload-pre-processing maas-system/payload-processing',
    'serving.kserve.io",version:"v1alpha2"',
    'maas-controller-lease-rbac-can-i.txt',
    'verify_kind_capacity',
    'if [[ "$MODE" == platform ]]; then',
    'verify_capacity_mode',
    'verify_platform',
    'Platform-only readiness gate passed; task namespace and serving-pool graph are not required at this phase',
    'unsupported mode: %s',
    '^(platform|build|apply|verify|collect|cleanup)$',
    'WAIVED_FOR_KIND_DEV_SMOKE',
    'task_owned_count="$(jq --arg ns "$NAMESPACE" --arg run "$RUN_LABEL"',
    'Found $task_owned_count LLMInferenceServices owned by this task/run',
    'KSERVE_ROUTE_CONTROLLER_NAME',
    'controllerName==$controllerName',
    'kserve-config-llm-scheduler',
    'SCHEDULER_CONFIG_NAMESPACE',
    'MAAS_TENANT_CONFIG_NAMESPACE',
    'then printf models-as-a-service; else printf redhat-ods-applications',
    'LLMISVC_GATEWAY_NAMESPACE="$(env_default LLMISVC_GATEWAY_NAMESPACE "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf maas-system',
    'kind-serving-transport.json',
    'clusterWideMutation:false',
    'no DataScienceCluster exists in the upstream Kind topology',
    'SIMULATOR_IMAGE_PROVENANCE_FILE',
    'EXTPROC_IMAGE="$(env_default EXTPROC_IMAGE \'\')"',
    'EXTPROC_IMAGE must use an immutable sha256 digest when supplied',
    'image pin supplied but not deployed by this serving-pool helper',
    'not part of the serving-pool phase; source lock revision for later deployment',
    'Not deployed by this helper; build and verify the qualification image before Gateway traffic',
    'image-provenance-llm-d-sim.json',
    '.build_platform=="linux/amd64"',
    'shared Docker/Podman builder verifies registry digest',
    'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev or the explicit Kind platform',
    '--arg source "$SCHEDULER_CONFIG_NAME" --arg clusterPlatform "$CLUSTER_PLATFORM"',
    'if $clusterPlatform=="kind" then',
    '"--secure-serving=false"',
)
missing = [item for item in checks if item not in source]
if missing:
    print("Kind serving path contract missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

platform_gate = source.split('if [[ "$MODE" == platform ]]; then', 1)[1].split('\nfi\n', 1)[0]
namespace_check = source.index('verify_namespace\n', source.index('api_access\nwrite_invocation'))
platform_check = source.index('if [[ "$MODE" == platform ]]; then', source.index('api_access\nwrite_invocation'))
if not platform_check < namespace_check or 'verify_all' in platform_gate:
    print("platform-only mode must verify dependencies without requiring a task namespace or serving pools", file=sys.stderr)
    raise SystemExit(1)

configure = source.split('configure_llmisvc_transport() {', 1)[1]
kind_transport = configure.split('if [[ "$CLUSTER_PLATFORM" == kind ]]; then', 1)[1].split('\n  capture_llmisvc_transport_state', 1)[0]
for forbidden in ('patch datasciencecluster', 'capture_llmisvc_transport_state', 'read_effective_llmisvc_tls'):
    if forbidden in kind_transport:
        print(f"Kind transport path must not mutate/read DSCI state: {forbidden}", file=sys.stderr)
        raise SystemExit(1)

scheduler = source.split('make_task_scheduler_config() {', 1)[1].split('\n}\n', 1)[0]
if 'if $clusterPlatform=="kind" then' not in scheduler or '"--secure-serving=false"' not in scheduler:
    print("Kind-only EPP h2c setting is missing from the task-owned scheduler config", file=sys.stderr)
    raise SystemExit(1)
if 'oc apply --server-side --dry-run=server' not in scheduler or 'oc apply --server-side --field-manager=grid-llmd-e2e' in scheduler:
    print("scheduler renderer must keep apply in the ordered task-owned phase", file=sys.stderr)
    raise SystemExit(1)

print("Kind serving platform, API, TLS, image and route-controller contracts: PASS")
PY
