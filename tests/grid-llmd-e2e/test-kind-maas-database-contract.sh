#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-maas-database.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = [
    '--expected-api-server) EXPECTED_API_SERVER=',
    '--kubeconfig) KUBECONFIG_FILE=',
    '--image) IMAGE=',
    'namespace is not owned by this run',
    'refusing to adopt pre-existing',
    "grep -qi 'not found'",
    'openssl rand -hex 32',
    'chmod 0600 "$password_file" "$env_file" "$url_file"',
    'k apply --dry-run=server -f "$EVIDENCE_DIR/postgres-resources.yaml"',
    'k create -f "$EVIDENCE_DIR/postgres-resources.yaml"',
    'pg_isready -U maas -d maas',
    '[.containerStatuses[]|select(.name=="postgres" and .ready==true and (.imageID|endswith("@"+$digest)))]|length==1',
    'emptyDir:',
    'database Secrets do not carry the run ownership labels',
    'trap \'status=$?; on_error "$status" "$LINENO"\' ERR',
    'failure-pods.json',
    'failure-postgres-previous.log',
    'secretValuesRecorded:false',
]
if any(item not in source for item in required):
    raise SystemExit("MaaS Kind database install must use the run-owned namespace, private secrets, dry-run, readiness, and image identity gates")
for forbidden in ('kubectl patch', 'set -x', '--from-literal=POSTGRES_PASSWORD', 'get secret "$secret" -o yaml'):
    if forbidden in source:
        raise SystemExit(f"database helper must not expose credentials or mutate with a live patch: {forbidden}")
for incompatible in ('fsGroup: 70', 'runAsNonRoot: true', 'runAsUser: 70'):
    if incompatible in source:
        raise SystemExit(f"PostgreSQL's image entrypoint must retain ownership of its data-dir initialization: {incompatible}")
if source.count('k create secret generic postgres-creds') != 2 or source.count('k create secret generic maas-db-config') != 2:
    raise SystemExit("both database Secrets must be server-side dry-run validated before plain create")
if 'app.kubernetes.io/managed-by=grid-llmd-kind' not in source or 'praxis.kind/run=$RUN_ID' not in source:
    raise SystemExit("database resources and Secrets must carry run ownership labels")
print("MaaS Kind database ownership, private credential, dry-run, readiness, and image-pin contract: PASS")
PY
