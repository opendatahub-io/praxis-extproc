#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT=""; KCFG=""; API=""; AIGC=""; SHA=""; RUN=""; EVIDENCE=""; TIMEOUT=180
while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KCFG="${2:?}"; shift 2 ;;
    --expected-api-server) API="${2:?}"; shift 2 ;;
    --aigc-source-dir) AIGC="${2:?}"; shift 2 ;;
    --aigc-source-sha) SHA="${2:?}"; shift 2 ;;
    --run-id) RUN="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE="${2:?}"; shift 2 ;;
    --timeout-seconds) TIMEOUT="${2:?}"; shift 2 ;;
    --help|-h) echo 'Usage: install-kind-maas-gateway.sh --context CTX --kubeconfig FILE --expected-api-server URL --aigc-source-dir DIR --aigc-source-sha SHA --run-id ID --evidence-dir DIR'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
fail() { local reason="$*"; if declare -F capture_failure >/dev/null && [[ -d "${EVIDENCE:-}" ]]; then capture_failure "$reason"; fi; echo "install-kind-maas-gateway: $reason" >&2; exit 2; }
[[ -n "$CONTEXT" && -f "$KCFG" && -n "$API" && -d "$AIGC" && "$SHA" =~ ^[0-9a-f]{40}$ && -n "$RUN" && -n "$EVIDENCE" ]] || fail 'explicit cluster, locked AIGC source, run ID, and new evidence directory are required'
[[ ! -e "$EVIDENCE" ]] || fail 'evidence directory already exists'
[[ "$(stat -c '%a' "$KCFG")" == 600 || "$(stat -c '%a' "$KCFG")" == 400 ]] || fail 'private kubeconfig mode must be 0600 or 0400'
[[ "$(git -C "$AIGC" rev-parse HEAD)" == "$SHA" && -z "$(git -C "$AIGC" branch --show-current)" && -z "$(git -C "$AIGC" status --porcelain)" ]] || fail 'AIGC worktree is not clean at the locked detached SHA'
mkdir -p "$(dirname "$EVIDENCE")"; mkdir -m 0700 "$EVIDENCE"
k() { kubectl --kubeconfig="$KCFG" --context="$CONTEXT" --request-timeout=20s "$@"; }
capture_failure() {
  local reason="$1"
  set +e
  k get gateway maas-default-gateway -n maas-system -o json >"$EVIDENCE/gateway-failure.json" 2>&1
  k get gatewayclass istio -o json >"$EVIDENCE/gatewayclass-failure.json" 2>&1
  k get service maas-default-gateway maas-default-gateway-istio -n maas-system -o json >"$EVIDENCE/services-failure.json" 2>&1
  k get endpointslices -n maas-system -l gateway.networking.k8s.io/gateway-name=maas-default-gateway -o json >"$EVIDENCE/endpoints-failure.json" 2>&1
  k get pods -n maas-system -o json >"$EVIDENCE/pods-failure.json" 2>&1
  k get events -n maas-system --sort-by=.lastTimestamp -o json | jq '[.items[-80:][]|{type,reason,message,object:.involvedObject,lastTimestamp,count}]' >"$EVIDENCE/events-failure.json" 2>&1
  jq -n --arg run "$RUN" --arg sha "$SHA" --arg reason "$reason" '{status:"FAILED",run:$run,aigcSourceSha:$sha,reason:$reason,secretValuesRecorded:false}' >"$EVIDENCE/RESULT.json"
}
on_error() { local code="$1" line="$2"; trap - ERR; capture_failure "command failed with exit $code at line $line"; exit "$code"; }
trap 'code=$?; on_error "$code" "$LINENO"' ERR
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$API" && "$(k get --raw=/readyz)" == *ok* ]] || fail 'explicit API identity or /readyz check failed'
[[ -f "$AIGC/test/kind-env/manifests/30-gateway.yaml" ]] || fail 'locked AIGC Gateway fixture is missing'
k get namespace maas-system -o json >"$EVIDENCE/namespace.json"
jq -e --arg run "$RUN" '.metadata.labels["praxis.kind/run"]==$run' "$EVIDENCE/namespace.json" >/dev/null || fail 'maas-system is not run-owned'
k get gatewayclass istio -o json >"$EVIDENCE/gatewayclass.json"
jq -e 'any(.status.conditions[]?;.type=="Accepted" and .status=="True")' "$EVIDENCE/gatewayclass.json" >/dev/null || fail 'Istio GatewayClass is not Accepted'
MANIFEST="$AIGC/test/kind-env/manifests/30-gateway.yaml"
python3 "$SCRIPT_DIR/render_kind_maas_gateway.py" --input "$MANIFEST" --output "$EVIDENCE/gateway.yaml" --run-id "$RUN" --summary "$EVIDENCE/render-summary.json" >"$EVIDENCE/render.log"
sha256sum "$MANIFEST" "$EVIDENCE/gateway.yaml" >"$EVIDENCE/manifest-sha256.txt"
python3 - "$EVIDENCE/gateway.yaml" "$EVIDENCE/gateway.json" <<'PYJSON'
import json, sys, yaml
from pathlib import Path
Path(sys.argv[2]).write_text(json.dumps(yaml.safe_load(Path(sys.argv[1]).read_text(encoding="utf-8"))) + "\n", encoding="utf-8")
PYJSON
gateway_present=false
if k get gateway maas-default-gateway -n maas-system -o json >"$EVIDENCE/preexisting-gateway.json" 2>/dev/null; then
  gateway_present=true
  jq -e --arg run "$RUN" --slurpfile desired "$EVIDENCE/gateway.json" '.metadata.labels["praxis.kind/run"]==$run and .spec==$desired[0].spec' "$EVIDENCE/preexisting-gateway.json" >/dev/null || fail 'existing Gateway is not this run-owned exact source fixture; refusing adoption'
