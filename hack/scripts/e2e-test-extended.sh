#!/usr/bin/env bash
# Run the ignored qualification-tier k8s e2e suite and write report.json.
#
# Extra args after an optional leading `--` are forwarded to libtest
# (e.g. `make test-e2e-extended V=1` → `--nocapture`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

export E2E_EXTENDED_PROFILE="${E2E_EXTENDED_PROFILE:-maas_two_hop_fd_streamed}"
export QUALIFICATION_IDLE_SECS="${QUALIFICATION_IDLE_SECS:-300}"

STARTED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
mkdir -p target/e2e-extended/junit
# Drop prior qualification evidence so this run cannot inherit chain/idle/TLS results.
rm -f target/e2e-extended/report.json

CTX="${E2E_CONTEXT:-kind-praxis-e2e}"
NS="${E2E_GATEWAY_NS:-istio-system}"
SVC="${E2E_GATEWAY_SVC:-e2e-gateway-istio}"
PORT="${E2E_PORT:-18080}"

# Preserve a caller-supplied GATEWAY_URL; otherwise resolve LB or localhost.
# Validate LB_IP as IPv4 before interpolating (no `source` of generated shell).
if [[ -z "${GATEWAY_URL:-}" ]]; then
  LB_IP="$(kubectl --context "$CTX" -n "$NS" get svc "$SVC" \
    -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"
  if [[ "$LB_IP" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] \
    && curl -s -o /dev/null --max-time 5 "http://${LB_IP}/" 2>/dev/null; then
    export GATEWAY_URL="http://${LB_IP}"
  else
    export GATEWAY_URL="http://127.0.0.1:${PORT}"
  fi
fi

[[ "${1:-}" == "--" ]] && shift
EXTRA_ARGS=("$@")

# libtest summary line: "test result: ok. N passed; ..."
count_passed() {
  local log="$1"
  python3 - "$log" <<'PY'
import re, sys
path = sys.argv[1]
text = open(path, encoding="utf-8", errors="replace").read()
matches = re.findall(r"test result:.*?\b(\d+)\s+passed\b", text)
print(matches[-1] if matches else "0")
PY
}

TEST_LOG="$(mktemp)"
cleanup() { rm -f "$TEST_LOG"; }
trap cleanup EXIT

set +e
if curl -s -o /dev/null --max-time 5 "${GATEWAY_URL}/" 2>/dev/null; then
  # ${arr[@]+...} keeps empty arrays safe under `set -u` on bash ≤4.3 / macOS bash 3.2.
  cargo test --features k8s-e2e --test k8s_e2e -- extended --ignored --test-threads=1 \
    ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
    2>&1 | tee "$TEST_LOG"
  STATUS=${PIPESTATUS[0]}
else
  # Baseline harness owns port-forward cleanup when the resolved URL is unreachable.
  bash hack/scripts/e2e-test.sh -- extended --ignored --test-threads=1 \
    ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"} \
    2>&1 | tee "$TEST_LOG"
  STATUS=${PIPESTATUS[0]}
fi
set -e

PASSED="$(count_passed "$TEST_LOG")"
if [[ "$STATUS" -eq 0 && "$PASSED" -eq 0 ]]; then
  echo "e2e-test-extended: zero tests executed (refusing outcome=pass)" >&2
  STATUS=1
fi

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
  ${REPORT_CHAIN_ARGS[@]+"${REPORT_CHAIN_ARGS[@]}"}

exit "$STATUS"
