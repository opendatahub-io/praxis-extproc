#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOOK="$SCRIPT_DIR/epp-metrics-hook.sh"
TMP="$(mktemp -d)"
cleanup() {
  if [[ -f "$TMP/forward-pids" ]]; then
    while read -r pid; do
      [[ "$pid" =~ ^[0-9]+$ ]] && kill "$pid" 2>/dev/null || true
    done <"$TMP/forward-pids"
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
mkdir -m 0700 "$TMP/bin"

cat >"$TMP/bin/oc" <<'FAKE_OC'
#!/usr/bin/env bash
set -euo pipefail
args="$*"
case "$args" in
  *"whoami --show-server"*) printf '%s\n' "$EXPECTED_API_SERVER" ;;
  *"get --raw=/readyz"*) printf 'ok\n' ;;
  *"config view"*) printf '{"clusters":[{"cluster":{}}]}\n' ;;
  *"get namespace"*) printf '{"metadata":{"labels":{"praxis.fast/qualification-run":"test-run"}}}\n' ;;
  *"create token"*) printf 'test-token-not-recorded\n' ;;
  *"get service"*) printf '{"spec":{"ports":[{"port":9090}]}}\n' ;;
  *"port-forward"*)
    printf '%s\n' "$$" >>"$FAKE_PORT_FORWARD_PIDS"
    exec sleep 60
    ;;
  *) printf 'unexpected fake oc invocation\n' >&2; exit 2 ;;
esac
FAKE_OC
chmod 0700 "$TMP/bin/oc"

cat >"$TMP/bin/curl" <<'FAKE_CURL'
#!/usr/bin/env bash
set -euo pipefail
output=""
while (($#)); do
  if [[ "$1" == -o && $# -ge 2 ]]; then
    output="$2"
    shift 2
  else
    shift
  fi
done
[[ -n "$output" ]] || { printf 'fake curl did not receive an output file\n' >&2; exit 2; }
pool="${output##*/}"
pool="${pool%-epp-metrics-sample.prom}"
printf 'llm_d_epp_average_queue_size{name="%s"} 0\n' "$pool" >"$output"
FAKE_CURL
chmod 0700 "$TMP/bin/curl"

export PATH="$TMP/bin:$PATH"
export EXPECTED_API_SERVER=https://api.test.invalid:6443
export FAKE_PORT_FORWARD_PIDS="$TMP/forward-pids"

bash "$HOOK" \
  --context test-context \
  --namespace test-run \
  --metrics-service-account metrics-scraper \
  --metrics-service-account-namespace operator-system \
  --metric-name llm_d_epp_average_queue_size \
  --service pool-a-inference-pool=test-run/pool-a-epp-service:9090 \
  --service pool-b-inference-pool=test-run/pool-b-epp-service:9090 \
  --evidence-dir "$TMP/evidence" \
  --phase sample >"$TMP/hook-output.json"

[[ "$(wc -l <"$FAKE_PORT_FORWARD_PIDS")" -eq 2 ]] || {
  printf 'expected one port-forward per EPP service\n' >&2
  exit 1
}
while read -r pid; do
  if kill -0 "$pid" 2>/dev/null; then
    printf 'port-forward process %s survived the metrics hook\n' "$pid" >&2
    exit 1
  fi
done <"$FAKE_PORT_FORWARD_PIDS"
jq -e '.values["pool-a-inference-pool"]==0 and .values["pool-b-inference-pool"]==0' \
  "$TMP/evidence/epp-metrics-sample.json" >/dev/null
printf 'EPP metrics port-forward cleanup passed\n'
