#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Verify the MaaS controller-owned API; never create or patch its operands.
set -Eeuo pipefail
umask 077
CONTEXT=""; KCFG=""; API=""; RUN=""; IMAGE=""; EVIDENCE=""; TIMEOUT=300
while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KCFG="${2:?}"; shift 2 ;;
    --expected-api-server) API="${2:?}"; shift 2 ;;
    --run-id) RUN="${2:?}"; shift 2 ;;
    --image) IMAGE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE="${2:?}"; shift 2 ;;
    --timeout-seconds) TIMEOUT="${2:?}"; shift 2 ;;
    --help|-h) echo 'Usage: verify-kind-maas-api.sh --context CTX --kubeconfig FILE --expected-api-server URL --run-id ID --image REPO@sha256:DIGEST --evidence-dir NEW-DIR [--timeout-seconds N]'; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
fail() { local msg="$*"; if declare -F capture_failure >/dev/null && [[ -d "${EVIDENCE:-}" ]]; then capture_failure "$msg"; fi; echo "verify-kind-maas-api: $msg" >&2; exit 2; }
[[ -n "$CONTEXT" && -f "$KCFG" && -n "$API" && -n "$RUN" && -n "$EVIDENCE" ]] || fail 'explicit API identity, kubeconfig, run ID, and evidence path are required'
[[ "$IMAGE" =~ ^[A-Za-z0-9._:/-]+@sha256:[0-9a-f]{64}$ ]] || fail 'MaaS API image must be pinned by digest'
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ && ! -e "$EVIDENCE" ]] || fail 'timeout must be positive and evidence directory new'
[[ "$(stat -c '%a' "$KCFG")" == 600 || "$(stat -c '%a' "$KCFG")" == 400 ]] || fail 'kubeconfig mode must be 0600 or 0400'
mkdir -p "$(dirname "$EVIDENCE")"; mkdir -m 0700 "$EVIDENCE"
k() { kubectl --kubeconfig="$KCFG" --context="$CONTEXT" --request-timeout=20s "$@"; }
capture_failure() {
  local reason="$1"
  set +e
  k get config default -n maas-system -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,owners:.metadata.ownerReferences},spec:.spec,status:.status}' >"$EVIDENCE/config-failure.json" 2>&1
  k get deployment maas-api -n maas-system -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,owners:.metadata.ownerReferences},spec:{replicas:.spec.replicas,images:[.spec.template.spec.containers[]|{name,image}]},status:.status}' >"$EVIDENCE/deployment-failure.json" 2>&1
  k get pods -n maas-system -l app.kubernetes.io/name=maas-api -o json | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,phase:.status.phase,conditions:.status.conditions,containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount,state,lastState}]}]' >"$EVIDENCE/pods-failure.json" 2>&1
  k get service maas-api -n maas-system -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,owners:.metadata.ownerReferences},spec:{selector:.spec.selector,ports:.spec.ports}}' >"$EVIDENCE/service-failure.json" 2>&1
  k get endpointslices -n maas-system -l kubernetes.io/service-name=maas-api -o json | jq '[.items[]|{name:.metadata.name,ports:.ports,endpoints:[.endpoints[]?|{conditions,targetRef}]}]' >"$EVIDENCE/endpoints-failure.json" 2>&1
  k get httproute maas-api-route -n maas-system -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,owners:.metadata.ownerReferences},spec:.spec,status:.status}' >"$EVIDENCE/route-failure.json" 2>&1
  k get events -n maas-system --sort-by=.lastTimestamp -o json | jq '[.items[-80:][]|{type,reason,message,involvedObject,lastTimestamp,count}]' >"$EVIDENCE/events-failure.json" 2>&1
  jq -n --arg run "$RUN" --arg reason "$reason" --arg image "$IMAGE" '{status:"FAILED",run:$run,reason:$reason,expectedImage:$image,secretValuesRecorded:false}' >"$EVIDENCE/RESULT.json"
}
on_error() { local code="$1" line="$2"; trap - ERR; capture_failure "command failed with exit $code at line $line"; exit "$code"; }
trap 'code=$?; on_error "$code" "$LINENO"' ERR
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$API" && "$(k get --raw=/readyz)" == *ok* ]] || fail 'API server identity or /readyz check failed'
k get namespace maas-system -o json >"$EVIDENCE/namespace.json"
jq -e --arg run "$RUN" '.metadata.labels["praxis.kind/run"]==$run' "$EVIDENCE/namespace.json" >/dev/null || fail 'maas-system is not owned by this run'
k get config default -n maas-system -o json >"$EVIDENCE/config.json"
config_uid="$(jq -er '.metadata.uid' "$EVIDENCE/config.json")"
k get service maas-api -n maas-system -o json >"$EVIDENCE/service.json"
jq -e --arg uid "$config_uid" 'any(.metadata.ownerReferences[]?;.kind=="Config" and .name=="default" and .uid==$uid)' "$EVIDENCE/service.json" >/dev/null || fail 'MaaS API Service is not owned by the live Config/default'
deadline=$((SECONDS + TIMEOUT)); passed=false
while (( SECONDS < deadline )); do
  if k get deployment maas-api -n maas-system -o json >"$EVIDENCE/deployment-live.json" 2>/dev/null && \
     k get pods -n maas-system -l app.kubernetes.io/name=maas-api -o json >"$EVIDENCE/pods-live.json" 2>/dev/null && \
     k get endpointslices -n maas-system -l kubernetes.io/service-name=maas-api -o json >"$EVIDENCE/endpoints-live.json" 2>/dev/null; then
    if jq -e --arg uid "$config_uid" --arg image "$IMAGE" '.metadata.generation as $g | any(.metadata.ownerReferences[]?;.kind=="Config" and .name=="default" and .uid==$uid) and .status.observedGeneration==$g and (.spec.replicas//1)==1 and .status.readyReplicas==1 and .status.availableReplicas==1 and ([.spec.template.spec.containers[]|select(.name=="maas-api" and .image==$image)]|length)==1' "$EVIDENCE/deployment-live.json" >/dev/null && \
       jq -e --arg digest "${IMAGE##*@}" '[.items[].status.containerStatuses[]?|select(.name=="maas-api" and .ready==true and (.imageID|endswith("@"+$digest)))]|length==1' "$EVIDENCE/pods-live.json" >/dev/null && \
       jq -e 'any(.items[]?;.ports|any(.[]?;.port==8443)) and ([.items[].endpoints[]?|select(.conditions.ready==true)]|length)==1' "$EVIDENCE/endpoints-live.json" >/dev/null; then
      passed=true
      break
    fi
  fi
  sleep 2
