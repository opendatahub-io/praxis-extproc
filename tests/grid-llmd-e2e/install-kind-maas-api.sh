#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Wait for the MaaS controller to install its own Kind API operands.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT=""; KUBECONFIG_FILE=""; EXPECTED_API_SERVER=""; SOURCE_DIR=""; SOURCE_SHA=""
RUN_ID=""; IMAGE=""; NAMESPACE="maas-system"; EVIDENCE_DIR=""; TIMEOUT_SECONDS=300
while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --source-dir) SOURCE_DIR="${2:?}"; shift 2 ;;
    --source-sha) SOURCE_SHA="${2:?}"; shift 2 ;;
    --run-id) RUN_ID="${2:?}"; shift 2 ;;
    --api-image) IMAGE="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --timeout-seconds) TIMEOUT_SECONDS="${2:?}"; shift 2 ;;
    --help|-h)
      printf 'Usage: install-kind-maas-api.sh --context CTX --kubeconfig FILE --expected-api-server URL --source-dir DIR --source-sha SHA --run-id ID --api-image REPO@sha256:DIGEST --evidence-dir NEW-DIR [--namespace NS] [--timeout-seconds N]\n'
      exit 0
      ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done

fail() { printf 'install-kind-maas-api: %s\n' "$*" >&2; exit 2; }
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" ]] || fail 'explicit context, private kubeconfig, and expected API server are required'
[[ -d "$SOURCE_DIR" && "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ ]] || fail 'locked MaaS source worktree and full source SHA are required'
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || fail 'run ID is not a valid ownership label'
[[ "$IMAGE" =~ ^[A-Za-z0-9._:/-]+@sha256:[0-9a-f]{64}$ ]] || fail 'API image must be an immutable SHA-256 reference'
[[ "$NAMESPACE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || fail 'namespace is invalid'
[[ "$TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || fail 'timeout must be a positive integer'
[[ -n "$EVIDENCE_DIR" && ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory must be new'
[[ "$(stat -c '%a' "$KUBECONFIG_FILE")" == 600 || "$(stat -c '%a' "$KUBECONFIG_FILE")" == 400 ]] || fail 'kubeconfig must have mode 0600 or 0400'
for binary in kubectl jq git sha256sum; do command -v "$binary" >/dev/null || fail "$binary is required"; done

mkdir -p "$(dirname "$EVIDENCE_DIR")"
mkdir -m 0700 "$EVIDENCE_DIR"
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" --request-timeout=20s "$@"; }
capture_failure() {
  local reason="$1"
  set +e
  k get gateway maas-default-gateway -n "$NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},status:.status}' >"$EVIDENCE_DIR/failure-gateway.json" 2>&1
  k get config default -n "$NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},status:.status}' >"$EVIDENCE_DIR/failure-config.json" 2>&1
  k get deployment maas-api -n "$NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,ownerReferences:.metadata.ownerReferences},spec:{replicas:.spec.replicas,containers:[.spec.template.spec.containers[]|{name,image}]},status:.status}' >"$EVIDENCE_DIR/failure-deployment.json" 2>&1
  k get pods -n "$NAMESPACE" -l app.kubernetes.io/name=maas-api -o json |
    jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,phase:.status.phase,conditions:.status.conditions,containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,state,lastState}]}]' >"$EVIDENCE_DIR/failure-pods.json" 2>&1
  k get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json |
    jq '[.items[-80:][]|{type,reason,message,involvedObject,lastTimestamp,count}]' >"$EVIDENCE_DIR/failure-events.json" 2>&1
  jq -n --arg run "$RUN_ID" --arg reason "$reason" --arg sourceSha "$SOURCE_SHA" --arg image "$IMAGE" \
    '{status:"FAILED",run:$run,reason:$reason,sourceSha:$sourceSha,expectedImage:$image,mutatedControllerOperands:false,secretValuesRecorded:false}' >"$EVIDENCE_DIR/RESULT.json"
  set -e
}
on_error() { local status="$1" line="$2"; trap - ERR; capture_failure "command failed with exit $status at helper line $line"; exit "$status"; }
trap 'status=$?; on_error "$status" "$LINENO"' ERR

server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || fail 'context API server differs from expected API server'
[[ "$(k get --raw=/readyz)" == *ok* ]] || fail 'Kubernetes API is not Ready'
[[ "$(git -C "$SOURCE_DIR" rev-parse HEAD)" == "$SOURCE_SHA" ]] || fail 'MaaS source HEAD differs from the run lock'
[[ -z "$(git -C "$SOURCE_DIR" branch --show-current)" ]] || fail 'MaaS source worktree must be detached'

# MaaS main selects maas-api/deploy/overlays/xks/kustomization.yaml on
# vanilla Kubernetes. The controller
# owns Deployment/maas-api and applies this source bundle after its Gateway
# prerequisite exists. The OpenShift TLS overlay is not valid to apply here:
# it carries a ConfigMap-backed service-CA volume while the xKS source replaces
# that same named volume with the cert-manager CA Secret.
XKS_OVERLAY="$SOURCE_DIR/maas-api/deploy/overlays/xks"
XKS_KUSTOMIZATION="$XKS_OVERLAY/kustomization.yaml"
[[ -f "$XKS_KUSTOMIZATION" ]] || fail 'locked MaaS xKS API overlay is missing'
grep -Fq '../../../../deployment/base/maas-api/overlays/tls' "$XKS_KUSTOMIZATION" || fail 'MaaS xKS overlay no longer includes the expected TLS base'
grep -Fq '/spec/template/spec/volumes/1' "$XKS_KUSTOMIZATION" || fail 'MaaS xKS overlay no longer replaces the OpenShift CA volume'
grep -Fq 'secretName: opendatahub-ca' "$XKS_KUSTOMIZATION" || fail 'MaaS xKS overlay does not use its source-defined CA Secret'
printf '%s\n' "$XKS_KUSTOMIZATION" >"$EVIDENCE_DIR/controller-overlay-path.txt"
sha256sum "$XKS_KUSTOMIZATION" >"$EVIDENCE_DIR/controller-overlay-sha256.txt"

k get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace.json"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run' "$EVIDENCE_DIR/namespace.json" >/dev/null || fail 'MaaS namespace is not owned by this run'
k get gateway maas-default-gateway -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gateway.json"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run and (.metadata.generation as $g | any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and any(.status.listeners[]?.conditions[]?;.type=="Programmed" and .status=="True" and .observedGeneration==$g))' \
  "$EVIDENCE_DIR/gateway.json" >/dev/null || fail 'run-owned MaaS Gateway must be current-generation Accepted and Programmed before API reconciliation'

# This verifier is read-only. It waits for Config/default reconciliation and
# proves ownership, current Deployment generation, immutable runtime imageID,
# Service endpoint, and accepted HTTPRoute. No static bundle is applied here.
"$SCRIPT_DIR/verify-kind-maas-api.sh" \
  --context "$CONTEXT" \
  --kubeconfig "$KUBECONFIG_FILE" \
  --expected-api-server "$EXPECTED_API_SERVER" \
  --run-id "$RUN_ID" \
  --image "$IMAGE" \
  --timeout-seconds "$TIMEOUT_SECONDS" \
  --evidence-dir "$EVIDENCE_DIR/controller-owned-verify"

jq -n --arg run "$RUN_ID" --arg sourceSha "$SOURCE_SHA" --arg apiServer "$server" --arg namespace "$NAMESPACE" --arg image "$IMAGE" \
  --arg overlayPath "$XKS_KUSTOMIZATION" --arg overlaySha "$(sha256sum "$XKS_KUSTOMIZATION" | awk '{print $1}')" \
  '{status:"PASS",run:$run,sourceSha:$sourceSha,apiServer:$apiServer,namespace:$namespace,image:$image,owner:"MaaS controller via Config/default",controllerOverlayPath:$overlayPath,controllerOverlaySha256:$overlaySha,controllerVerification:"controller-owned-verify/RESULT.json",controllerOperandsMutated:false,secretValuesRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'MaaS controller-owned Kind API passed its current-generation, pinned-image, endpoint, and route gate; no API operand was statically applied\n'
