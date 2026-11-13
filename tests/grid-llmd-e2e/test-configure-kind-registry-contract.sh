#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPER="$SCRIPT_DIR/configure-kind-registry.sh"
TRANSFORM="$SCRIPT_DIR/configure-kind-registry-config.py"

python3 "$SCRIPT_DIR/test_configure_kind_registry_config.py"
python3 - "$HELPER" "$TRANSFORM" <<'PY'
from pathlib import Path
import sys

helper = Path(sys.argv[1]).read_text(encoding="utf-8")
transform = Path(sys.argv[2]).read_text(encoding="utf-8")
required = (
    'docker inspect "$NODE_NAME" --format',
    'docker network inspect "$NETWORK_NAME" --format',
    'HOST_DIR="/etc/containerd/certs.d/${REGISTRY_NAME}:5000"',
    'docker cp "$EVIDENCE_DIR/registry-hosts.toml" "$NODE_NAME:$HOST_DIR/hosts.toml"',
    'configure-kind-registry-config.py',
    'docker exec "$NODE_NAME" systemctl restart containerd',
    'kubectl --context "$KUBE_CONTEXT" wait',
    'kubectl --context "$KUBE_CONTEXT" get --raw=/readyz',
)
positions = [helper.find(fragment) for fragment in required]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("registry setup must check ownership, configure containerd, then pass node/API readiness")

if "tomllib.loads(output)" not in transform or "REGISTRY_CONFIG_PATH" not in transform:
    raise SystemExit("containerd transformation must parse and verify the resulting TOML")

print("Kind registry ownership, containerd config, and readiness contract: PASS")
PY
