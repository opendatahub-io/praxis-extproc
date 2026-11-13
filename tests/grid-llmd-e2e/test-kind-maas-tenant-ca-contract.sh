#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-maas-tenant-ca.sh" <<'PY'
from pathlib import Path
import subprocess
import sys

script = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'MAAS_ISSUER="rhai-ca-issuer"',
    'spec:\n  selfSigned: {}',
    'isCA: true',
    'secretName: $ROOT_SECRET',
    'spec:\n  ca:\n    secretName: $ROOT_SECRET',
    'status.conditions[]?;.type=="Ready" and .status=="True"',
    'privateKeyBytesRecorded:false',
    'hasTLSPrivateKey:((.data//{})|has("tls.key"))',
    'run-owned AITenants remain; wait for their natural deletion',
    'run-owned leaf Certificates remain',
    '.metadata.uid==$uid',
    'KIND_MAAS_TENANT_CA_MODE must be plan, prepare-issuer, install, trust, verify, or cleanup',
    'MODE" == install || "$MODE" == prepare-issuer',
    'mode:$mode,status:(if $mode=="prepare-issuer" then "issuer-ready" else "ready" end)',
    'jq -n --arg mode "$MODE" --arg run "$RUN_ID" --arg issuer "$MAAS_ISSUER"',
    'Authorino trust is intentionally deferred until the MaaS API CA bundle exists',
    'Authorino trust bundle',
    'AUTHORINO_CA_CONFIGMAP="authorino-maas-api-ca"',
    'SSL_CERT_FILE',
    'combinedBundleSHA256',
    'expected_bundle_sha="$(jq -er \'.authorinoTrust.combinedBundleSHA256\' <<<"$state")"',
    'observed_bundle_sha="$(sha256sum "$EVIDENCE_DIR/authorino-ca-verify.pem" | awk \'{print $1}\')"',
    '"$observed_bundle_sha" == "$expected_bundle_sha"',
    'restore_authorino_trust "$state"',
    'TLS verification remains enabled',
    'trust, verify, or cleanup',
    'idempotent no-op',
    'Verified the run tenant CA was already present',
)
missing = [item for item in required if item not in script]
if missing:
    raise SystemExit(f"Kind MaaS tenant CA safeguards are missing: {missing}")

if '== "$sha"' in script:
    raise SystemExit("Kind MaaS tenant CA verification must compare against the saved state digest, not an unset shell variable")

for forbidden in (
    'kubectl get secret "$ROOT_SECRET" -o yaml',
    'kubectl get secrets -A -o yaml',
    'tls.key: ',
    'remove-finalizer',
    'finalizers/remove',
):
    if forbidden in script:
        raise SystemExit(f"Kind MaaS tenant CA helper must not contain {forbidden!r}")

result = subprocess.run(
    ["bash", str(Path(sys.argv[1])),],
    env={"KIND_MAAS_TENANT_CA_MODE": "plan"},
    text=True,
    capture_output=True,
    check=False,
)
if result.returncode != 0 or "no cluster calls" not in result.stdout:
    raise SystemExit("Kind MaaS tenant CA plan mode must be non-mutating")

lifecycle = Path(sys.argv[1]).with_name("run-kind-lifecycle.sh").read_text(encoding="utf-8")
required_lifecycle = (
    'KIND_MAAS_TENANT_CA_MODE=prepare-issuer',
    'KIND_MAAS_API_CA_MODE=install',
    'KIND_MAAS_TENANT_CA_MODE=trust',
    'run_phase tenant-ca-trust',
)
if any(item not in lifecycle for item in required_lifecycle):
    raise SystemExit("Kind lifecycle must prepare the issuer once, then add Authorino tenant trust using the existing CA state")
issuer = lifecycle.index('KIND_MAAS_TENANT_CA_MODE=prepare-issuer')
api_ca = lifecycle.index('KIND_MAAS_API_CA_MODE=install')
tenant_trust = lifecycle.index('KIND_MAAS_TENANT_CA_MODE=trust')
if not issuer < api_ca < tenant_trust:
    raise SystemExit("tenant CA issuer preparation, MaaS API CA setup, and Authorino tenant trust are out of dependency order")
if 'KIND_MAAS_TENANT_CA_MODE=install KIND_MAAS_TENANT_CA_STATE_FILE="$STATE_DIR/tenant-ca-state.json"' in lifecycle:
    raise SystemExit("tenant CA lifecycle must not invoke install a second time with the issuer state file")

print("Kind MaaS tenant CA issuer, state reuse, Authorino trust ordering, private-key, ownership, and cleanup contracts: PASS")
PY
