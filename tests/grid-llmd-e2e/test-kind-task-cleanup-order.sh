#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/cleanup-kind-task-namespace.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
grid_absence = source.find('for kind_name in "gridnetwork $NETWORK_NAME" "gridsite $SITE_NAME"')
provider_absence = source.find("inferenceproviders.grid.praxis.fast llminferenceservices.serving.kserve.io aitenants.maas.opendatahub.io", grid_absence)
gateway_uninstall = source.find('helm uninstall "$GATEWAY_RELEASE"', provider_absence)
route_delete = source.find('uid_precondition_delete "/apis/gateway.networking.k8s.io/v1/namespaces/$NAMESPACE/httproutes/', gateway_uninstall)
gateway_delete = source.find('uid_precondition_delete "/apis/gateway.networking.k8s.io/v1/namespaces/$NAMESPACE/gateways/', route_delete)
gateway_convergence = source.rfind("gateway-controller-resource-wait.jsonl", gateway_uninstall)
role_cleanup = source.find('for resource in clusterrole clusterrolebinding;', gateway_convergence)
namespace_delete = source.find('namespace-delete-options.json', role_cleanup)
namespace_wait = source.find('namespace-delete-wait.jsonl', namespace_delete)
positions = [grid_absence, provider_absence, gateway_uninstall, route_delete, gateway_delete, gateway_convergence, role_cleanup, namespace_delete, namespace_wait]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("task cleanup must follow Grid withdrawal and serving cleanup, then remove Gateway/runtime resources, before namespace deletion")

required = (
    "preconditions:{uid:$uid}",
    "praxis.fast/qualification-run",
    "KIND_TASK_CLEANUP_MODE must be plan, fresh, or resume",
    "already-absent",
    '"$MODE" == resume && "$GATEWAY_SNAPSHOT_COUNT" == 0 && "$TASK_ROUTE_COUNT" == 0',
    "LIVE_GATEWAY_COUNT == 0 && LIVE_TASK_ROUTE_COUNT == 0 && LIVE_GENERATED_COUNT == 0",
    'mode:"resume-no-gateway-installed"',
    '"$EVIDENCE_DIR/namespace-delete-options.json"',
    'dataKeys:(if .kind=="Secret" or .kind=="ConfigMap" then ((.data//{})|keys) else null end)',
    "naturalGridWithdrawalWasVerifiedBeforeCleanup:true",
)
missing = [fragment for fragment in required if fragment not in source]
if missing:
    raise SystemExit(f"cleanup ownership or evidence safeguards are missing: {missing}")

if any(fragment in source for fragment in ("oc patch", "kubectl patch", "remove-finalizer", "finalizers/remove")):
    raise SystemExit("task cleanup must not remove finalizers manually")

no_gateway = source.find('"$MODE" == resume && "$GATEWAY_SNAPSHOT_COUNT" == 0 && "$TASK_ROUTE_COUNT" == 0')
no_gateway_evidence = source.find('mode:"resume-no-gateway-installed"', no_gateway)
role_cleanup = source.find('for resource in clusterrole clusterrolebinding;', no_gateway_evidence)
if min(no_gateway, no_gateway_evidence, role_cleanup) < 0 or not no_gateway < no_gateway_evidence < role_cleanup:
    raise SystemExit("resume cleanup must prove the absent-Gateway state before continuing to namespace cleanup")

print("Task cleanup supports verified resume without an installed Gateway and UID-preconditions namespace deletion: PASS")
PY
