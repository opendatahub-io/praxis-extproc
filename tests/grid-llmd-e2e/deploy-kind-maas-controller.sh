#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
SOURCE_DIR=""
SOURCE_SHA=""
RUN_ID=""
CONTROLLER_IMAGE=""
API_IMAGE=""
PAYLOAD_IMAGE=""
EVIDENCE_DIR=""
APPLY=false

usage() {
  cat <<'USAGE'
Usage: deploy-kind-maas-controller.sh --context CTX --kubeconfig PRIVATE-FILE \
  --expected-api-server URL --source-dir CLEAN-MAAS-WORKTREE --source-sha SHA \
  --run-id ID --controller-image REPO@sha256:DIGEST --api-image REPO@sha256:DIGEST \
  --payload-image REPO@sha256:DIGEST --evidence-dir NEW-DIR [--apply]

Builds the MaaS controller render from the source-owned xKS overlay. This keeps
Kind certificate-manager TLS resources, removes OpenShift service-CA annotations,
and pins the three runtime images before the existing readiness installer runs.
Without --apply it performs a server-side dry-run only. It does not record Secret
data or private keys.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --source-dir) SOURCE_DIR="${2:?}"; shift 2 ;;
    --source-sha) SOURCE_SHA="${2:?}"; shift 2 ;;
    --run-id) RUN_ID="${2:?}"; shift 2 ;;
    --controller-image) CONTROLLER_IMAGE="${2:?}"; shift 2 ;;
    --api-image) API_IMAGE="${2:?}"; shift 2 ;;
    --payload-image) PAYLOAD_IMAGE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --apply) APPLY=true; shift ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { printf 'deploy-kind-maas-controller: %s\n' "$*" >&2; exit 2; }
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" ]] || fail 'explicit context, private kubeconfig, and expected API server are required'
[[ -d "$SOURCE_DIR" && -n "$EVIDENCE_DIR" ]] || fail 'locked MaaS source worktree and new evidence directory are required'
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || fail 'source SHA must be a full commit ID'
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,80}$ ]] || fail 'run ID is not a valid ownership label'
for image in "$CONTROLLER_IMAGE" "$API_IMAGE" "$PAYLOAD_IMAGE"; do
  [[ "$image" =~ ^[a-zA-Z0-9._:/-]+@sha256:[0-9a-f]{64}$ ]] || fail 'controller, API, and payload images must use immutable SHA-256 references'
done
[[ ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory already exists; refusing overwrite'
for binary in kubectl jq git kustomize python3 sha256sum; do command -v "$binary" >/dev/null || fail "$binary is required"; done

mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || fail 'selected context API server differs from the expected run cluster'
[[ "$(k get --raw=/readyz)" == *ok* ]] || fail 'Kubernetes API is not ready'
[[ "$(git -C "$SOURCE_DIR" rev-parse HEAD)" == "$SOURCE_SHA" ]] || fail 'MaaS source HEAD differs from the run lock'
[[ -z "$(git -C "$SOURCE_DIR" branch --show-current)" ]] || fail 'MaaS source worktree must be detached'
[[ "$(stat -c '%a' "$KUBECONFIG_FILE")" == 600 || "$(stat -c '%a' "$KUBECONFIG_FILE")" == 400 ]] || fail 'kubeconfig must have mode 0600 or 0400'
k get namespace maas-system -o json |
  jq '{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels}' >"$EVIDENCE_DIR/namespace.json"

OVERLAY="$SOURCE_DIR/deployment/base/maas-controller/overlays/xks"
[[ -f "$OVERLAY/kustomization.yaml" ]] || fail 'source MaaS xKS overlay is missing'
kustomize build --load-restrictor LoadRestrictionsNone "$OVERLAY" >"$EVIDENCE_DIR/xks-source-render.yaml"
python3 "$SCRIPT_DIR/render_kind_maas_controller_deployment.py" \
  --input "$EVIDENCE_DIR/xks-source-render.yaml" \
  --output "$EVIDENCE_DIR/locked-controller-input.yaml" \
  --run-id "$RUN_ID" \
  --controller-image "$CONTROLLER_IMAGE" \
  --api-image "$API_IMAGE" \
  --payload-image "$PAYLOAD_IMAGE" \
  >"$EVIDENCE_DIR/render-result.txt"
sha256sum "$EVIDENCE_DIR/xks-source-render.yaml" "$EVIDENCE_DIR/locked-controller-input.yaml" >"$EVIDENCE_DIR/render-hashes.txt"

install_args=(
  --context "$CONTEXT" --kubeconfig "$KUBECONFIG_FILE"
  --expected-api-server "$EXPECTED_API_SERVER"
  --namespace maas-system --monitoring-namespace ""
  --input "$EVIDENCE_DIR/locked-controller-input.yaml"
  --evidence-dir "$EVIDENCE_DIR/install"
)
[[ "$APPLY" == true ]] && install_args+=(--apply)
bash "$SCRIPT_DIR/install-kind-maas-controller.sh" "${install_args[@]}"
if [[ "$APPLY" == true ]]; then
  jq -n --arg run "$RUN_ID" --arg sourceSha "$SOURCE_SHA" --arg apiServer "$server" \
    --arg controllerImage "$CONTROLLER_IMAGE" --arg apiImage "$API_IMAGE" --arg payloadImage "$PAYLOAD_IMAGE" \
    '{status:"PASS",run:$run,sourceSha:$sourceSha,apiServer:$apiServer,namespace:"maas-system",
      controllerImage:$controllerImage,apiImage:$apiImage,payloadImage:$payloadImage,
      xksOverlay:true,serverSideDryRun:true,controllerLeaseVerified:true,secretValuesRecorded:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
else
  jq -n --arg run "$RUN_ID" --arg sourceSha "$SOURCE_SHA" --arg apiServer "$server" \
    '{status:"DRY_RUN_PASS",run:$run,sourceSha:$sourceSha,apiServer:$apiServer,namespace:"maas-system",
      xksOverlay:true,serverSideDryRun:true,resourcesChanged:false,secretValuesRecorded:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
fi