done
[[ "$passed" == true ]] || fail "controller-owned MaaS API did not become Ready with its pinned runtime image within ${TIMEOUT}s"
k get httproute maas-api-route -n maas-system -o json >"$EVIDENCE/route.json"
jq -e --arg uid "$config_uid" --arg gateway maas-default-gateway '
  any(.metadata.ownerReferences[]?;.kind=="Config" and .name=="default" and .uid==$uid) and
  any(.spec.parentRefs[]?;.name==$gateway) and
  any(.status.parents[]?.conditions[]?;.type=="Accepted" and .status=="True") and
  any(.status.parents[]?.conditions[]?;.type=="ResolvedRefs" and .status=="True")
' "$EVIDENCE/route.json" >/dev/null || fail 'controller-owned MaaS API HTTPRoute is not accepted/resolved by the default Gateway'
jq -n --arg run "$RUN" --arg image "$IMAGE" --arg configUID "$config_uid" --arg deploymentUID "$(jq -r '.metadata.uid' "$EVIDENCE/deployment-live.json")" \
  '{status:"PASS",run:$run,owner:"MaaS controller via Config/default",configUID:$configUID,deploymentUID:$deploymentUID,image:$image,currentGeneration:true,oneReadyReplica:true,digestMatchedImageID:true,oneReadyServiceEndpoint:true,routeAcceptedAndResolved:true,secretValuesRecorded:false}' >"$EVIDENCE/RESULT.json"
echo 'MaaS controller-owned API and route passed current-generation, owner, imageID, endpoint, and route gates'
