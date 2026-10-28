#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR" <<'PY'
from pathlib import Path
import re
import sys

root = Path(sys.argv[1])
operator = (root / "deploy-grid-operator.sh").read_text(encoding="utf-8")
gateway = (root / "deploy-gateway-path.sh").read_text(encoding="utf-8")
bootstrap_path = root / "operator-bootstrap-praxis.yaml"
bootstrap = bootstrap_path.read_text(encoding="utf-8")

required_operator = (
    'GRID_PRAXIS_CONFIG_GENERATE=true requires GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE',
    'GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION=true requires GRID_PRAXIS_CONFIG_GENERATE=true',
    'mode:"render",status:"rendered",clusterMutated:false',
    'generate: true',
    'clusterEndpoints:',
    'mountReconciliation:',
    '.phase=="Rendered" and .observedGeneration==$generation',
    '.phase=="Ready" and .deploymentGeneration==$generation',
    'grid.praxis-proxy.io/mount-reconciliation"]=="enabled"',
    'grid.praxis-proxy.io/network"]==$network',
    'grid.praxis-proxy.io/gateway"]==$gateway',
    'overlayFile":"/etc/praxis/routing/routing-overlay.json"',
    'overlayReloadEnabled',
    'gateway-mounted-overlay-summary.json',
    'gateway-overlay-projection-attempts.jsonl',
    'selector.matchLabels|to_entries',
    '[.candidates[].cluster]|unique|length',
    'projection_deadline=$((SECONDS + ${GRID_PRAXIS_CONFIG_TIMEOUT_SECONDS:-300}))',
    'Gateway overlay projection did not converge before timeout',
    '.revision==$revision and .scope.network==$network',
)
missing = [item for item in required_operator if item not in operator]
if missing:
    print(f"Grid operator/Gateway overlay handoff gate is incomplete: {missing!r}", file=sys.stderr)
    raise SystemExit(1)

if 'if [[ "$config_cm" != "$GRID_PRAXIS_CONFIG_MAP_NAME" ]]' not in operator or \
   '[[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true ]]' not in operator:
    print("operator handoff must require the configured consumer ConfigMap when delegated mount reconciliation is disabled", file=sys.stderr)
    raise SystemExit(1)

render_start = operator.index('  render)')
render_end = operator.index('  install)', render_start)
render_branch = operator[render_start:render_end]
if 'oc apply --dry-run=server' not in render_branch or 'helm upgrade --install' in render_branch or 'oc apply -f ' in render_branch:
    print("Grid operator render mode must stop after ownership checks and server-side dry-run", file=sys.stderr)
    raise SystemExit(1)

required_gateway = (
    'CONFIG_SOURCE="${GRID_GATEWAY_PRAXIS_CONFIG_SOURCE:-byo}"',
    'byo|operator)',
    'MOUNT_RECONCILIATION_ENABLED" == true',
    'PRAXIS_CONFIG_MAP_EXTERNAL="${GRID_GATEWAY_PRAXIS_CONFIG_MAP_EXTERNAL:-false}"',
    'validate-generated-praxis-config.py',
    'if [[ -n "$PRAXIS_CONFIG_FILE" && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false ]]',
    'source: byo',
    'configMapName: "$OVERLAY_CONFIGMAP"',
    'sidecar:\n    enabled: false',
    'configMapName: "$OPERATOR_CONFIG_MAP_NAME"',
    'mountReconciliation:',
    'operator-bootstrap-configmap.yaml',
    'operator-bootstrap-configmap-apply.txt',
    'status:(if $mode=="render" then "rendered"',
    'if [[ -n "$OVERLAY_CONFIGMAP" ]]',
    'grid-overlay-status-before.json',
    'grid-overlay-configmap-summary.json',
    'if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then',
    'operator Praxis source owns the generated config and overlay mount; do not set BYO config or overlay inputs',
    '(.metadata.labels["app.kubernetes.io/managed-by"]=="grid-operator" and',
    'external Praxis ConfigMap is not owned by this run or the matching task GridNetwork',
)
missing = [item for item in required_gateway if item not in gateway]
if missing:
    print(f"Gateway operator-source preparation contract is incomplete: {missing!r}", file=sys.stderr)
    raise SystemExit(1)

external_source_start = gateway.index('if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then')
external_source_end = gateway.index('\n    live_config_sha=', external_source_start)
external_source = gateway[external_source_start:external_source_end]
if '.metadata.labels["app.kubernetes.io/managed-by"]=="grid-operator"' not in external_source or \
   'validate-generated-praxis-config.py' not in gateway:
    print("Grid-generated Praxis ConfigMap must be ownership-checked and structurally validated before the task Gateway mounts it", file=sys.stderr)
    raise SystemExit(1)

if "filter: static_response" not in bootstrap or "status: 503" not in bootstrap:
    print("operator bootstrap must fail closed with a temporary 503 responder", file=sys.stderr)
    raise SystemExit(1)
if "authorization" in bootstrap.lower() or "token" in bootstrap.lower() or "endpoints:" in bootstrap:
    print("bootstrap configuration must not contain credentials or backend endpoints", file=sys.stderr)
    raise SystemExit(1)

regex_match = re.search(
    r"(?m)^\s*reload:\s*\r?\n\s+enabled:\s+true\s*$",
    "      - filter: intelligent_route\n        reload:\n          enabled: true\n",
)
if not regex_match:
    print("the generated config reload contract fixture is invalid", file=sys.stderr)
    raise SystemExit(1)

print("Grid Gateway operator source, fail-closed bootstrap, mount handoff, and overlay revision verification: PASS")
PY
