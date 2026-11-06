#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
RUN_ID=""
EVIDENCE_DIR=""

usage() {
  cat <<'USAGE'
Usage: create-kind-platform-namespaces.sh --context CTX --kubeconfig PRIVATE-FILE \
  --expected-api-server URL --run-id ID --evidence-dir NEW-DIR

Creates the fresh run-owned MaaS/AIGC namespaces required by the Kind lane.
It refuses to adopt existing namespaces and records only namespace metadata.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="$2"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="$2"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="$2"; shift 2 ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { printf 'create-kind-platform-namespaces: %s\n' "$*" >&2; exit 2; }
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" ]] || fail 'explicit context, kubeconfig, and expected API server are required'
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || fail 'run ID is not a valid ownership label'
[[ -n "$EVIDENCE_DIR" && ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory must be new'
[[ "$(stat -c '%a' "$KUBECONFIG_FILE")" == 600 || "$(stat -c '%a' "$KUBECONFIG_FILE")" == 400 ]] || fail 'kubeconfig must have mode 0600 or 0400'
for binary in kubectl jq; do command -v "$binary" >/dev/null || fail "$binary is required"; done

mkdir -p "$(dirname "$EVIDENCE_DIR")"
mkdir -m 0700 "$EVIDENCE_DIR"
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || fail 'selected context API server differs from the expected API server'
[[ "$(k get --raw=/readyz)" == *ok* ]] || fail 'Kubernetes API is not ready'

namespaces=(maas-system opendatahub ai-tenants models-as-a-service)
for namespace in "${namespaces[@]}"; do
  if k get namespace "$namespace" -o name >"$EVIDENCE_DIR/$namespace-existing.txt" 2>"$EVIDENCE_DIR/$namespace-error.txt"; then
    fail "refusing to adopt pre-existing namespace $namespace"
  fi
  grep -qi 'not found' "$EVIDENCE_DIR/$namespace-error.txt" || fail "could not verify absence of namespace $namespace"
done

for namespace in "${namespaces[@]}"; do
  manifest="$EVIDENCE_DIR/$namespace.yaml"
  cat >"$manifest" <<YAML
apiVersion: v1
kind: Namespace
metadata:
  name: $namespace
  labels:
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.kind/run: "$RUN_ID"
    istio-injection: enabled
YAML
  if [[ "$namespace" == maas-system ]]; then
    printf '    local-env.opendatahub.io/gateway-tenant: models-as-a-service\n' >>"$manifest"
  fi
  chmod 0600 "$manifest"
  k create --dry-run=server -f "$manifest" >"$EVIDENCE_DIR/$namespace-dry-run.txt"
done

for namespace in "${namespaces[@]}"; do
  k create -f "$EVIDENCE_DIR/$namespace.yaml" >"$EVIDENCE_DIR/$namespace-create.txt"
  k get namespace "$namespace" -o json \
    | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,creationTimestamp:.metadata.creationTimestamp}}' \
    >"$EVIDENCE_DIR/$namespace.json"
  jq -e --arg run "$RUN_ID" --arg name "$namespace" '
    .metadata.name==$name and
    .metadata.labels["praxis.kind/run"]==$run and
    .metadata.labels["app.kubernetes.io/managed-by"]=="grid-llmd-kind" and
    .metadata.labels["istio-injection"]=="enabled" and
    (if $name=="maas-system" then .metadata.labels["local-env.opendatahub.io/gateway-tenant"]=="models-as-a-service" else true end)
  ' "$EVIDENCE_DIR/$namespace.json" >/dev/null || fail "namespace $namespace ownership or injection labels did not persist"
done

jq -n --arg run "$RUN_ID" --arg context "$CONTEXT" --arg apiServer "$server" \
  --argjson namespaces "$(printf '%s\n' "${namespaces[@]}" | jq -R . | jq -s .)" \
  '{status:"PASS",runId:$run,context:$context,apiServer:$apiServer,namespaces:$namespaces,existingObjectsAdopted:false,secretValuesRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'Created four run-owned Kind MaaS/AIGC namespaces; Istio injection and MaaS Gateway tenant labels verified\n'
