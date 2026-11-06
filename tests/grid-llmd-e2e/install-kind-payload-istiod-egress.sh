#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
RUN_ID=""
NAMESPACE="maas-system"
EVIDENCE_DIR=""

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --run-id) RUN_ID="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --help|-h) printf 'Usage: install-kind-payload-istiod-egress.sh --context CTX --kubeconfig FILE --expected-api-server URL --run-id RUN --evidence-dir NEW-DIR [--namespace NS]\n'; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
  esac
done

for binary in kubectl jq python3; do command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }; done
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" && -n "$RUN_ID" && -n "$EVIDENCE_DIR" ]] || {
  printf 'context, private kubeconfig, expected API server, run ID, and evidence directory are required\n' >&2; exit 2;
}
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists; refusing overwrite\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"; chmod 0700 "$EVIDENCE_DIR"
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 2; }
[[ "$(k get --raw=/readyz)" == *ok* ]] || { printf 'Kubernetes API is not ready\n' >&2; exit 2; }
k get service istiod -n istio-system -o json >"$EVIDENCE_DIR/istiod-service.json"
k get endpointslices -n istio-system -l kubernetes.io/service-name=istiod -o json >"$EVIDENCE_DIR/istiod-endpointslices.json"
python3 "$SCRIPT_DIR/render_kind_payload_istiod_networkpolicy.py" \
  --service "$EVIDENCE_DIR/istiod-service.json" --endpointslices "$EVIDENCE_DIR/istiod-endpointslices.json" \
  --output "$EVIDENCE_DIR/payload-istiod-egress.yaml" --summary "$EVIDENCE_DIR/result.json" \
  --run-id "$RUN_ID" --namespace "$NAMESPACE"
k apply --dry-run=server -f "$EVIDENCE_DIR/payload-istiod-egress.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
k apply -f "$EVIDENCE_DIR/payload-istiod-egress.yaml" >"$EVIDENCE_DIR/apply.txt"
k get networkpolicy grid-llmd-kind-allow-istiod-xds -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/networkpolicy-live.json"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run and
  .spec.policyTypes==["Egress"] and
  (.spec.podSelector.matchExpressions|any(.key=="app" and .operator=="In" and
     (.values|sort)==(["payload-pre-processing","payload-processing"]|sort))) and
  (.spec.egress|length==1) and (.spec.egress[0].ports==[{"protocol":"TCP","port":15012}]) and
  (.spec.egress[0].to|length==1) and
  .spec.egress[0].to[0].namespaceSelector.matchLabels["kubernetes.io/metadata.name"]=="istio-system"' \
  "$EVIDENCE_DIR/networkpolicy-live.json" >/dev/null || { printf 'installed Istiod exception does not match the narrow xDS contract\n' >&2; exit 1; }
printf 'installed a run-owned, payload-only Istiod TCP/15012 egress exception\n'
