#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
NAMESPACE="maas-system"
MONITORING_NAMESPACE=""
INPUT=""
EVIDENCE_DIR=""
APPLY=false

usage() {
  cat <<'USAGE'
Usage: install-kind-maas-controller.sh --context CTX --kubeconfig FILE \
  --expected-api-server URL --input SOURCE-RENDERED-YAML --evidence-dir NEW-DIR \
  [--namespace NS] [--monitoring-namespace NS] [--apply]

Without --apply, validate the corrected render with server-side dry-run only.
With --apply, install it, then verify the current Deployment, Ready Pod, Lease
permissions, and a leader-election Lease held by that exact Pod. Run this before
the temporary cleanup-image override. No Secret data or credentials are recorded.
By default the Kind profile passes an empty --monitoring-namespace, which skips
optional OpenShift-specific MaaS observability resources. Set --monitoring-namespace
only when a compatible monitoring stack exists in that namespace.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --monitoring-namespace) MONITORING_NAMESPACE="$2"; shift 2 ;;
    --input) INPUT="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --apply) APPLY=true; shift ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

for binary in kubectl jq python3; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" && -f "$INPUT" && -n "$EVIDENCE_DIR" ]] || {
  printf 'context, private kubeconfig, API server, source render, and evidence directory are required\n' >&2
  exit 2
}
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists; refusing overwrite\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 2; }
[[ "$(k get --raw=/readyz)" == *ok* ]] || { printf 'Kubernetes API is not ready\n' >&2; exit 2; }
k get namespace "$NAMESPACE" -o json |
  jq '{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels}' >"$EVIDENCE_DIR/namespace.json"

python3 "$SCRIPT_DIR/render-kind-maas-controller.py" \
  --input "$INPUT" --output "$EVIDENCE_DIR/maas-controller-rendered.yaml" --namespace "$NAMESPACE" \
  --monitoring-namespace "$MONITORING_NAMESPACE" \
  >"$EVIDENCE_DIR/render-result.txt"
set +e
k apply --dry-run=server -f "$EVIDENCE_DIR/maas-controller-rendered.yaml" \
  >"$EVIDENCE_DIR/server-dry-run.txt" 2>&1
dry_run_status=$?
set -e
printf '%s\n' "$dry_run_status" >"$EVIDENCE_DIR/server-dry-run-exit-status.txt"
if ((dry_run_status != 0)); then
  printf 'MaaS controller corrected render failed server-side dry-run; no resources were applied\n' >&2
  cat "$EVIDENCE_DIR/server-dry-run.txt" >&2
  exit "$dry_run_status"
fi

if [[ "$APPLY" == true ]]; then
  k apply -f "$EVIDENCE_DIR/maas-controller-rendered.yaml" >"$EVIDENCE_DIR/apply.txt"
  k rollout status deployment/maas-controller -n "$NAMESPACE" --timeout=240s >"$EVIDENCE_DIR/rollout.txt"
  k get deployment maas-controller -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/controller-deployment.json"
  jq -e '.status.observedGeneration==.metadata.generation and
    (.status.readyReplicas//0)==(.spec.replicas//1) and
    (.status.availableReplicas//0)==(.spec.replicas//1)' \
    "$EVIDENCE_DIR/controller-deployment.json" >/dev/null || {
    printf 'MaaS controller Deployment has not observed its ready generation\n' >&2
    exit 1
  }
  pod_selector="$(jq -er '.spec.selector.matchLabels|to_entries|map("\(.key)=\(.value)")|join(",")' \
    "$EVIDENCE_DIR/controller-deployment.json")"
  k get pods -n "$NAMESPACE" -l "$pod_selector" -o json >"$EVIDENCE_DIR/controller-pods.json"
  jq -e '[.items[]|select(.metadata.deletionTimestamp==null and .status.phase=="Running" and
    any(.status.conditions[]?;.type=="Ready" and .status=="True"))]|length==1' \
    "$EVIDENCE_DIR/controller-pods.json" >/dev/null || {
    printf 'expected exactly one active Ready MaaS controller Pod\n' >&2
    exit 1
  }
  pod_name="$(jq -er '[.items[]|select(.metadata.deletionTimestamp==null and .status.phase=="Running" and
    any(.status.conditions[]?;.type=="Ready" and .status=="True"))][0].metadata.name' "$EVIDENCE_DIR/controller-pods.json")"
  k get pod "$pod_name" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/controller-pod.json"
  pod_started_at="$(jq -er '.status.containerStatuses[]|select(.name=="manager")|.state.running.startedAt' \
    "$EVIDENCE_DIR/controller-pod.json")"
  for verb in get create update patch; do
    allowed="$(k auth can-i "$verb" leases.coordination.k8s.io \
      --as="system:serviceaccount:$NAMESPACE:maas-controller" --namespace "$NAMESPACE")"
    printf '%s\t%s\n' "$verb" "$allowed" >>"$EVIDENCE_DIR/lease-rbac.txt"
    [[ "$allowed" == yes ]] || { printf 'MaaS controller cannot %s its leader-election Lease\n' "$verb" >&2; exit 1; }
  done
  lease_acquired=false
  for attempt in {1..30}; do
    if ! k get lease maas-controller.models-as-a-service.opendatahub.io -n "$NAMESPACE" -o json \
      >"$EVIDENCE_DIR/lease-current.json" 2>"$EVIDENCE_DIR/lease-get-error.txt"; then
      printf '%s\twaiting-for-lease\n' "$attempt" >>"$EVIDENCE_DIR/lease-poll.txt"
      sleep 2
      continue
    fi
    if ! k logs "$pod_name" -n "$NAMESPACE" -c manager --timestamps=true --since-time="$pod_started_at" \
      2>"$EVIDENCE_DIR/lease-log-error.txt" \
      | grep -F 'Successfully acquired lease' \
      | grep -F "lock=\"$NAMESPACE/maas-controller.models-as-a-service.opendatahub.io\"" \
      >"$EVIDENCE_DIR/lease-acquisition.txt"; then
      : >"$EVIDENCE_DIR/lease-acquisition.txt"
    fi
    if python3 "$SCRIPT_DIR/verify_kind_maas_controller_lease.py" \
      --pod "$EVIDENCE_DIR/controller-pod.json" \
      --deployment "$EVIDENCE_DIR/controller-deployment.json" \
      --lease "$EVIDENCE_DIR/lease-current.json" \
      --logs "$EVIDENCE_DIR/lease-acquisition.txt" \
      --namespace "$NAMESPACE" \
      --lease-name maas-controller.models-as-a-service.opendatahub.io \
      --output "$EVIDENCE_DIR/lease-verification.json"; then
      lease_acquired=true
      printf '%s\tacquired\n' "$attempt" >>"$EVIDENCE_DIR/lease-poll.txt"
      break
    fi
    printf '%s\twaiting\n' "$attempt" >>"$EVIDENCE_DIR/lease-poll.txt"
    sleep 2
  done
  [[ "$lease_acquired" == true ]] || {
    printf 'current MaaS controller Pod has not acquired a leader-election Lease\n' >&2
    exit 1
  }
  k get deployment,serviceaccount,role,rolebinding -n "$NAMESPACE" -o json |
    jq '{items:[.items[]|{kind:.kind,name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,observedGeneration:.status.observedGeneration}]}' \
      >"$EVIDENCE_DIR/controller-resources.json"
  printf 'corrected source render passed rollout, Ready Pod image identity, lease RBAC, and current-Pod leader-election gates\n'
else
  printf 'corrected source render passed server-side dry-run; no live resources changed\n'
fi
