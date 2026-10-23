#!/usr/bin/env bash
# Run the k8s e2e suite against the gateway, choosing a reachable GATEWAY_URL.
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

# Profile under test (conformance | integrity). Exported so the test binary can
# skip tests that do not belong to the active lane.
PROFILE="${PROFILE:-conformance}"
echo "e2e: profile=${PROFILE}"

# Gating is by module: every test lives in the `conformance::` or `integrity::`
# module, and the run selects a lane with a name filter (the profile name). A
# test's module is its profile gate — nothing to track here.
case "$PROFILE" in
  conformance|integrity) ;;
  *) echo "unknown PROFILE='$PROFILE' (expected conformance|integrity)" >&2; exit 1 ;;
esac

# Any HTTP response (even 404) means the endpoint is reachable.
reachable() { curl -s -o /dev/null --max-time 5 "$1" 2>/dev/null; }

PF_PID=""
cleanup() { [[ -n "$PF_PID" ]] && kill "$PF_PID" 2>/dev/null || true; }
trap cleanup EXIT

LB_IP="$(kubectl --context "$CTX" -n "$NS" get svc "$SVC" \
  -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"

if [[ -n "$LB_IP" ]] && reachable "http://${LB_IP}/"; then
  export GATEWAY_URL="http://${LB_IP}"
  echo "e2e: using LoadBalancer IP ${GATEWAY_URL}"
else
  echo "e2e: LoadBalancer IP unreachable (rootless podman?); port-forwarding ${SVC} -> 127.0.0.1:${PORT}"
  kubectl --context "$CTX" -n "$NS" port-forward "svc/${SVC}" "${PORT}:80" >/dev/null 2>&1 &
  PF_PID=$!
  for _ in $(seq 1 20); do
    reachable "http://127.0.0.1:${PORT}/" && break
    sleep 0.5
  done
  reachable "http://127.0.0.1:${PORT}/" || { echo "e2e: port-forward did not become ready" >&2; exit 1; }
  export GATEWAY_URL="http://127.0.0.1:${PORT}"
  echo "e2e: using ${GATEWAY_URL}"

  # A single `kubectl port-forward` tunnel collapses when the default,
  # fully-parallel test run floods it with concurrent requests (connection
  # resets, then a dead tunnel). The direct LoadBalancer path above has no such
  # bottleneck. Serialize here so the tunnel stays healthy; an explicit
  # `--test-threads` on the CLI still overrides this default.
  : "${RUST_TEST_THREADS:=1}"
  export RUST_TEST_THREADS
  echo "e2e: serializing tests over the port-forward (RUST_TEST_THREADS=${RUST_TEST_THREADS})"
fi

# The profile name is a cargo-side test-name filter (before `--`); any caller
# flags (e.g. --nocapture) are test-binary args (after `--`).
cargo test --features k8s-e2e --test k8s_e2e "$PROFILE" -- "$@"
