#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT

cat >"$TEMP_DIR/operator-values.yaml" <<'YAML'
image:
  repository: quay.io/example/operator
  digest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
grid:
  id: grid-run-123
  network: grid-run
  swimKeySecretName: ""
site:
  name: site-run
enrollment:
  enabled: false
YAML

python3 "$SCRIPT_DIR/prepare-grid-operator-bootstrap-values.py" \
  "$TEMP_DIR/operator-values.yaml" "$TEMP_DIR/bootstrap-values.yaml"
sed 's/^  id: .*/  id: ""/' "$TEMP_DIR/operator-values.yaml" >"$TEMP_DIR/expected-bootstrap-values.yaml"
cmp -s "$TEMP_DIR/expected-bootstrap-values.yaml" "$TEMP_DIR/bootstrap-values.yaml"
grep -Fq '  id: ""' "$TEMP_DIR/bootstrap-values.yaml"
grep -Fq '  network: grid-run' "$TEMP_DIR/bootstrap-values.yaml"
grep -Fq '  enabled: false' "$TEMP_DIR/bootstrap-values.yaml"

sed '/^  id:/d' "$TEMP_DIR/operator-values.yaml" >"$TEMP_DIR/missing-grid-id.yaml"
if python3 "$SCRIPT_DIR/prepare-grid-operator-bootstrap-values.py" \
  "$TEMP_DIR/missing-grid-id.yaml" "$TEMP_DIR/invalid.yaml" 2>/dev/null; then
  printf 'bootstrap values transformation unexpectedly accepted a missing grid.id\n' >&2
  exit 1
fi

python3 - "$SCRIPT_DIR/deploy-grid-operator.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
namespace_start = source.index("ensure_owned_namespace() {")
namespace_end = source.index("\nensure_internal_image_pull_access() {", namespace_start)
namespace_helper = source[namespace_start:namespace_end]
ownership_check = namespace_helper.index('[[ "$owner" == "$RUN_LABEL" ]]')
shared_label = namespace_helper.rindex('oc label namespace "$namespace" app.kubernetes.io/part-of=grid-llmd-e2e')
if not ownership_check < shared_label:
    raise SystemExit("existing namespace must pass the run-ownership check before adding the shared component label")
if 'app.kubernetes.io/managed-by=grid-llmd-e2e-run' in namespace_helper[shared_label:]:
    raise SystemExit("relabeling an existing namespace must preserve its original managed-by label")
required = (
    'command helm --kube-context="$KUBE_CONTEXT" "$@"',
    'helm list -n "$RELEASE_NAMESPACE" --filter "^$RELEASE_NAME$" -o json',
    'appVersion:.app_version,updated',
    '[[ "$MODE" == render || "$MODE" == install || "$MODE" == verify || "$MODE" == collect ]]',
    'Render performs read-only ownership checks and server-side dry-run only',
    'mode:"render",status:"rendered",clusterMutated:false',
    'GRID_PRAXIS_SITE_ROUTER="${GRID_PRAXIS_SITE_ROUTER:-intelligentRoute}"',
    'CHART_VERSION="${GRID_OPERATOR_CHART_VERSION:-}"',
    'Set GRID_PRAXIS_SITE_ROUTER=gridSiteRoute during bootstrap to select the future',
    'siteRouter: $GRID_PRAXIS_SITE_ROUTER',
    'grid_signals=poll',
    'signals:',
    '  enabled: false',
    '    enabled: false',
    'generated GRID_PRAXIS_SITE_ROUTER=gridSiteRoute requires GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION=true',
    "grep -Fq 'mode: \"poll\"' <<<\"$network_render\"",
    '[[ "$observed_chart_name" == grid-operator ]]',
    'observed_chart_version="$(awk -F\': \' \'$1 == "version" { print $2; exit }\' <<<"$chart_metadata")"',
    'if [[ -z "$CHART_VERSION" ]]; then',
    'CHART_VERSION="$observed_chart_version"',
    'operator chart version mismatch: expected $CHART_VERSION, observed $observed_chart_version',
    'grid_crd_render_count="$(grep -c \'^kind: CustomResourceDefinition$\'',
    "grep -Eq '^kind: (GridNetwork|GridSite|InferenceProvider)$'",
    "CURRENT_PHASE=helm-crd-bootstrap",
    '--values "$EVIDENCE_DIR/operator-bootstrap-values.yaml"',
    'oc wait --for=condition=Established "crd/$crd"',
    'server-dry-run-after-crds.txt',
    "CURRENT_PHASE=helm-install",
    '--values "$EVIDENCE_DIR/operator-values.yaml"',
)
missing = [needle for needle in required if needle not in source]
if missing:
    print("Grid operator staged-install contract is missing: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)
if 'grep -Fq "org.opencontainers.image.revision: $SOURCE_REVISION" <<<"$chart_metadata"' in source:
    print("operator chart validation must use the archive/source lock, not absent Chart.yaml OCI annotations", file=sys.stderr)
    raise SystemExit(1)

stage_start = source.index("CURRENT_PHASE=helm-crd-bootstrap")
final_start = source.index("CURRENT_PHASE=helm-install", stage_start)
stage = source[stage_start:final_start]
ordered = (
    'helm upgrade --install "$RELEASE_NAME" "$CHART_FILE"',
    'oc wait --for=condition=Established "crd/$crd"',
    'capture_grid_crds "$EVIDENCE_DIR/grid-crds-after-bootstrap.json"',
    'server-dry-run-after-crds.txt',
)
positions = [stage.index(needle) for needle in ordered]
if positions != sorted(positions):
    print("Grid operator bootstrap steps are not in dependency order", file=sys.stderr)
    raise SystemExit(1)

print("Grid operator staged-install and explicit-context contract: PASS")
PY

python3 - "$SCRIPT_DIR/deploy-grid-operator.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'DEPLOYMENT_NAME="${GRID_OPERATOR_DEPLOYMENT_NAME:-grid-operator}"',
    'SERVICE_ACCOUNT_NAME="${GRID_OPERATOR_SERVICE_ACCOUNT_NAME:-grid-operator}"',
    "ensure_internal_image_pull_access() {",
    'source_namespace="${repository_path%%/*}"',
    '[[ -n "$source_namespace" && "$source_namespace" == "$RESOURCE_NAMESPACE" ]]',
    'binding_name="${RELEASE_NAME}-image-puller"',
    'kind: RoleBinding',
    'praxis.fast/qualification-run: "$RUN_LABEL"',
    'name: $SERVICE_ACCOUNT_NAME',
    'namespace: $RELEASE_NAMESPACE',
    'name: system:image-puller',
    'oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e',
    'ensure_internal_image_pull_access',
)
missing = [fragment for fragment in required if fragment not in source]
if missing:
    raise SystemExit("operator image-pull/default identity contract is incomplete: " + repr(missing))

helper_start = source.index("ensure_internal_image_pull_access() {")
helper_end = source.index("verify_runtime() {", helper_start)
helper = source[helper_start:helper_end]
for forbidden in ("kind: ClusterRoleBinding", "oc create clusterrolebinding", "oc apply -f -"):
    if forbidden in helper:
        raise SystemExit("image-pull helper must remain a namespace-scoped RoleBinding: " + forbidden)
if not (helper.index("--dry-run=server") < helper.index("oc apply --server-side --field-manager=grid-llmd-e2e")):
    raise SystemExit("image-pull RoleBinding must pass server-side dry-run before apply")

print("Grid operator default Deployment and task-scoped registry pull RBAC contract: PASS")
PY

printf 'Grid operator bootstrap values and staged install: PASS\n'
