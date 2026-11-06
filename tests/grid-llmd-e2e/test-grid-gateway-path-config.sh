#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-gateway-path.sh" <<'PY'
from pathlib import Path
import json
import os
import subprocess
import sys

SCRIPT = Path(sys.argv[1])
source = SCRIPT.read_text(encoding="utf-8")
checks = (
    'Modes: plan, render, install, verify, collect.',
    '[[ "$MODE" == render || "$MODE" == install || "$MODE" == verify || "$MODE" == collect ]]',
    'if [[ "$MODE" == render ]]; then',
    'clusterMutated:false',
    'Grid Gateway render and server-side dry-runs passed; no cluster resources changed',
    'GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"',
    'CHART_VERSION="${PRAXIS_GATEWAY_CHART_VERSION:-}"',
    'CLUSTER_PLATFORM="${GRID_LLMD_CLUSTER_PLATFORM:-openshift}"',
    'GRID_SERVING_ENABLED="${GRID_GATEWAY_GRID_SERVING_ENABLED:-false}"',
    'RELEASE_HELM_MOUNTS="${GRID_GATEWAY_RELEASE_HELM_MOUNTS:-$MOUNT_RECONCILIATION_ENABLED}"',
    'HOSTNAME="${GRID_GATEWAY_HOSTNAME:-${GRID_SITE_NAME}.grid.internal}"',
    'command helm --kube-context="$KUBE_CONTEXT" "$@"',
    '-f "$SCRIPT_DIR/single-ready-pinned-pod.jq"',
    'specContainers:[.spec.containers[]?|{name,image}]',
    'GATEWAY_CONTROLLER_NAME="$(jq -er',
    'render_gateway_object_annotations() {',
    'networking.istio.io/service-type: ClusterIP',
    'Kind task Gateway requires CLUSTER_ENV=kind and the Istio Gateway controller',
    'gateway-object-annotations.txt',
    '.controllerName==$controller',
    '- Helm Kubernetes context: $KUBE_CONTEXT',
    'helm --kube-context=%q upgrade --install',
    'Expected API server: `%s`',
    'Observed API server: `%s`',
    'capture_failure 1',
    'observed_chart_name="$(awk -F\': \' \'$1 == "name" { print $2; exit }\' <<<"$chart_metadata")"',
    'observed_chart_version="$(awk -F\': \' \'$1 == "version" { print $2; exit }\' <<<"$chart_metadata")"',
    'if [[ -z "$CHART_VERSION" ]]; then',
    'CHART_VERSION="$observed_chart_version"',
    'Praxis Gateway chart version mismatch: expected $CHART_VERSION, observed $observed_chart_version',
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
    'configured-praxis-base-configmap-metadata.json',
    'active_configmap_name="$PRAXIS_CONFIG_MAP_NAME"',
    'if [[ "$active_configmap_name" == "${SERVICE_NAME}-config" ]]; then',
    '.metadata.labels["praxis.fast/qualification-run"]==$run and',
    '.metadata.labels["app.kubernetes.io/instance"]==$release',
    '.metadata.labels["app.kubernetes.io/managed-by"]=="Helm"',
    '.metadata.annotations["meta.helm.sh/release-name"]==$release',
    '.metadata.annotations["meta.helm.sh/release-namespace"]==$namespace',
    'active bootstrap Praxis ConfigMap is neither owned by this task run nor by the matching Helm release',
    '.metadata.labels["app.kubernetes.io/managed-by"]=="grid-llmd-e2e-run"',
    '.metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"',
    'active-bootstrap-configmap-metadata.json',
    'role:"active-same-run-bootstrap"',
    'external_configmap_json="$base_configmap_json"',
    'validate the Grid-generated target ConfigMap that Helm is about to',
    'select(.name=="config")',
    'grid-consumer-config-*',
    'usesDoubleBufferedGridSlot',
    'select-grid-gateway-configmap.py',
    'praxis_config_map_name="$(python3 "$SCRIPT_DIR/select-grid-gateway-configmap.py"',
    'active_configmap_for_render="$(jq -er',
    'rendered_praxis_config_map_name="$praxis_config_map_name"',
    'active Grid Praxis ConfigMap changed after Helm values were rendered',
    'helmMountedConfigMapName:$chartName',
    '- Helm-mounted Praxis ConfigMap: ',
    'active double-buffered Praxis ConfigMap is not owned by the matching task GridNetwork and Gateway',
    'expectedEndpointInventorySha256:$endpointsSha',
    'OVERLAY_CONFIGMAP="${GRID_GATEWAY_OVERLAY_CONFIGMAP:-}"',
    'flavor: grid-gateway',
    'configMapName: "$praxis_config_map_name"',
    'configMapName: "$OVERLAY_CONFIGMAP"',
    'mountReconciliation:',
    'releaseHelmMounts: $RELEASE_HELM_MOUNTS',
    'source: operator',
    'operator Praxis source does not support the external BYO overlay mount',
    'GridNetwork has no current Distributed overlay or explicitly allowed retained overlay for the task Gateway',
    'grid-overlay-status-before.json',
    'grid-overlay-configmap-summary.json',
    'configMapName: "$OVERLAY_CONFIGMAP"',
    'if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then',
    'configured Praxis ConfigMap is not owned by this run or the matching task GridNetwork',
    'Grid-generated Praxis ConfigMap failed overlay, reload, endpoint, or credential-field validation',
    'if [[ -n "$PRAXIS_CONFIG_FILE" && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false ]]',
    'Rendered Gateway Deployment does not mount the configured task Grid overlay ConfigMap',
    'mountPath: "/etc/praxis/routing"',
    'Grid operator-generated consumer config',
    'RUNTIME_READY_TIMEOUT_SECONDS="${GRID_GATEWAY_RUNTIME_TIMEOUT_SECONDS:-300}"',
    'gateway-pod-readiness-attempts.jsonl',
    'pod_deadline=$((SECONDS + RUNTIME_READY_TIMEOUT_SECONDS))',
    'while ((SECONDS < pod_deadline)); do',
    'gateway_pod_ready=true',
    'Gateway Pod set did not converge to one active Ready Pod with the pinned runtime digest',
    'gateway-proxy-readiness-attempts.jsonl',
    'gateway_proxy_deadline=$((SECONDS + RUNTIME_READY_TIMEOUT_SECONDS))',
    'while ((SECONDS < gateway_proxy_deadline)); do',
    'single-ready-gateway-endpoint.jq',
    'Gateway API data-plane Service did not converge to one Ready EndpointSlice endpoint on 443',
    'operator-generated-config-no-overlay',
    'grid-generated-external-bootstrap',
    'then "bootstrap-only"',
    'expected_bootstrap_sha="$(sha256sum "$BOOTSTRAP_CONFIG_FILE" | awk \'{print $1}\')"',
    'observed_config_sha="$(jq -j -er \'.data["praxis.yaml"]\' <<<"$external_configmap_json" | sha256sum | awk \'{print $1}\')"',
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
select_slot = source.index('praxis_config_map_name="$(python3 "$SCRIPT_DIR/select-grid-gateway-configmap.py"')
render_values = source.index('cat >"$EVIDENCE_DIR/gateway-values.yaml"')
if select_slot > render_values:
    raise SystemExit("the active Grid Praxis ConfigMap must be selected before Helm values are rendered")
if 'openshift.io/gateway-controller/v1' in source:
    print("Gateway route readiness must use the selected GatewayClass controller, not an OpenShift-only controller name", file=sys.stderr)
    raise SystemExit(1)

annotation_start = source.index("render_gateway_object_annotations() {")
annotation_end = source.index("\n}", annotation_start) + 2
annotation_function = source[annotation_start:annotation_end]

def render_annotation(platform, environment, controller):
    return subprocess.run(
        ["bash", "-c", annotation_function + "\nrender_gateway_object_annotations"],
        env={
            **os.environ,
            "CLUSTER_PLATFORM": platform,
            "CLUSTER_ENV": environment,
            "GATEWAY_CONTROLLER_NAME": controller,
        },
        check=False,
        capture_output=True,
        text=True,
    )

kind_annotation = render_annotation("kind", "kind", "istio.io/gateway-controller")
if kind_annotation.returncode != 0 or kind_annotation.stdout != (
    "  annotations:\n    networking.istio.io/service-type: ClusterIP\n"
):
    raise SystemExit("Kind Istio Gateway must render its ClusterIP service annotation")
if render_annotation("kind", "ephemeral-dev", "istio.io/gateway-controller").returncode == 0:
    raise SystemExit("Kind Gateway annotation must reject a mismatched CLUSTER_ENV")
if render_annotation("kind", "kind", "other/gateway-controller").returncode == 0:
    raise SystemExit("Kind Gateway annotation must reject unsupported controllers")
if render_annotation("openshift", "ephemeral-dev", "openshift.io/gateway-controller").stdout:
    raise SystemExit("OpenShift Gateway rendering must not receive the Kind-only annotation")

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
    'source: byo',
    'configMapName: "$praxis_config_map_name"',
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
pod_poll = source.index('pod_attempts="$EVIDENCE_DIR/gateway-pod-readiness-attempts.jsonl"')
pod_gate = source.index('Praxis Gateway does not have exactly one active Ready Pod')
if not rollout_status < pod_poll < pod_snapshot < pod_gate:
    print("Gateway Pod image identity must poll after rollout completion and before the final stable snapshot", file=sys.stderr)
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
if 'overlay:' in operator_values:
    print("operator-source Gateway values must leave the overlay mount to supported BYO external mode", file=sys.stderr)
    raise SystemExit(1)
if 'if [[ -n "$OVERLAY_CONFIGMAP" ]]' not in source:
    print("the direct overlay preflight must verify the current operator-distributed ConfigMap", file=sys.stderr)
    raise SystemExit(1)

endpoint_predicate = SCRIPT.parent / "single-ready-gateway-endpoint.jq"
if not endpoint_predicate.is_file():
    raise SystemExit("Gateway TLS EndpointSlice readiness predicate is missing")

def gateway_endpoint_ready(slices):
    return subprocess.run(
        ["jq", "-e", "-f", str(endpoint_predicate)],
        input=json.dumps({"items": slices}),
        capture_output=True,
        text=True,
        check=False,
    ).returncode == 0

if gateway_endpoint_ready([]):
    raise SystemExit("an absent Istio Gateway EndpointSlice must remain unready")
ready_slice = [{"ports": [{"name": "https", "port": 443}], "endpoints": [
    {"conditions": {"ready": True}, "addresses": ["10.0.0.1"]}
]}]
if not gateway_endpoint_ready(ready_slice):
    raise SystemExit("one Ready HTTPS EndpointSlice endpoint should pass")
two_ready_endpoints = [{"ports": [{"name": "https", "port": 443}], "endpoints": [
    {"conditions": {"ready": True}, "addresses": ["10.0.0.1"]},
    {"conditions": {"ready": True}, "addresses": ["10.0.0.2"]},
]}]
if gateway_endpoint_ready(two_ready_endpoints):
    raise SystemExit("multiple Ready Gateway endpoints must fail the single-endpoint contract")
wrong_port = [{"ports": [{"name": "https", "port": 8443}], "endpoints": [
    {"conditions": {"ready": True}, "addresses": ["10.0.0.1"]}
]}]
if gateway_endpoint_ready(wrong_port):
    raise SystemExit("a Ready Gateway endpoint without TLS port 443 must fail")
not_ready = [{"ports": [{"name": "https", "port": 443}], "endpoints": [
    {"conditions": {"ready": False}, "addresses": ["10.0.0.1"]}
]}]
if gateway_endpoint_ready(not_ready):
    raise SystemExit("a non-Ready Gateway endpoint must fail")

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