fi
if k get service maas-default-gateway -n maas-system -o json >"$EVIDENCE/preexisting-alias-service.json" 2>/dev/null; then
  jq -e --arg run "$RUN" --arg uid "$(jq -r '.metadata.uid' "$EVIDENCE/preexisting-gateway.json")" '.metadata.labels["praxis.kind/run"]==$run and any(.metadata.ownerReferences[]?;.kind=="Gateway" and .name=="maas-default-gateway" and .uid==$uid)' "$EVIDENCE/preexisting-alias-service.json" >/dev/null || fail 'existing alias Service is not owned by the current run Gateway UID'
fi
k apply --dry-run=server -f "$EVIDENCE/gateway.yaml" >"$EVIDENCE/server-dry-run.txt"
if [[ "$gateway_present" == false ]]; then k create -f "$EVIDENCE/gateway.yaml" >"$EVIDENCE/create.txt"; else cp "$EVIDENCE/preexisting-gateway.json" "$EVIDENCE/create.txt"; fi
deadline=$((SECONDS + TIMEOUT)); ready=false
while (( SECONDS < deadline )); do
  k get gateway maas-default-gateway -n maas-system -o json >"$EVIDENCE/gateway-live.json"
  if jq -e '.metadata.generation as $g | ([.status.conditions[]?|select(.type=="Accepted" and .status=="True" and .observedGeneration==$g)]|length)==1 and any(.status.listeners[]?.conditions[]?; .type=="Programmed" and .status=="True" and .observedGeneration==$g)' "$EVIDENCE/gateway-live.json" >/dev/null; then ready=true; break; fi
  sleep 2
done
[[ "$ready" == true ]] || fail "Gateway listener not current-generation Accepted/Programmed within ${TIMEOUT}s"
ready=false
while (( SECONDS < deadline )); do
  if k get service maas-default-gateway-istio -n maas-system -o json >"$EVIDENCE/istio-service.json" 2>/dev/null && k get endpointslices -n maas-system -l gateway.networking.k8s.io/gateway-name=maas-default-gateway -o json >"$EVIDENCE/istio-endpointslices.json"; then
    if jq -e 'any(.items[]?;.ports|any(.[]?;.port==80)) and any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$EVIDENCE/istio-endpointslices.json" >/dev/null; then ready=true; break; fi
  fi
  sleep 2
done
[[ "$ready" == true ]] || fail "Gateway Istio Service has no Ready HTTP endpoint within ${TIMEOUT}s"
# Kind has no external address controller to fulfill Istio's requested service hostname.
# AIGC's own Kind harness publishes the in-cluster generated-service hostname only
# after the listener and Ready endpoint exist; retain that supported adaptation.
gateway_host=maas-default-gateway-istio.maas-system.svc.cluster.local
jq -n --arg host "$gateway_host" '{status:{addresses:[{type:"Hostname",value:$host}]}}' >"$EVIDENCE/kind-status-address.json"
k patch gateway maas-default-gateway -n maas-system --subresource=status --type=merge --patch-file="$EVIDENCE/kind-status-address.json" >"$EVIDENCE/kind-status-address-patch.txt"
ready=false
while (( SECONDS < deadline )); do
  k get gateway maas-default-gateway -n maas-system -o json >"$EVIDENCE/gateway-live.json"
  if jq -e --arg host "$gateway_host" '.metadata.generation as $g | ([.status.conditions[]?|select(.type=="Accepted" and .status=="True" and .observedGeneration==$g)]|length)==1 and ([.status.addresses[]?|select(.type=="Hostname" and .value==$host)]|length)==1 and any(.status.listeners[]?;.name=="http" and any(.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and any(.conditions[]?;.type=="Programmed" and .status=="True" and .observedGeneration==$g))' "$EVIDENCE/gateway-live.json" >/dev/null; then ready=true; break; fi
  sleep 2
done
[[ "$ready" == true ]] || fail "Gateway lacks the source-supported Kind internal address or current listener readiness"
printf 'aigc_source_sha=%s\nfixture_sha256=%s\ngateway_uid=%s\ngateway_generation=%s\nkind_status_address=%s\n' "$SHA" "$(sha256sum "$MANIFEST" | awk '{print $1}')" "$(jq -r '.metadata.uid' "$EVIDENCE/gateway-live.json")" "$(jq -r '.metadata.generation' "$EVIDENCE/gateway-live.json")" "$gateway_host" >"$EVIDENCE/source-and-gateway-identity.txt"
KUBE_CONTEXT="$CONTEXT" KUBECONFIG="$KCFG" EXPECTED_API_SERVER="$API" E2E_RUN_ID="$RUN" KIND_MAAS_GATEWAY_EVIDENCE_DIR="$EVIDENCE/alias-service" KIND_MAAS_GATEWAY_TIMEOUT_SECONDS="$TIMEOUT" "$SCRIPT_DIR/install-kind-maas-default-gateway.sh"
k get gateway maas-default-gateway -n maas-system -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},status:{conditions:.status.conditions,addresses:.status.addresses}}' >"$EVIDENCE/gateway-summary.json"
jq -n --arg run "$RUN" --arg sha "$SHA" --arg uid "$(jq -r '.metadata.uid' "$EVIDENCE/gateway-live.json")" '{status:"PASS",run:$run,aigcSourceSha:$sha,gatewayUID:$uid,currentAccepted:true,currentHttpListenerProgrammed:true,kindInternalStatusAddress:true,generatedIstioEndpointReady:true,uidOwnedAliasServiceReady:true,secretValuesRecorded:false}' >"$EVIDENCE/RESULT.json"
echo 'run-owned MaaS Gateway and UID-bound alias passed readiness checks'
