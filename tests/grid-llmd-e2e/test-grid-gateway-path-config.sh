#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-gateway-path.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
checks = (
    'Modes: plan, render, install, verify, collect.',
    '[[ "$MODE" == render || "$MODE" == install || "$MODE" == verify || "$MODE" == collect ]]',
    'if [[ "$MODE" == render ]]; then',
    'clusterMutated:false',
    'Grid Gateway render and server-side dry-runs passed; no cluster resources changed',
    'GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"',
    'GRID_SERVING_ENABLED="${GRID_GATEWAY_GRID_SERVING_ENABLED:-false}"',
    'RELEASE_HELM_MOUNTS="${GRID_GATEWAY_RELEASE_HELM_MOUNTS:-$MOUNT_RECONCILIATION_ENABLED}"',
    'HOSTNAME="${GRID_GATEWAY_HOSTNAME:-${GRID_SITE_NAME}.grid.internal}"',
    'command helm --kube-context="$KUBE_CONTEXT" "$@"',
    '- Helm Kubernetes context: $KUBE_CONTEXT',
    'helm --kube-context=%q upgrade --install',
    'Expected API server: `%s`',
    'Observed API server: `%s`',
    'capture_failure 1',
    'openssl verify -CAfile "$tmpdir/grid-ca.crt" "$tmpdir/site.crt"',
    'grep -Fq "DNS:$HOSTNAME" "$EVIDENCE_DIR/grid-identity-certificate-metadata.txt"',
    'PRAXIS_CONFIG_FILE="${GRID_GATEWAY_PRAXIS_CONFIG_FILE:-}"',
    'PRAXIS_CONFIG_MAP_EXTERNAL="${GRID_GATEWAY_PRAXIS_CONFIG_MAP_EXTERNAL:-false}"',
    'BOOTSTRAP_MODE="${GRID_GATEWAY_BOOTSTRAP_MODE:-false}"',
    'bootstrap mode accepts only the checked 503 startup responder config',
    'if [[ "$BOOTSTRAP_MODE" != true ]]; then',
    'EXPECTED_ENDPOINTS_FILE="${GRID_GATEWAY_EXPECTED_ENDPOINTS_FILE:-}"',
    'validate-generated-praxis-config.py',
    'external-praxis-config-validation.json',
    'expectedEndpointInventorySha256:$endpointsSha',
    'OVERLAY_CONFIGMAP="${GRID_GATEWAY_OVERLAY_CONFIGMAP:-}"',
    'flavor: grid-gateway',
    'configMapName: "$praxis_config_map_name"',
    'configMapName: "$OVERLAY_CONFIGMAP"',
    'mountReconciliation:',
    'releaseHelmMounts: $RELEASE_HELM_MOUNTS',
    'source: operator',
    'operator Praxis source owns the generated config and overlay mount; do not set BYO config or overlay inputs',
    'GridNetwork has no current Distributed overlay or explicitly allowed retained overlay for the task Gateway',
    'grid-overlay-status-before.json',
    'grid-overlay-configmap-summary.json',
    'configMapName: "$OVERLAY_CONFIGMAP"',
    'if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then',
    'external Praxis ConfigMap is not owned by this run or the matching task GridNetwork',
    'Grid-generated Praxis ConfigMap failed overlay, reload, endpoint, or credential-field validation',
    'if [[ -n "$PRAXIS_CONFIG_FILE" && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false ]]',
    'Rendered Gateway Deployment does not mount the configured task Grid overlay ConfigMap',
    'mountPath: "/etc/praxis/routing"',
    'Grid operator-generated consumer config',
    'operator-generated-overlay-aware',
    "grep -Fq '/etc/praxis/routing/routing-overlay.json' \"$PRAXIS_CONFIG_FILE\"",
    'BYO_MOUNT_RECONCILIATION_VALUES="$(cat <<YAML',
    'if [[ "$CONFIG_SOURCE" == byo && "$MOUNT_RECONCILIATION_ENABLED" == true ]]',
    'destination_identity_json="$(oc get secret',
    'destination-identity-before-metadata.json',
    'destination-ca-before-metadata.json',
    'gatewayNs "$NAMESPACE"',
    '.spec.tls.siteSecretRef.namespace==$gatewayNs',
    '.spec.tls.caSecretRef.namespace==$gatewayNs',
    'run-owned Gateway-namespace copies',
    'backendRefs:',
    'name: $SERVICE_NAME',
)
missing = [value for value in checks if value not in source]
if missing:
    print("Grid Gateway path config contract is missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

site_declaration = source.index('GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"')
hostname_declaration = source.index('HOSTNAME="${GRID_GATEWAY_HOSTNAME:-${GRID_SITE_NAME}.grid.internal}"')
if site_declaration > hostname_declaration:
    print("GridSite must be resolved before deriving the default Gateway hostname", file=sys.stderr)
    raise SystemExit(1)

route = source[source.index("cat >\"$EVIDENCE_DIR/gateway-route.yaml\""):]
if "name: $SERVICE_NAME" not in route or "llmInferenceService" in route or "inferencePool" in route:
    print("task Gateway HTTPRoute must target only its named Grid Gateway Service", file=sys.stderr)
    raise SystemExit(1)

byo_start = source.index('else\ncat >"$EVIDENCE_DIR/gateway-values.yaml"')
byo_end = source.index("fi\n\nhelm template", byo_start)
byo_values = source[byo_start:byo_end]
for value in (
    'configMapName: "$OVERLAY_CONFIGMAP"',
    'sidecar:\n    enabled: false',
    '$BYO_MOUNT_RECONCILIATION_VALUES',
):
    if value not in byo_values:
        print(f"BYO Gateway values must preserve the overlay mount and optional operator delegation: {value!r}", file=sys.stderr)
        raise SystemExit(1)

delegation_start = source.index('BYO_MOUNT_RECONCILIATION_VALUES="$(cat <<YAML')
delegation_end = source.index('YAML\n)"', delegation_start)
delegation_values = source[delegation_start:delegation_end]
for value in (
    'mountReconciliation:\n  enabled: true',
    'releaseHelmMounts: $RELEASE_HELM_MOUNTS',
    'serviceAccount: $GRID_OPERATOR_SERVICE_ACCOUNT',
    'namespace: $GRID_OPERATOR_NAMESPACE',
):
    if value not in delegation_values:
        print(f"BYO mount delegation must stay scoped to this operator identity: {value!r}", file=sys.stderr)
        raise SystemExit(1)

render_start = source.index('if [[ "$MODE" == render ]]; then')
dry_run = source.index('oc apply --dry-run=server -f "$EVIDENCE_DIR/gateway-route.yaml"')
secret_bootstrap = source.index('CURRENT_PHASE=task-secret-bootstrap')
if not dry_run < render_start < secret_bootstrap:
    print("render mode must finish server-side dry-runs before exiting ahead of all secret/Helm/resource mutations", file=sys.stderr)
    raise SystemExit(1)

rollout_status = source.index('oc rollout status deployment/"$DEPLOYMENT_NAME"')
pod_snapshot = source.index('oc get pods -n "$NAMESPACE" -l app.kubernetes.io/instance="$RELEASE_NAME" -o json')
if rollout_status > pod_snapshot:
    print("Gateway runtime Pod evidence must be collected only after the Deployment rollout is complete", file=sys.stderr)
    raise SystemExit(1)
if 'trap \'rm -rf "$tmpdir"\' EXIT' in source:
    print("temporary certificate cleanup must not replace the failure-result EXIT trap", file=sys.stderr)
    raise SystemExit(1)
if 'if [[ -n "${tmpdir:-}" && -d "$tmpdir" ]]; then' not in source:
    print("Gateway success and failure exits must clean up the temporary certificate directory", file=sys.stderr)
    raise SystemExit(1)

operator_start = source.index('if [[ "$CONFIG_SOURCE" == operator ]]; then\ncat >"$EVIDENCE_DIR/gateway-values.yaml"')
operator_end = source.index('\nelse\ncat >"$EVIDENCE_DIR/gateway-values.yaml"', operator_start)
operator_values = source[operator_start:operator_end]
for value in (
    'source: operator',
    'configMapName: "$OPERATOR_CONFIG_MAP_NAME"',
    'gridServing:\n      enabled: $GRID_SERVING_ENABLED\n      networkName: "$GRID_NETWORK_NAME"\n      gatewayRefName: "$SERVICE_NAME"',
    'mountReconciliation:\n  enabled: true',
):
    if value not in operator_values:
        print(f"operator config must preserve the supported operator handoff: {value!r}", file=sys.stderr)
        raise SystemExit(1)
if 'configMapName: "$OVERLAY_CONFIGMAP"' in operator_values or 'sidecar:' in operator_values:
    print("the Helm operator source mode does not support a direct overlay mount", file=sys.stderr)
    raise SystemExit(1)
if 'if [[ -n "$OVERLAY_CONFIGMAP" ]]' not in source:
    print("the direct overlay preflight must verify the current operator-distributed ConfigMap", file=sys.stderr)
    raise SystemExit(1)

if 'destination-identity-before.json' in source or 'destination-ca-before.json' in source:
    print("Secret evidence must contain metadata/key names only, never raw Secret JSON", file=sys.stderr)
    raise SystemExit(1)

for raw_capture in (
    'oc get secret "$TLS_SECRET_NAME" -n "$NAMESPACE" -o json >',
    'oc get secret "$SOURCE_CA_SECRET" -n "$NAMESPACE" -o json >',
):
    if raw_capture in source:
        print("raw Secret JSON must not be redirected into the evidence directory", file=sys.stderr)
        raise SystemExit(1)

print("Grid Gateway hostname, context, TLS, task config/overlay, Secret evidence, and backend contract: PASS")
PY
