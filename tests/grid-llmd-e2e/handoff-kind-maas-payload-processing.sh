#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
NAMESPACE="maas-system"
RUN_ID=""
EVIDENCE_DIR=""
APPLY=false

usage() {
  cat <<'USAGE'
Usage: handoff-kind-maas-payload-processing.sh --context CTX --kubeconfig FILE \
  --expected-api-server URL --run-id RUN --evidence-dir NEW-DIR [--namespace NS] [--apply]

Capture payload resources left by an earlier run-owned bootstrap apply and,
with --apply, remove only resources bearing this run's ownership label and the
bootstrap manager label. AIGC then reconciles the current Praxis-owned set.
Resources with another manager are retained. This is a development recovery
step; a clean run should find no bootstrap payload resources to remove.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --run-id) RUN_ID="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --apply) APPLY=true; shift ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

for binary in kubectl jq python3; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" && -n "$RUN_ID" && -n "$EVIDENCE_DIR" ]] || {
  printf 'context, private kubeconfig, expected API server, run ID, and evidence directory are required\n' >&2
  exit 2
}
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists; refusing overwrite\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
actual_server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$actual_server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 2; }
[[ "$(k get --raw=/readyz)" == *ok* ]] || { printf 'Kubernetes API is not ready\n' >&2; exit 2; }
k get namespace "$NAMESPACE" -o json | jq '{name:.metadata.name,uid:.metadata.uid}' >"$EVIDENCE_DIR/namespace.json"

selector="praxis.kind/run=$RUN_ID,app.kubernetes.io/component=payload-processing"
private_inventory="$(mktemp "${TMPDIR:-/tmp}/kind-payload-handoff.XXXXXXXXXX")"
chmod 0600 "$private_inventory"
trap 'rm -f "$private_inventory"' EXIT
k get deployments,services,configmaps,serviceaccounts,roles,rolebindings,clusterroles,clusterrolebindings,networkpolicies,destinationrules.networking.istio.io,envoyfilters.networking.istio.io \
  -n "$NAMESPACE" -l "$selector" -o json >"$private_inventory"
python3 - "$private_inventory" "$EVIDENCE_DIR/inventory-summary.json" <<'PY'
import json
import sys
from pathlib import Path

inventory = json.loads(Path(sys.argv[1]).read_text())
items = inventory.get("items", [])
summary = []
for item in items:
    metadata = item.get("metadata", {})
    summary.append({
        "kind": item.get("kind"),
        "name": metadata.get("name"),
        "namespace": metadata.get("namespace"),
        "uid": metadata.get("uid"),
        "generation": metadata.get("generation"),
        "manager": metadata.get("labels", {}).get("app.kubernetes.io/managed-by"),
        "owner_reference_count": len(metadata.get("ownerReferences", [])),
    })
Path(sys.argv[2]).write_text(json.dumps({"resources": summary, "count": len(summary)}, indent=2, sort_keys=True) + "\n")
PY

delete_one() {
  local kind="$1" name="$2" uid="$3" resource current
  case "$kind" in
    Deployment) resource=deployments.apps ;;
    Service) resource=services ;;
    ConfigMap) resource=configmaps ;;
    ServiceAccount) resource=serviceaccounts ;;
    Role) resource=roles.rbac.authorization.k8s.io ;;
    RoleBinding) resource=rolebindings.rbac.authorization.k8s.io ;;
    ClusterRole) resource=clusterroles.rbac.authorization.k8s.io ;;
    ClusterRoleBinding) resource=clusterrolebindings.rbac.authorization.k8s.io ;;
    NetworkPolicy) resource=networkpolicies.networking.k8s.io ;;
    DestinationRule) resource=destinationrules.networking.istio.io ;;
    EnvoyFilter) resource=envoyfilters.networking.istio.io ;;
    *) printf 'unsupported resource kind in payload handoff inventory: %s\n' "$kind" >&2; return 1 ;;
  esac
  current="$(k get "$resource" "$name" -n "$NAMESPACE" -o json)"
  jq -e --arg run "$RUN_ID" --arg uid "$uid" \
    '.metadata.uid==$uid and .metadata.labels["praxis.kind/run"]==$run and
     .metadata.labels["app.kubernetes.io/component"]=="payload-processing" and
     .metadata.labels["app.kubernetes.io/managed-by"]=="grid-llmd-kind" and
     ((.metadata.ownerReferences//[])|length)==0' <<<"$current" >/dev/null || {
    printf 'payload handoff resource changed ownership or UID; refusing deletion: %s/%s\n' "$kind" "$name" >&2
    return 1
  }
  k delete "$resource" "$name" -n "$NAMESPACE" --wait=true --timeout=90s
}

deleted=0
if [[ "$APPLY" == true ]]; then
  while IFS=$'\t' read -r kind name uid manager owners; do
    [[ -n "$kind" ]] || continue
    if [[ "$manager" != grid-llmd-kind || "$owners" != 0 ]]; then
      printf '%s\t%s\tretained\tmanager=%s\townerRefs=%s\n' "$kind" "$name" "$manager" "$owners" \
        >>"$EVIDENCE_DIR/actions.tsv"
      continue
    fi
    delete_one "$kind" "$name" "$uid"
    printf '%s\t%s\tdeleted-run-owned-bootstrap\n' "$kind" "$name" >>"$EVIDENCE_DIR/actions.tsv"
    deleted=$((deleted + 1))
  done < <(jq -r '.resources[]|[.kind,.name,.uid,.manager,(.owner_reference_count|tostring)]|@tsv' "$EVIDENCE_DIR/inventory-summary.json")
fi

k get deployments,services,configmaps,serviceaccounts,roles,rolebindings,clusterroles,clusterrolebindings,networkpolicies,destinationrules.networking.istio.io,envoyfilters.networking.istio.io \
  -n "$NAMESPACE" -l "$selector" -o json |
  jq '{items:[.items[]|{kind:.kind,name:.metadata.name,uid:.metadata.uid,manager:.metadata.labels["app.kubernetes.io/managed-by"]}]}' \
    >"$EVIDENCE_DIR/inventory-after.json"
printf '{"mode":"%s","deleted_bootstrap_resources":%s,"remaining_run_labeled_payload_resources":%s}\n' \
  "$([[ "$APPLY" == true ]] && printf apply || printf plan)" "$deleted" \
  "$(jq '.items|length' "$EVIDENCE_DIR/inventory-after.json")" >"$EVIDENCE_DIR/result.json"
