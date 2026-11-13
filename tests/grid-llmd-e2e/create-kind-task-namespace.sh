#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Create the single run-owned namespace used by the Kind serving and Gateway graphs.
set -Eeuo pipefail
umask 077

MODE="${KIND_TASK_NAMESPACE_MODE:-plan}"
CONTEXT="${KUBE_CONTEXT:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
EVIDENCE_DIR="${KIND_TASK_NAMESPACE_EVIDENCE_DIR:-}"

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Create or verify one task-owned Kind namespace for the LLMInferenceServices,
pool Gateways, Grid resources, and task Gateway. The helper refuses to adopt a
namespace owned by another run and does not change platform namespaces.
Modes: plan, apply, verify.
PLAN
  exit 0
fi

[[ "$MODE" == apply || "$MODE" == verify ]] || { printf 'KIND_TASK_NAMESPACE_MODE must be plan, apply, or verify\n' >&2; exit 2; }
for binary in oc jq rg sha256sum; do command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }; done
[[ "${GRID_LLMD_CLUSTER_PLATFORM:-}" == kind && "${CLUSTER_ENV:-}" == kind ]] || {
  printf 'task namespace helper requires GRID_LLMD_CLUSTER_PLATFORM=kind and CLUSTER_ENV=kind\n' >&2; exit 2;
}
[[ -n "$CONTEXT" && -n "$EXPECTED_SERVER" && -n "$NAMESPACE" && -n "$RUN_LABEL" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, EXPECTED_API_SERVER, GRID_RESOURCE_NAMESPACE, GRID_RUN_LABEL, and KIND_TASK_NAMESPACE_EVIDENCE_DIR are required\n' >&2; exit 2;
}
[[ "$NAMESPACE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ && ${#NAMESPACE} -le 63 ]] || { printf 'invalid task namespace\n' >&2; exit 2; }
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'invalid run label\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

oc() { command oc --context="$CONTEXT" --request-timeout=20s "$@"; }
server="$(oc whoami --show-server)"
ready="$(oc get --raw=/readyz)"
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
[[ "$server" == "$EXPECTED_SERVER" && "$ready" == *ok* && "$insecure" != true ]] || {
  printf 'verified API identity/readiness guard failed\n' >&2; exit 1;
}
printf 'context=%s\nserver=%s\nreadyz=%s\napi_tls=verified\n' "$CONTEXT" "$server" "$ready" >"$EVIDENCE_DIR/api-access-summary.txt"

if oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace-before.json" 2>"$EVIDENCE_DIR/namespace-get.stderr"; then
  jq -e --arg run "$RUN_LABEL" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .metadata.labels["local-env.opendatahub.io/kind-run"]==$run and
    .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"
  ' "$EVIDENCE_DIR/namespace-before.json" >/dev/null || {
    printf 'existing namespace is not owned by this run; refusing adoption or relabeling\n' >&2; exit 1;
  }
  if [[ "$MODE" == apply ]]; then
    printf 'namespace exists with matching run ownership; verified without mutation\n' >"$EVIDENCE_DIR/action.txt"
  else
    printf 'namespace exists with matching run ownership\n' >"$EVIDENCE_DIR/action.txt"
  fi
else
  rg -qi 'notfound|not found' "$EVIDENCE_DIR/namespace-get.stderr" || {
    cat "$EVIDENCE_DIR/namespace-get.stderr" >&2
    printf 'namespace lookup failed for a reason other than NotFound\n' >&2
    exit 1
  }
  [[ "$MODE" == apply ]] || { printf 'task namespace does not exist\n' >&2; exit 1; }
  cat >"$EVIDENCE_DIR/task-namespace.yaml" <<YAML
apiVersion: v1
kind: Namespace
metadata:
  name: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-kind
    praxis.fast/qualification-run: "$RUN_LABEL"
    local-env.opendatahub.io/kind-run: "$RUN_LABEL"
YAML
  chmod 0600 "$EVIDENCE_DIR/task-namespace.yaml"
  sha256sum "$EVIDENCE_DIR/task-namespace.yaml" >"$EVIDENCE_DIR/task-namespace.sha256"
  oc apply --dry-run=server -f "$EVIDENCE_DIR/task-namespace.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
  oc create -f "$EVIDENCE_DIR/task-namespace.yaml" -o json >"$EVIDENCE_DIR/namespace-created.json"
  printf 'created new task namespace with run ownership labels\n' >"$EVIDENCE_DIR/action.txt"
fi

oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace-after.json"
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  .metadata.labels["local-env.opendatahub.io/kind-run"]==$run and
  .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"
' "$EVIDENCE_DIR/namespace-after.json" >/dev/null || {
  printf 'task namespace ownership labels did not persist\n' >&2; exit 1;
}
jq '{name:.metadata.name,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels}' \
  "$EVIDENCE_DIR/namespace-after.json" >"$EVIDENCE_DIR/namespace-identity.json"
jq -n --arg mode "$MODE" --arg name "$NAMESPACE" --arg run "$RUN_LABEL" \
  '{mode:$mode,status:"verified",namespace:$name,runLabel:$run,inferenceTrafficSent:false}' >"$EVIDENCE_DIR/RESULT.json"
printf 'task namespace %s verified for run %s\n' "$NAMESPACE" "$RUN_LABEL"
