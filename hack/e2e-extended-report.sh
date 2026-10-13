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
mkdir -p target/e2e-extended

ENVOY_VERSION="$(kubectl --context "$CTX" -n istio-system get deploy -l app=istiod -o jsonpath='{.items[0].spec.template.spec.containers[0].image}' 2>/dev/null || echo unknown)"
ISTIO_VERSION="$(kubectl --context "$CTX" -n istio-system get pods -l app=istiod -o jsonpath='{.items[0].status.containerStatuses[0].imageID}' 2>/dev/null || echo unknown)"
IMAGE_DIGEST="${PRAXIS_EXTPROC_IMAGE_DIGEST:-unknown}"

python3 - "$PROFILE" "$STARTED_AT" "$FINISHED_AT" "$OUTCOME" "$ENVOY_VERSION" "$ISTIO_VERSION" "$IMAGE_DIGEST" "$CHAIN" "$CHAIN_REASON" <<'PY'
import json, sys
from pathlib import Path
profile, started, finished, outcome, envoy, istio, digest, chain, chain_reason = sys.argv[1:10]
hops = 2 if profile == "maas_two_hop_fd_streamed" else 0
path = Path("target/e2e-extended/report.json")
idle_results = []
tls_mode = "pending_idle_tls_tranche"
tls_neg = None
if path.exists():
    try:
        prior = json.loads(path.read_text())
        if prior.get("chain") == "executed":
            chain = "executed"
            chain_reason = prior.get("chain_reason", chain_reason)
        idle_results = prior.get("idle_matrix_results", [])
        # Preserve TLS fields written by idle_tls tests; do not clobber.
        if prior.get("tls_validation_mode"):
            tls_mode = prior["tls_validation_mode"]
        if "tls_negative_scenario" in prior:
            tls_neg = prior.get("tls_negative_scenario")
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
  "tls_validation_mode": tls_mode,
  "tls_negative_scenario": tls_neg,
  "ext_proc_modes": {"request": "FULL_DUPLEX_STREAMED", "response": "FULL_DUPLEX_STREAMED"},
  "idle_matrix_results": idle_results,
  "started_at": started,
  "finished_at": finished,
  "outcome": outcome,
}
path.write_text(json.dumps(report, indent=2) + "\n")
print(f"wrote {path}")
PY
