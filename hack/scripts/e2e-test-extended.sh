#!/usr/bin/env bash
# Run the ignored qualification-tier k8s e2e suite and write report.json.
#
# Extra args after `--` are forwarded to libtest (e.g. `--nocapture`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

export E2E_EXTENDED_PROFILE="${E2E_EXTENDED_PROFILE:-maas_two_hop_fd_streamed}"
export QUALIFICATION_IDLE_SECS="${QUALIFICATION_IDLE_SECS:-300}"

STARTED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
mkdir -p target/e2e-extended/junit
# Drop any prior report so this run cannot inherit stale chain/idle results.
rm -f target/e2e-extended/report.json

CTX="${E2E_CONTEXT:-kind-praxis-e2e}"
NS="${E2E_GATEWAY_NS:-istio-system}"
SVC="${E2E_GATEWAY_SVC:-e2e-gateway-istio}"
PORT="${E2E_PORT:-18080}"

reachable() { curl -s -o /dev/null --max-time 5 "$1" 2>/dev/null; }

# Preserve a caller-supplied GATEWAY_URL; otherwise resolve LB or localhost.
if [[ -z "${GATEWAY_URL:-}" ]]; then
  LB_IP="$(kubectl --context "$CTX" -n "$NS" get svc "$SVC" \
    -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"
  if [[ -n "$LB_IP" ]] && reachable "http://${LB_IP}/"; then
    export GATEWAY_URL="http://${LB_IP}"
    echo "e2e-extended: using LoadBalancer IP ${GATEWAY_URL}"
  else
    export GATEWAY_URL="http://127.0.0.1:${PORT}"
    echo "e2e-extended: using ${GATEWAY_URL} (port-forward via baseline harness if needed)"
  fi
else
  echo "e2e-extended: using caller GATEWAY_URL=${GATEWAY_URL}"
fi

EXTRA_ARGS=("$@")
# Makefile / callers may pass a leading `--` before libtest flags.
if [[ ${#EXTRA_ARGS[@]} -gt 0 && "${EXTRA_ARGS[0]}" == "--" ]]; then
  EXTRA_ARGS=("${EXTRA_ARGS[@]:1}")
fi

set +e
if reachable "${GATEWAY_URL}/"; then
  cargo test --features k8s-e2e --test k8s_e2e -- extended --ignored --test-threads=1 "${EXTRA_ARGS[@]}"
  STATUS=$?
else
  # Baseline harness owns port-forward cleanup when LB/localhost is unreachable.
  bash hack/scripts/e2e-test.sh -- extended --ignored --test-threads=1 "${EXTRA_ARGS[@]}"
  STATUS=$?
fi
set -e

FINISHED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
OUTCOME="pass"
[[ "$STATUS" -eq 0 ]] || OUTCOME="fail"

REPORT_CHAIN_ARGS=()
if [[ -f target/e2e-extended/report.json ]]; then
  if python3 -c 'import json,sys; d=json.load(open("target/e2e-extended/report.json")); sys.exit(0 if d.get("chain")=="executed" else 1)' 2>/dev/null; then
    REPORT_CHAIN_ARGS=(--chain executed)
  fi
fi

bash hack/e2e-extended-report.sh \
  --profile "${E2E_EXTENDED_PROFILE}" \
  --started-at "${STARTED_AT}" \
  --finished-at "${FINISHED_AT}" \
  --outcome "${OUTCOME}" \
  "${REPORT_CHAIN_ARGS[@]}"

exit "$STATUS"
