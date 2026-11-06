#!/usr/bin/env bash
# Run the k8s E2E suite against the main and streaming test Gateways.
#
# On docker, the Kind network is host-routable so the MetalLB LoadBalancer IP
# works directly. On rootless podman the Kind network lives in a network
# namespace and the LB IP is NOT reachable from the host, so we tunnel to the
# gateway with `kubectl port-forward` instead. This makes `make e2e-test` work
# out of the box on both engines.
#
# Extra args are forwarded to `cargo test` (e.g. `-- --nocapture`).
set -euo pipefail

CTX="${E2E_CONTEXT:-kind-praxis-e2e}"
NS="${E2E_GATEWAY_NS:-istio-system}"
SVC="${E2E_GATEWAY_SVC:-e2e-gateway-istio}"
PORT="${E2E_PORT:-18080}"
STREAMING_SVC="${E2E_STREAMING_GATEWAY_SVC:-e2e-streaming-gateway-istio}"
STREAMING_PORT="${E2E_STREAMING_PORT:-18081}"
# E2E deploys IPP into the gateway namespace (istio-system); override only when
# pointing at a different overlay (e.g. demo/odh in praxis-extproc).
EXTPROC_NS="${E2E_EXTPROC_NS:-$NS}"
IPP_DEPLOY="${E2E_IPP_DEPLOY:-payload-processing}"
METRICS_PORT="${E2E_METRICS_PORT:-19090}"

# Body mode under test (fds|buffered|streamed). Prefer an explicit export from `make
# e2e-test MODE=...`; otherwise default to fds to match Makefile.
export E2E_BODY_MODE="${E2E_BODY_MODE:-${MODE:-fds}}"
LOG_DIR="${E2E_LOG_DIR:-target/e2e-logs/${E2E_BODY_MODE}}"
mkdir -p "$LOG_DIR"

# Any HTTP response (even 404) means the endpoint is reachable.
reachable() { curl -s -o /dev/null --max-time 5 "$1" 2>/dev/null; }

PF_PID=""
STREAMING_PF_PID=""
METRICS_PF_PID=""
cleanup() {
  [[ -n "$PF_PID" ]] && kill "$PF_PID" 2>/dev/null || true
  [[ -n "$STREAMING_PF_PID" ]] && kill "$STREAMING_PF_PID" 2>/dev/null || true
  [[ -n "$METRICS_PF_PID" ]] && kill "$METRICS_PF_PID" 2>/dev/null || true
}
trap cleanup EXIT

LB_IP="$(kubectl --context "$CTX" -n "$NS" get svc "$SVC" \
  -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"

if [[ -n "$LB_IP" ]] && reachable "http://${LB_IP}/"; then
  export GATEWAY_URL="http://${LB_IP}"
  echo "e2e: using LoadBalancer IP ${GATEWAY_URL}"
else
  echo "e2e: LoadBalancer IP unreachable (rootless podman?); port-forwarding ${SVC} -> 127.0.0.1:${PORT}"
  kubectl --context "$CTX" -n "$NS" port-forward "svc/${SVC}" "${PORT}:80" >"$LOG_DIR/gateway-port-forward.log" 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 20); do
    reachable "http://127.0.0.1:${PORT}/" && break
    sleep 0.5
  done
  reachable "http://127.0.0.1:${PORT}/" || { echo "e2e: port-forward did not become ready" >&2; exit 1; }
  export GATEWAY_URL="http://127.0.0.1:${PORT}"
  echo "e2e: using ${GATEWAY_URL}"
fi

STREAMING_LB_IP="$(kubectl --context "$CTX" -n "$NS" get svc "$STREAMING_SVC" \
  -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"

if [[ -n "$STREAMING_LB_IP" ]] && reachable "http://${STREAMING_LB_IP}/"; then
  export STREAMING_GATEWAY_URL="http://${STREAMING_LB_IP}"
  echo "e2e: using streaming LoadBalancer IP ${STREAMING_GATEWAY_URL}"
else
  echo "e2e: streaming LoadBalancer IP unreachable; port-forwarding ${STREAMING_SVC} -> 127.0.0.1:${STREAMING_PORT}"
  kubectl --context "$CTX" -n "$NS" port-forward "svc/${STREAMING_SVC}" "${STREAMING_PORT}:80" >"$LOG_DIR/streaming-port-forward.log" 2>&1 &
  STREAMING_PF_PID=$!
  for _ in $(seq 1 20); do
    reachable "http://127.0.0.1:${STREAMING_PORT}/" && break
    sleep 0.5
  done
  reachable "http://127.0.0.1:${STREAMING_PORT}/" || { echo "e2e: streaming port-forward did not become ready" >&2; exit 1; }
  export STREAMING_GATEWAY_URL="http://127.0.0.1:${STREAMING_PORT}"
  echo "e2e: using ${STREAMING_GATEWAY_URL}"
fi

echo "e2e: port-forwarding ${EXTPROC_NS}/deploy/${IPP_DEPLOY} metrics -> 127.0.0.1:${METRICS_PORT}"
kubectl --context "$CTX" -n "$EXTPROC_NS" port-forward "deploy/${IPP_DEPLOY}" "${METRICS_PORT}:9090" >"$LOG_DIR/metrics-port-forward.log" 2>&1 &
METRICS_PF_PID=$!
for _ in $(seq 1 40); do
  reachable "http://127.0.0.1:${METRICS_PORT}/metrics" && break
  sleep 0.5
done
reachable "http://127.0.0.1:${METRICS_PORT}/metrics" || {
  echo "e2e: IPP metrics port-forward did not become ready (${EXTPROC_NS}/deploy/${IPP_DEPLOY}:9090)" >&2
  kubectl --context "$CTX" -n "$EXTPROC_NS" get deploy,pods -l "app=${IPP_DEPLOY}" -o wide >&2 || true
  exit 1
}
export IPP_METRICS_URL="http://127.0.0.1:${METRICS_PORT}/metrics"
# Audience stays unset: TokenReview in the extproc process checks the
# apiserver audience. This account is the scraper, not the extproc identity.
METRICS_READER_SA="${E2E_METRICS_READER_SA:-e2e-metrics-reader}"
IPP_METRICS_TOKEN="$(kubectl --context "$CTX" -n "$EXTPROC_NS" create token "${METRICS_READER_SA}")"
[[ -n "${IPP_METRICS_TOKEN}" ]] || { echo "e2e: empty metrics reader token for ${METRICS_READER_SA}" >&2; exit 1; }
export IPP_METRICS_TOKEN
echo "e2e: using ${IPP_METRICS_URL} (E2E_BODY_MODE=${E2E_BODY_MODE}, reader ${EXTPROC_NS}/${METRICS_READER_SA})"

if [[ -n "$PF_PID" || -n "$STREAMING_PF_PID" || -n "$METRICS_PF_PID" ]]; then
  # Port-forward tunnels can collapse when concurrent tests flood them. Keep
  # the whole E2E process serial when either Gateway needs a tunnel.
  : "${RUST_TEST_THREADS:=1}"
  export RUST_TEST_THREADS
  echo "e2e: serializing tests over port-forwards (RUST_TEST_THREADS=${RUST_TEST_THREADS})"
fi

cargo test --features k8s-e2e --test k8s_e2e "$@"
