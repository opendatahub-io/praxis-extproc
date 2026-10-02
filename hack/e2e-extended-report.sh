#!/usr/bin/env bash
# Merge env / cluster versions into target/e2e-extended/report.json.
set -euo pipefail

PROFILE="${E2E_EXTENDED_PROFILE:-maas_two_hop_fd_streamed}"
STARTED_AT=""
FINISHED_AT=""
OUTCOME="pass"
CHAIN="${E2E_EXTENDED_CHAIN:-deferred}"
CHAIN_REASON="${E2E_EXTENDED_CHAIN_REASON:-harness tranche; chained scenarios not yet executed}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --profile) PROFILE="$2"; shift 2 ;;
    --started-at) STARTED_AT="$2"; shift 2 ;;
    --finished-at) FINISHED_AT="$2"; shift 2 ;;
    --outcome) OUTCOME="$2"; shift 2 ;;
    --chain) CHAIN="$2"; shift 2 ;;
    --chain-reason) CHAIN_REASON="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

CTX="${E2E_CONTEXT:-kind-praxis-e2e}"
NS="${E2E_GATEWAY_NS:-istio-system}"
GW_DEPLOY="${E2E_GATEWAY_DEPLOY:-e2e-gateway-istio}"
mkdir -p target/e2e-extended

# Control-plane Istio version (image tag on istiod), not the image ID digest.
ISTIO_VERSION="$(
  kubectl --context "$CTX" -n istio-system get deploy istiod \
    -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null \
    | awk -F: '{print $NF}' || true
)"
[[ -n "${ISTIO_VERSION:-}" ]] || ISTIO_VERSION="unknown"

# Gateway Envoy version from the data-plane admin server_info endpoint.
ENVOY_VERSION="$(
  kubectl --context "$CTX" -n "$NS" exec "deploy/${GW_DEPLOY}" -c istio-proxy -- \
    curl -s --max-time 5 localhost:15000/server_info 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin).get("version","unknown"))' 2>/dev/null \
    || true
)"
[[ -n "${ENVOY_VERSION:-}" ]] || ENVOY_VERSION="unknown"

IMAGE_DIGEST="${PRAXIS_EXTPROC_IMAGE_DIGEST:-unknown}"

python3 - "$PROFILE" "$STARTED_AT" "$FINISHED_AT" "$OUTCOME" "$ENVOY_VERSION" "$ISTIO_VERSION" "$IMAGE_DIGEST" "$CHAIN" "$CHAIN_REASON" <<'PY'
import json, sys
from pathlib import Path
profile, started, finished, outcome, envoy, istio, digest, chain, chain_reason = sys.argv[1:10]
hops = 2 if profile == "maas_two_hop_fd_streamed" else 0
path = Path("target/e2e-extended/report.json")
idle_results = []
if path.exists():
    try:
        prior = json.loads(path.read_text())
        if prior.get("chain") == "executed":
            chain = "executed"
            chain_reason = prior.get("chain_reason", chain_reason)
        idle_results = prior.get("idle_matrix_results", [])
    except json.JSONDecodeError:
        pass
report = {
  "profile": profile,
  "chain": chain,
  "chain_reason": chain_reason,
  "ext_proc_hops": hops,
  "envoy_version": envoy,
  "istio_version": istio,
  "praxis_extproc_image_digest": digest,
  "tls_validation_mode": "pending_idle_tls_tranche",
  "tls_negative_scenario": None,
  "ext_proc_modes": {"request": "FULL_DUPLEX_STREAMED", "response": "FULL_DUPLEX_STREAMED"},
  "idle_matrix_results": idle_results,
  "started_at": started,
  "finished_at": finished,
  "outcome": outcome,
}
path.write_text(json.dumps(report, indent=2) + "\n")
print(f"wrote {path}")
PY
