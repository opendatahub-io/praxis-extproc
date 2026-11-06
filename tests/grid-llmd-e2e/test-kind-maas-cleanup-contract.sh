#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/prepare-kind-maas-cleanup.sh" "$SCRIPT_DIR/kind-maas-cleanup.Containerfile" \
  "$SCRIPT_DIR/cleanup-kind-serving.sh" "$SCRIPT_DIR/kind_maas_injection.py" <<'PY'
from pathlib import Path
import sys

script = Path(sys.argv[1]).read_text(encoding="utf-8")
containerfile = Path(sys.argv[2]).read_text(encoding="utf-8")
serving_cleanup = Path(sys.argv[3]).read_text(encoding="utf-8")
injection_helper = Path(sys.argv[4]).read_text(encoding="utf-8")

required = (
    "kind get clusters | rg -qx \"$CLUSTER\"",
    "RELATED_IMAGE_UBI_MINIMAL_IMAGE",
    "maas-api-key-cleanup-image",
    "openssl verify -CAfile",
    "-verify_hostname \"$API_HOST\"",
    "preconditions:{uid:$uid}",
    "/api/v1/namespaces/$NAMESPACE/secrets/$CA_SECRET",
    "originalCleanupImageEnv",
    "KIND_MAAS_CLEANUP_MODE must be plan, prepare, or restore",
    "caSecretCreatedByThisRun",
    "AITenant/$tenant remains; restore only after both revocation finalizers finish",
    "kind-maas-cleanup-state/v2",
    "namespaceInjection",
    "maas-namespace-injection-before.json",
    "maas-namespace-injection-restore-patch.json",
    "originalNamespaceInjectionLabelRestored:true",
)
missing = [fragment for fragment in required if fragment not in script]
if missing:
    raise SystemExit(f"Kind MaaS cleanup safeguards are missing: {missing}")

if "maas-api-ca-source.raw.json" in script or "maas-controller-before.raw.json" in script:
    raise SystemExit("the helper must not write raw Secret or Deployment payloads to evidence")
if "remove-finalizer" in script or "finalizers/remove" in script:
    raise SystemExit("the helper must not remove finalizers")
if '"op": "test", "path": "/metadata/uid"' not in injection_helper:
    raise SystemExit("namespace injection changes must test the original Namespace UID")
if '"path": "/metadata/labels/istio-injection"' not in injection_helper:
    raise SystemExit("namespace injection changes must be guarded by the exact original label")
if '"value": "enabled"' not in injection_helper or '"value": "disabled"' not in injection_helper:
    raise SystemExit("namespace injection changes must transition enabled to disabled and restore it")
if "revision-based Istio injection" not in injection_helper or "ambient Istio dataplane" not in injection_helper:
    raise SystemExit("unsupported revision or ambient injection modes must fail closed")
if ".prepared==true" not in serving_cleanup:
    raise SystemExit("serving cleanup must reject an incomplete MaaS preparation journal")
if "kind-maas-cleanup-state/v2" not in serving_cleanup:
    raise SystemExit("serving cleanup must require the namespace-aware cleanup journal")
if "@sha256:907b68736aa798b2d38255b7aa070b2a70acb90803864a40f05d0ec47556ddd0" not in containerfile:
    raise SystemExit("the helper image must use the recorded immutable UBI base digest")
if "USER 1001" not in containerfile:
    raise SystemExit("the cleanup helper image must satisfy Kind's runAsNonRoot job policy")

print("Kind MaaS cleanup image, TLS, Namespace UID/label journal, natural finalization, restore, and evidence contracts: PASS")
PY
