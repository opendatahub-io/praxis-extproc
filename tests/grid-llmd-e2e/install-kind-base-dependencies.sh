#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install shared Kind dependencies in the order required by MaaS/KServe.
set -Eeuo pipefail
umask 077

CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
ISTIOCTL=""
EVIDENCE_DIR=""
TIMEOUT="5m"
FAILURE_CAPTURED=false

readonly GATEWAY_API_VERSION="v1.5.1"
readonly GATEWAY_API_SHA256="751002b3b91a87f7ae3bd2517c79a47a8d7ed6702901808a1cf9bd97d284f9b8"
readonly CERT_MANAGER_VERSION="v1.15.3"
readonly CERT_MANAGER_CHART_SHA256="016e68d9f7083d2c4fd302f951ee6490dbf4cb1ef44cfc06914c39cbfb01d858"
readonly ISTIO_VERSION="1.28.0"
readonly ISTIOCTL_SHA256="f33302401eea94c4da8f4502979887c75fdbe06d3bf23d38545b8de7f2d057db"
# Stay on the CEL-capable 1.4 line while taking its latest stable patch. Kuadrant
# 1.4.7 ships WASM shim 0.12.4, including the documented cold-start race fix.
# The prior 1.4.2 run returned one fail-closed HTTP 500 with a Kuadrant WASM gRPC
# error; this patch-level upgrade is a targeted hypothesis to test, not a proven
# root-cause fix.
readonly KUADRANT_VERSION="1.4.7"
readonly KUADRANT_CHART_SHA256="121f7d05463102b6d9f5c08f0ca324e090847467fa29353c92fcca16b733ddb8"
readonly GATEWAY_API_URL="https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.5.1/standard-install.yaml"

