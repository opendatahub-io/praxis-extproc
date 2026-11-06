#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/cleanup-kind-serving.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
script_dir = source.find('SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"')
helper_call = source.find('"$SCRIPT_DIR/prepare-kind-maas-cleanup.sh"')
prepare = source.find("prepare_maas_cleanup_if_needed\n", source.find("snapshot_owned llminferenceservices"))
tenant_a = source.find("delete_owned aitenant pool-a ai-tenants")
tenant_b = source.find("delete_owned aitenant pool-b ai-tenants")
restore = source.find("restore_maas_cleanup_if_needed\n", tenant_b)
ca_secret = source.find("delete_owned secret opendatahub-ca maas-system")
gateway_loop = source.find('for ref in \\\n  "$NAMESPACE/$POOL_A_GATEWAY/pool-a"')
gateway_delete = source.find('delete_owned gateway "$name" "$ns" "$pool"', gateway_loop)
gateway_wait = source.find('wait_gateway_services_absent "$ns" "$name"', gateway_delete)
gateway_snapshot = source.find("snapshot_owned_pool_gateways \"$ns\" \"$gateway_snapshot\"", gateway_wait)
gateway_absence = source.find("jq -e 'length==0' \"$gateway_snapshot\"", gateway_snapshot)
positions = [script_dir, helper_call, prepare, tenant_a, tenant_b, restore, ca_secret, gateway_loop, gateway_delete, gateway_wait, gateway_absence]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("Kind MaaS helper path initialization, preparation, natural AITenant finalization, restoration, and CA cleanup must precede pool Gateway deletion")

if 'run_maas_cleanup_helper prepare "$EVIDENCE_DIR/maas-cleanup-prepare"' not in source:
    raise SystemExit("Kind cleanup must prepare the run-owned MaaS cleanup image before deleting AITenants")
if 'run_maas_cleanup_helper restore "$EVIDENCE_DIR/maas-cleanup-restore"' not in source:
    raise SystemExit("Kind cleanup must restore MaaS only after both AITenant finalizers complete")
if '"$CLUSTER_ENV" == kind' not in source:
    raise SystemExit("MaaS cleanup preparation must remain scoped to Kind and preserve the existing OpenShift path")
if 'KIND_MAAS_CLEANUP_STATE_FILE must reuse the private state journal' not in source:
    raise SystemExit("Kind cleanup must require the exact private state journal from preparation")

if "have naturally completed deletion; only then remove the Gateway parents" not in source:
    raise SystemExit("cleanup ordering rationale is missing")

if 'KIND_SERVING_CLEANUP_MODE must be fresh or resume' not in source or '"$CLEANUP_MODE" == fresh' not in source:
    raise SystemExit("fresh and resume cleanup modes must be explicit")

if "snapshot_owned_pool_gateways()" not in source or '(.metadata.name==$a or .metadata.name==$b)' not in source:
    raise SystemExit("post-cleanup proof must filter the run-owned pool Gateways, not the task ingress Gateway")

if any(fragment in source for fragment in ("oc patch", "kubectl patch", "remove-finalizer", "finalizers/remove")):
    raise SystemExit("serving cleanup must not remove finalizers manually")

print("AITenant cleanup precedes pool Gateway deletion without finalizer removal: PASS")
PY
