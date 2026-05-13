#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-extproc}"
KUBECTL="kubectl --context kind-${CLUSTER_NAME}"
NAMESPACE="praxis-test"
GATEWAY_NAME="praxis-test"
LOCAL_PORT="18080"

# ---------------------------------------------------------------------------
# Wait for Gateway
# ---------------------------------------------------------------------------

echo "==> Waiting for Gateway to be programmed..."
for i in $(seq 1 120); do
    STATUS=$(${KUBECTL} -n "${NAMESPACE}" get gateway "${GATEWAY_NAME}" \
        -o jsonpath='{.status.conditions[?(@.type=="Programmed")].status}' \
        2>/dev/null || true)
    if [ "${STATUS}" = "True" ]; then
        echo "    Gateway programmed after ${i}s"
        break
    fi
    if [ "${i}" -eq 120 ]; then
        echo "FAIL: Gateway not programmed after 120s"
        ${KUBECTL} -n "${NAMESPACE}" describe gateway "${GATEWAY_NAME}"
        exit 1
    fi
    sleep 1
done

# ---------------------------------------------------------------------------
# Port-Forward
# ---------------------------------------------------------------------------

echo "==> Setting up port-forward to Gateway..."
GW_DEPLOY=$(${KUBECTL} -n "${NAMESPACE}" get deploy \
    -l gateway.networking.k8s.io/gateway-name="${GATEWAY_NAME}" \
    -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)

if [ -z "${GW_DEPLOY}" ]; then
    echo "FAIL: no Gateway deployment found"
    ${KUBECTL} -n "${NAMESPACE}" get deploy --show-labels
    exit 1
fi

${KUBECTL} -n "${NAMESPACE}" port-forward "deploy/${GW_DEPLOY}" "${LOCAL_PORT}:8080" &
PF_PID=$!
trap "kill ${PF_PID} 2>/dev/null || true" EXIT
sleep 3

# ---------------------------------------------------------------------------
# Traffic Test
# ---------------------------------------------------------------------------

echo "==> Sending traffic through Gateway..."
RESPONSE_HEADERS=$(mktemp)
RESPONSE_BODY=$(curl -s -D "${RESPONSE_HEADERS}" --max-time 10 \
    "http://localhost:${LOCAL_PORT}/" || true)

echo "    Body: ${RESPONSE_BODY}"

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

PASS=true

if grep -q "^HTTP/.* 200" "${RESPONSE_HEADERS}"; then
    echo "PASS: traffic routed to echo backend (200 OK)"
else
    echo "FAIL: expected 200 OK from echo backend"
    PASS=false
fi

if grep -qi "x-praxis: true" "${RESPONSE_HEADERS}"; then
    echo "PASS: ext_proc filter applied (X-Praxis header present)"
else
    echo "FAIL: X-Praxis header not found in response"
    PASS=false
fi

# ---------------------------------------------------------------------------
# Debug Output on Failure
# ---------------------------------------------------------------------------

if [ "${PASS}" != "true" ]; then
    echo ""
    echo "==> Debug info:"
    echo "--- Response Headers ---"
    cat "${RESPONSE_HEADERS}"
    echo "--- praxis-extproc logs ---"
    ${KUBECTL} -n praxis-extproc logs deployment/praxis-extproc \
        --tail=50 2>/dev/null || true
    echo "--- Gateway Envoy logs ---"
    ${KUBECTL} -n "${NAMESPACE}" logs "deploy/${GW_DEPLOY}" \
        --tail=50 2>/dev/null || true
    echo "--- Pods ---"
    ${KUBECTL} -n "${NAMESPACE}" get pods -o wide
    ${KUBECTL} -n praxis-extproc get pods -o wide
    rm -f "${RESPONSE_HEADERS}"
    exit 1
fi

rm -f "${RESPONSE_HEADERS}"
echo "==> Smoke test passed."