usage() {
  cat <<'USAGE'
Usage: install-kind-base-dependencies.sh --context CTX --kubeconfig PRIVATE-FILE \
  --expected-api-server URL --istioctl PATH --evidence-dir NEW-DIR [--timeout 5m]

Installs Gateway API CRDs, cert-manager, Istio, then Kuadrant/Authorino. Istio
must precede Kuadrant so its operator discovers an accepted GatewayClass at
startup; this avoids a manual Kuadrant restart. Versions and artifact digests
are pinned here. The caller must provision the run-owned Kind cluster/registry.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --istioctl) ISTIOCTL="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --timeout) TIMEOUT="${2:?}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { printf 'install-kind-base-dependencies: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$EVIDENCE_DIR/run.log"; }
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
h() { command helm --kubeconfig "$KUBECONFIG_FILE" --kube-context "$CONTEXT" "$@"; }

capture_failure() {
  local status="$1"
  ((status != 0)) || return 0
  [[ "$FAILURE_CAPTURED" == true ]] && return 0
  FAILURE_CAPTURED=true
  set +e
  k get --raw=/readyz >"$EVIDENCE_DIR/failure-readyz.txt" 2>&1
  k get nodes -o wide >"$EVIDENCE_DIR/failure-nodes.txt" 2>&1
  k get pods -A -o wide >"$EVIDENCE_DIR/failure-pods.txt" 2>&1
  k get deployments,statefulsets,services,endpointslices -A -o yaml >"$EVIDENCE_DIR/failure-workloads.yaml" 2>&1
  k get crd -o name >"$EVIDENCE_DIR/failure-crds.txt" 2>&1
  k get events -A --sort-by=.lastTimestamp >"$EVIDENCE_DIR/failure-events.txt" 2>&1
  h list --all -A >"$EVIDENCE_DIR/failure-helm-releases.txt" 2>&1
  k logs deployment/istiod -n istio-system --all-containers=true --tail=150 >"$EVIDENCE_DIR/failure-istiod-logs.txt" 2>&1
  k logs deployment/kuadrant-operator-controller-manager -n kuadrant-system --all-containers=true --tail=150 \
    >"$EVIDENCE_DIR/failure-kuadrant-logs.txt" 2>&1
  printf '%s\n' "$status" >"$EVIDENCE_DIR/failure-exit-status.txt"
  set -e
  return "$status"
}

[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" && -x "$ISTIOCTL" && -n "$EVIDENCE_DIR" ]] || {
  printf 'explicit context, private kubeconfig, expected API server, executable istioctl, and evidence directory are required\n' >&2
  exit 2
}
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists; refusing overwrite\n' >&2; exit 2; }
[[ "$TIMEOUT" =~ ^[0-9]+[smh]$ ]] || { printf 'timeout must be a duration such as 5m\n' >&2; exit 2; }
mkdir -p "$(dirname "$EVIDENCE_DIR")"
mkdir -m 0700 "$EVIDENCE_DIR"
mkdir -m 0700 "$EVIDENCE_DIR/charts"
trap 'status=$?; capture_failure "$status"' EXIT

for binary in kubectl helm curl sha256sum jq python3 date seq sleep; do
  command -v "$binary" >/dev/null || fail "$binary is required"
done
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || fail 'context API server does not match the expected Kind API'
k get --raw=/readyz >"$EVIDENCE_DIR/readyz-before.txt"
grep -qx ok "$EVIDENCE_DIR/readyz-before.txt" || fail 'Kubernetes API is not ready'
printf 'context=%s\napi_server=%s\n' "$CONTEXT" "$server" >"$EVIDENCE_DIR/cluster-identity.txt"

istio_version="$("$ISTIOCTL" version --remote=false 2>&1)"
printf '%s\n' "$istio_version" >"$EVIDENCE_DIR/istioctl-version.txt"
grep -qx 'client version: 1.28.0' "$EVIDENCE_DIR/istioctl-version.txt" || fail 'istioctl must be version 1.28.0'
actual_istioctl_sha="$(sha256sum "$ISTIOCTL" | awk '{print $1}')"
[[ "$actual_istioctl_sha" == "$ISTIOCTL_SHA256" ]] || fail 'istioctl binary digest does not match the pinned 1.28.0 artifact'
sha256sum "$ISTIOCTL" >"$EVIDENCE_DIR/istioctl-sha256.txt"

log "Installing Gateway API $GATEWAY_API_VERSION"
curl --fail --location --silent --show-error --max-time 90 "$GATEWAY_API_URL" -o "$EVIDENCE_DIR/gateway-api.yaml"
actual_gateway_api_sha="$(sha256sum "$EVIDENCE_DIR/gateway-api.yaml" | awk '{print $1}')"
[[ "$actual_gateway_api_sha" == "$GATEWAY_API_SHA256" ]] || fail 'Gateway API manifest digest changed from the pinned artifact'
sha256sum "$EVIDENCE_DIR/gateway-api.yaml" >"$EVIDENCE_DIR/gateway-api-sha256.txt"
k apply -f "$EVIDENCE_DIR/gateway-api.yaml" >"$EVIDENCE_DIR/gateway-api-apply.txt"
for crd in gatewayclasses.gateway.networking.k8s.io gateways.gateway.networking.k8s.io httproutes.gateway.networking.k8s.io referencegrants.gateway.networking.k8s.io; do
  k wait --for=condition=Established "crd/$crd" --timeout="$TIMEOUT" >>"$EVIDENCE_DIR/gateway-api-crd-waits.txt"
done

log "Installing cert-manager $CERT_MANAGER_VERSION"
h repo add jetstack https://charts.jetstack.io --force-update >"$EVIDENCE_DIR/helm-repo-jetstack.txt"
h repo update jetstack >"$EVIDENCE_DIR/helm-repo-update-jetstack.txt"
h pull jetstack/cert-manager --version "$CERT_MANAGER_VERSION" --destination "$EVIDENCE_DIR/charts" >"$EVIDENCE_DIR/cert-manager-chart-pull.txt"
CERT_MANAGER_CHART="$EVIDENCE_DIR/charts/cert-manager-$CERT_MANAGER_VERSION.tgz"
actual_cert_chart_sha="$(sha256sum "$CERT_MANAGER_CHART" | awk '{print $1}')"
[[ "$actual_cert_chart_sha" == "$CERT_MANAGER_CHART_SHA256" ]] || fail 'cert-manager chart digest changed from the pinned artifact'
sha256sum "$CERT_MANAGER_CHART" >"$EVIDENCE_DIR/cert-manager-chart-sha256.txt"
h upgrade --install cert-manager "$CERT_MANAGER_CHART" --namespace cert-manager --create-namespace \
  --set crds.enabled=true --wait --timeout "$TIMEOUT" >"$EVIDENCE_DIR/cert-manager-install.txt" 2>&1
for deployment in cert-manager cert-manager-cainjector cert-manager-webhook; do
  k rollout status "deployment/$deployment" -n cert-manager --timeout="$TIMEOUT" >>"$EVIDENCE_DIR/cert-manager-rollouts.txt"
done
k get deployments -n cert-manager -o json >"$EVIDENCE_DIR/cert-manager-deployments.json"
jq -e '(.items|length)==3 and all(.items[];(.status.observedGeneration//0)>=(.metadata.generation//0) and (.status.availableReplicas//0)==(.spec.replicas//1))' \
  "$EVIDENCE_DIR/cert-manager-deployments.json" >/dev/null || fail 'cert-manager deployments are not all current and Available'

log "Installing Istio $ISTIO_VERSION before Kuadrant"
KUBECONFIG="$KUBECONFIG_FILE" KUBE_CONTEXT="$CONTEXT" ISTIOCTL="$ISTIOCTL" \
  KIND_ISTIO_EVIDENCE_DIR="$EVIDENCE_DIR/istio" \
  "$(dirname "${BASH_SOURCE[0]}")/install-kind-istio.sh" >"$EVIDENCE_DIR/istio-helper-caller.log" 2>&1
k get gatewayclass istio -o json >"$EVIDENCE_DIR/gatewayclass-istio.json"
jq -e 'any(.status.conditions[]?;.type=="Accepted" and .status=="True")' \
  "$EVIDENCE_DIR/gatewayclass-istio.json" >/dev/null || fail 'Istio GatewayClass is not Accepted'

log "Installing Kuadrant $KUADRANT_VERSION after Gateway API provider readiness"
h repo add kuadrant https://kuadrant.io/helm-charts/ --force-update >"$EVIDENCE_DIR/helm-repo-kuadrant.txt"
h repo update kuadrant >"$EVIDENCE_DIR/helm-repo-update-kuadrant.txt"
h pull kuadrant/kuadrant-operator --version "$KUADRANT_VERSION" --destination "$EVIDENCE_DIR/charts" >"$EVIDENCE_DIR/kuadrant-chart-pull.txt"
KUADRANT_CHART="$EVIDENCE_DIR/charts/kuadrant-operator-$KUADRANT_VERSION.tgz"
actual_kuadrant_chart_sha="$(sha256sum "$KUADRANT_CHART" | awk '{print $1}')"
[[ "$actual_kuadrant_chart_sha" == "$KUADRANT_CHART_SHA256" ]] || fail 'Kuadrant chart digest changed from the pinned artifact'
sha256sum "$KUADRANT_CHART" >"$EVIDENCE_DIR/kuadrant-chart-sha256.txt"
h upgrade --install kuadrant-operator "$KUADRANT_CHART" --namespace kuadrant-system --create-namespace \
  --wait --timeout "$TIMEOUT" >"$EVIDENCE_DIR/kuadrant-operator-install.txt" 2>&1
k wait --for=condition=Established crd/kuadrants.kuadrant.io --timeout="$TIMEOUT" >"$EVIDENCE_DIR/kuadrant-crd-wait.txt"
cat >"$EVIDENCE_DIR/kuadrant-instance.yaml" <<'EOF'
apiVersion: kuadrant.io/v1beta1
kind: Kuadrant
metadata:
  name: kuadrant
  namespace: kuadrant-system
spec: {}
EOF
sha256sum "$EVIDENCE_DIR/kuadrant-instance.yaml" >"$EVIDENCE_DIR/kuadrant-instance-sha256.txt"
k apply -f "$EVIDENCE_DIR/kuadrant-instance.yaml" >"$EVIDENCE_DIR/kuadrant-instance-apply.txt"
k wait kuadrant/kuadrant -n kuadrant-system --for=condition=Ready=True --timeout="$TIMEOUT" >"$EVIDENCE_DIR/kuadrant-ready.txt"
mapfile -t kuadrant_deployments < <(k get deployments -n kuadrant-system -o name)
for deployment in "${kuadrant_deployments[@]}"; do
  k rollout status "$deployment" -n kuadrant-system --timeout="$TIMEOUT" >>"$EVIDENCE_DIR/kuadrant-rollouts.txt"
done
k get deployments -n kuadrant-system -o json >"$EVIDENCE_DIR/kuadrant-deployments.json"
jq -e '(.items|length)>0 and all(.items[];(.status.observedGeneration//0)>=(.metadata.generation//0) and (.status.availableReplicas//0)==(.spec.replicas//1))' \
  "$EVIDENCE_DIR/kuadrant-deployments.json" >/dev/null || fail 'Kuadrant and managed authorization deployments are not current and Available'
k get pods -n kuadrant-system -o wide >"$EVIDENCE_DIR/kuadrant-pods.txt"
k get services,endpointslices -n kuadrant-system -o wide >"$EVIDENCE_DIR/kuadrant-services-endpoints.txt"
k get authorino authorino -n kuadrant-system -o json >"$EVIDENCE_DIR/authorino.json"
jq -e 'any(.status.conditions[]?;.type=="Ready" and .status=="True")' "$EVIDENCE_DIR/authorino.json" >/dev/null || fail 'Authorino is not Ready'

: >"$EVIDENCE_DIR/pending-polls.tsv"
pending_clear=false
for attempt in $(seq 1 60); do
  k get pods -A --field-selector=status.phase=Pending -o json >"$EVIDENCE_DIR/pending-pods.json"
  pending_count="$(jq '.items|length' "$EVIDENCE_DIR/pending-pods.json")"
  printf '%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$attempt" "$pending_count" >>"$EVIDENCE_DIR/pending-polls.tsv"
  if [[ "$pending_count" == 0 ]]; then
    pending_clear=true
    break
  fi
  sleep 2
done
[[ "$pending_clear" == true ]] || fail 'Pending Pods remain after the bounded dependency convergence wait'
k get pods -A -o wide >"$EVIDENCE_DIR/pods-final.txt"
k get --raw=/readyz >"$EVIDENCE_DIR/readyz-final.txt"
grep -qx ok "$EVIDENCE_DIR/readyz-final.txt" || fail 'Kubernetes API readiness failed after dependency install'
h list --all -A >"$EVIDENCE_DIR/helm-releases.txt"
df -h / >"$EVIDENCE_DIR/disk-final.txt"
jq -n --arg apiServer "$server" --arg gatewayApi "$GATEWAY_API_VERSION" --arg certManager "$CERT_MANAGER_VERSION" \
  --arg istio "$ISTIO_VERSION" --arg kuadrant "$KUADRANT_VERSION" \
  '{status:"PASS",apiServer:$apiServer,installOrder:["Gateway API","cert-manager","Istio","Kuadrant/Authorino"],
    gatewayApiVersion:$gatewayApi,certManagerVersion:$certManager,istioVersion:$istio,kuadrantVersion:$kuadrant,
    kuadrantReady:true,authorinoReady:true,gatewayClassAccepted:true,pendingPods:0}' >"$EVIDENCE_DIR/RESULT.json"
log 'Gateway API, cert-manager, Istio, Kuadrant, and Authorino are ready with zero Pending Pods'
