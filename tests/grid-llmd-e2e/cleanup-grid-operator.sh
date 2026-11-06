#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Delete task Grid CRs while their operator is live, then uninstall the release.
set -Eeuo pipefail
umask 077
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE="${GRID_OPERATOR_CLEANUP_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
RELEASE_NAME="${GRID_OPERATOR_RELEASE_NAME:-grid-operator}"
RELEASE_NAMESPACE="${GRID_OPERATOR_NAMESPACE:-}"
DEPLOYMENT_NAME="${GRID_OPERATOR_DEPLOYMENT_NAME:-grid-operator}"
RESOURCE_NAMESPACE="${GRID_RESOURCE_NAMESPACE:-}"
NETWORK_NAME="${GRID_NETWORK_NAME:-}"
SITE_NAME="${GRID_SITE_NAME:-}"
OPERATOR_IMAGE="${GRID_OPERATOR_IMAGE:-}"
WAIT_TIMEOUT="${GRID_OPERATOR_CLEANUP_TIMEOUT:-300s}"
EVIDENCE_DIR="${GRID_OPERATOR_CLEANUP_EVIDENCE_DIR:-}"
RESUME_FROM="${GRID_OPERATOR_CLEANUP_RESUME_FROM:-}"

usage() {
  cat <<'USAGE'
Delete the run-owned GridNetwork/GridSite with the Grid operator available,
wait for the GridNetwork withdrawal finalizer to complete, then uninstall Helm.

Modes: plan, cleanup, resume (set GRID_OPERATOR_CLEANUP_MODE).
Required for cleanup: EXPECTED_API_SERVER, KUBE_CONTEXT, GRID_RUN_LABEL,
GRID_OPERATOR_NAMESPACE, GRID_RESOURCE_NAMESPACE, GRID_NETWORK_NAME,
GRID_SITE_NAME, GRID_OPERATOR_IMAGE (repository@sha256 digest), and a fresh
GRID_OPERATOR_CLEANUP_EVIDENCE_DIR.

API TLS bypass requires CLUSTER_ENV=ephemeral-dev and
ALLOW_INSECURE_DEV_API=true. This script never removes a finalizer manually.
Resume requires GRID_OPERATOR_CLEANUP_RESUME_FROM pointing to the private evidence
directory from an attempt that submitted the UID-preconditioned GridNetwork delete.
It leaves the task resource namespace for the outer run cleanup.
USAGE
}

if [[ "$MODE" == plan ]]; then
  usage
  cat <<'PLAN'
Cleanup order:
  1. Verify API identity/readiness, run-owned namespaces/release/CRs, and a ready operator.
  2. Refuse cleanup while any provider references the task GridNetwork.
  3. Delete the GridNetwork using its UID precondition and wait for its withdrawal finalizer.
  4. Require the operator log to report published SWIM withdrawal and confirm the operator remains Ready.
  5. Delete the GridSite, then uninstall the Helm release and remove its run-owned namespace.
  6. Verify the release and task Grid CRs are absent while Grid CRDs remain.
PLAN
  exit 0
fi

[[ "$MODE" == cleanup || "$MODE" == resume ]] || { printf 'unsupported cleanup mode: %s\n' "$MODE" >&2; exit 2; }
for bin in oc helm jq rg curl cp; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" &&
   -n "$RELEASE_NAMESPACE" && -n "$RESOURCE_NAMESPACE" && -n "$NETWORK_NAME" &&
   -n "$SITE_NAME" && -n "$OPERATOR_IMAGE" && -n "$EVIDENCE_DIR" ]] || {
  printf 'required cleanup identity, resource, image, and evidence inputs are missing\n' >&2
  exit 2
}
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'invalid GRID_RUN_LABEL\n' >&2; exit 2; }
[[ "$OPERATOR_IMAGE" =~ ^.+@sha256:[0-9a-f]{64}$ ]] || { printf 'GRID_OPERATOR_IMAGE must use an immutable sha256 digest\n' >&2; exit 2; }
[[ "$WAIT_TIMEOUT" =~ ^[1-9][0-9]{0,3}s$ ]] || { printf 'GRID_OPERATOR_CLEANUP_TIMEOUT must be 1s..9999s\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
if [[ "$MODE" == resume ]]; then
  [[ -n "$RESUME_FROM" && -d "$RESUME_FROM" ]] || { printf 'resume mode requires an existing GRID_OPERATOR_CLEANUP_RESUME_FROM directory\n' >&2; exit 2; }
  for prior_file in gridnetwork-before.json gridnetwork-delete-response.json.summary.json; do
    [[ -s "$RESUME_FROM/$prior_file" ]] || { printf 'resume evidence is missing %s\n' "$prior_file" >&2; exit 2; }
  done
  [[ -s "$RESUME_FROM/gridnetwork-delete-options.json" || -s "$RESUME_FROM/delete-options.json" ]] ||
    { printf 'resume evidence is missing the GridNetwork UID-preconditioned DeleteOptions\n' >&2; exit 2; }
fi
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
if [[ "$MODE" == resume ]]; then
  mkdir -m 0700 "$EVIDENCE_DIR/prior-attempt"
  for prior_file in gridnetwork-before.json gridnetwork-delete-response.json gridnetwork-delete-response.json.http-status \
    gridnetwork-delete-response.json.summary.json gridnetwork-delete-options.json delete-options.json operator-deployment-before.json \
    operator-deployment-immediately-before-network-delete.json operator-deployment-after-network.json \
    operator-pods-before.json operator-pods-immediately-before-network-delete.json cleanup-identity.txt \
    gridnetwork-finalizer-watch.jsonl gridnetwork-after.json; do
    [[ -e "$RESUME_FROM/$prior_file" ]] && cp -- "$RESUME_FROM/$prior_file" "$EVIDENCE_DIR/prior-attempt/$prior_file"
  done
fi

oc() { command oc --context="$KUBE_CONTEXT" "$@"; }
helm() { command helm --kube-context="$KUBE_CONTEXT" "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
fail() { log "BLOCKED: $*"; exit 1; }
API_PROXY_PID=""
API_PROXY_PORT=""
stop_api_proxy() {
  if [[ -n "$API_PROXY_PID" ]]; then
    kill "$API_PROXY_PID" 2>/dev/null || true
    wait "$API_PROXY_PID" 2>/dev/null || true
    API_PROXY_PID=""
    API_PROXY_PORT=""
  fi
}
trap stop_api_proxy EXIT

start_api_proxy() {
  [[ -n "$API_PROXY_PID" ]] && return 0
  command oc --context="$KUBE_CONTEXT" proxy --address=127.0.0.1 --port=0 >"$EVIDENCE_DIR/api-proxy.log" 2>&1 &
  API_PROXY_PID=$!
  for _ in $(seq 1 50); do
    API_PROXY_PORT="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "$EVIDENCE_DIR/api-proxy.log" | head -n 1)"
    if [[ -n "$API_PROXY_PORT" ]] && curl -fsS --max-time 1 "http://127.0.0.1:$API_PROXY_PORT/readyz" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  fail 'loopback API proxy did not become ready for UID-preconditioned deletion'
}

delete_resource_with_uid() {
  local api_path uid output options_output http_status
  api_path="$1"
  uid="$2"
  output="$3"
  options_output="$4"
  start_api_proxy
  jq -n --arg uid "$uid" '{apiVersion:"v1",kind:"DeleteOptions",preconditions:{uid:$uid}}' \
    >"$options_output"
  http_status="$(curl -sS --max-time 30 -o "$output" -w '%{http_code}' \
    -H 'Content-Type: application/json' -X DELETE \
    --data-binary "@$options_output" \
    "http://127.0.0.1:$API_PROXY_PORT$api_path")" || fail "UID-preconditioned DELETE failed for $api_path"
  [[ "$http_status" == 200 || "$http_status" == 202 ]] || {
    printf '%s\n' "$http_status" >"$output.http-status"
    fail "UID-preconditioned DELETE returned HTTP $http_status for $api_path"
  }
  printf '%s\n' "$http_status" >"$output.http-status"
  jq '{kind,apiVersion,metadata:{name:.metadata.name,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp,finalizers:.metadata.finalizers},status:.status}' \
    "$output" >"$output.summary.json"
  jq -e --arg uid "$uid" '
    if .metadata.uid != null then .metadata.uid==$uid
    else .kind=="Status" and .status=="Success"
    end' "$output.summary.json" >/dev/null ||
    fail "UID-preconditioned DELETE response did not identify the expected object UID for $api_path"
}

require_absent() {
  local kind="$1" name="$2" namespace="$3" observed
  local -a namespace_args=()
  [[ -z "$namespace" ]] || namespace_args=(-n "$namespace")
  observed="$(oc get "$kind" "$name" "${namespace_args[@]}" --ignore-not-found -o name)" ||
    fail "could not verify absence of $kind/$name"
  [[ -z "$observed" ]] || fail "$kind/$name still exists"
}

api_ready() {
  local observed insecure ready
  observed="$(oc whoami --show-server)"
  [[ "$observed" == "$EXPECTED_API_SERVER" ]] || fail "API server mismatch: $observed"
  ready="$(oc get --raw=/readyz)"
  [[ "$ready" == *ok* ]] || fail 'API /readyz is not ok'
  insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
  if [[ "$insecure" == true ]]; then
    [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || fail 'insecure API kubeconfig requires explicit ephemeral-dev opt-in'
  fi
  printf 'capturedAt=%s\nserver=%s\ncontext=%s\nreadyz=%s\napiTlsMode=%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$observed" "$KUBE_CONTEXT" "$ready" \
    "$(if [[ "$insecure" == true ]]; then printf explicit-ephemeral-dev-unverified; else printf kubeconfig-verified; fi)" \
    >"$EVIDENCE_DIR/api-access.txt"
  chmod 0600 "$EVIDENCE_DIR/api-access.txt"
}

api_ready
[[ ! -e "$EVIDENCE_DIR/RUN-LOG.md" ]] || :
log "cleanup begins for $RELEASE_NAMESPACE/$RELEASE_NAME run=$RUN_LABEL"

helm status "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/helm-status-before.json" || fail 'Grid operator Helm release is absent'
jq -e '.info.status=="deployed"' "$EVIDENCE_DIR/helm-status-before.json" >/dev/null || fail 'Grid operator Helm release is not deployed'

oc get namespace "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-namespace-before.json" || fail 'operator namespace is absent'
oc get namespace "$RESOURCE_NAMESPACE" -o json >"$EVIDENCE_DIR/resource-namespace-before.json" || fail 'task resource namespace is absent'
for file in "$EVIDENCE_DIR/operator-namespace-before.json" "$EVIDENCE_DIR/resource-namespace-before.json"; do
  jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$file" >/dev/null || fail "namespace ownership check failed for $file"
done
NAMESPACE_UID="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/operator-namespace-before.json")"

await_operator_ready() {
  local prefix="$1" deadline raw_deployment raw_pods deployment_ready pod_ready
  deadline=$((SECONDS + ${WAIT_TIMEOUT%s}))
  [[ -e "$EVIDENCE_DIR/operator-readiness-attempts.jsonl" ]] || : >"$EVIDENCE_DIR/operator-readiness-attempts.jsonl"
  while ((SECONDS < deadline)); do
    raw_deployment="$EVIDENCE_DIR/operator-deployment-${prefix}.raw.json"
    raw_pods="$EVIDENCE_DIR/operator-pods-${prefix}.raw.json"
    if oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$raw_deployment" 2>/dev/null &&
       oc get pods -n "$RELEASE_NAMESPACE" -l app.kubernetes.io/instance="$RELEASE_NAME" -o json >"$raw_pods" 2>/dev/null; then
      jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,images:[.spec.template.spec.containers[].image]},status:{observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
        "$raw_deployment" >"$EVIDENCE_DIR/operator-deployment-${prefix}.json"
      jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp,phase:.status.phase,specContainers:[.spec.containers[]?|{name,image}],containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]' \
        "$raw_pods" >"$EVIDENCE_DIR/operator-pods-${prefix}.json"
      deployment_ready=false
      pod_ready=false
      jq -e --arg run "$RUN_LABEL" --arg image "$OPERATOR_IMAGE" \
        '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.replicas==1 and .spec.images==[$image] and
         .metadata.generation==.status.observedGeneration and (.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1 and
         any(.status.conditions[]?;.type=="Available" and .status=="True")' \
        "$EVIDENCE_DIR/operator-deployment-${prefix}.json" >/dev/null 2>&1 && deployment_ready=true
      jq -e --arg image "$OPERATOR_IMAGE" --arg digest "${OPERATOR_IMAGE##*@}" \
        -f "$SCRIPT_DIR/single-ready-pinned-pod.jq" \
        "$EVIDENCE_DIR/operator-pods-${prefix}.json" >/dev/null 2>&1 && pod_ready=true
      jq -cn --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson deploymentReady "$deployment_ready" \
        --argjson podReady "$pod_ready" --arg generation "$(jq -r '.metadata.generation' "$EVIDENCE_DIR/operator-deployment-${prefix}.json")" \
        --arg observedGeneration "$(jq -r '.status.observedGeneration // ""' "$EVIDENCE_DIR/operator-deployment-${prefix}.json")" \
        --arg readyReplicas "$(jq -r '.status.readyReplicas // 0' "$EVIDENCE_DIR/operator-deployment-${prefix}.json")" \
        '{observedAt:$observedAt,deploymentReady:$deploymentReady,podReady:$podReady,generation:$generation,observedGeneration:$observedGeneration,readyReplicas:$readyReplicas}' \
        >>"$EVIDENCE_DIR/operator-readiness-attempts.jsonl"
      if [[ "$deployment_ready" == true && "$pod_ready" == true ]]; then
        DEPLOYMENT_UID="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/operator-deployment-${prefix}.json")"
        OPERATOR_POD="$(jq -r '.[0].name' "$EVIDENCE_DIR/operator-pods-${prefix}.json")"
        OPERATOR_POD_UID="$(jq -r '.[0].uid' "$EVIDENCE_DIR/operator-pods-${prefix}.json")"
        return 0
      fi
    fi
    sleep 2
  done
  oc get events -n "$RELEASE_NAMESPACE" --sort-by=.lastTimestamp -o json 2>/dev/null |
    jq '[.items[-80:][]|{lastTimestamp:.lastTimestamp,type:.type,reason:.reason,message:.message,involvedObject:.involvedObject}]' \
    >"$EVIDENCE_DIR/operator-readiness-timeout-events.json" || true
  OPERATOR_POD="$(jq -r '.[0].name // empty' "$EVIDENCE_DIR/operator-pods-${prefix}.json" 2>/dev/null || true)"
  if [[ -n "$OPERATOR_POD" ]]; then
    oc describe pod "$OPERATOR_POD" -n "$RELEASE_NAMESPACE" >"$EVIDENCE_DIR/operator-readiness-timeout-describe.txt" 2>&1 || true
    oc logs "$OPERATOR_POD" -n "$RELEASE_NAMESPACE" --all-containers=true --tail=300 2>&1 |
      rg -i 'warn|error|restart|identity|gridnetwork|swim|tls|secret' >"$EVIDENCE_DIR/operator-readiness-timeout-logs.txt" || true
    oc logs "$OPERATOR_POD" -n "$RELEASE_NAMESPACE" --all-containers=true --previous --tail=300 2>&1 |
      rg -i 'warn|error|restart|identity|gridnetwork|swim|tls|secret' >"$EVIDENCE_DIR/operator-readiness-timeout-previous-logs.txt" || true
  fi
  fail "operator failed to converge Ready with the pinned image within $WAIT_TIMEOUT; diagnostics were captured"
}

await_operator_ready before

if oc get inferenceproviders.grid.praxis.fast -o json >"$EVIDENCE_DIR/providers-before.json" 2>/dev/null; then
  jq --arg run "$RUN_LABEL" --arg network "$NETWORK_NAME" \
    '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run or .spec.gridNetworkRef==$network)|{name:.metadata.name,uid:.metadata.uid,run:.metadata.labels["praxis.fast/qualification-run"],gridNetworkRef:.spec.gridNetworkRef}]' \
    "$EVIDENCE_DIR/providers-before.json" >"$EVIDENCE_DIR/providers-blocking-cleanup.json"
  jq -e 'length==0' "$EVIDENCE_DIR/providers-blocking-cleanup.json" >/dev/null || fail 'providers remain for this run or reference the task GridNetwork; remove providers before Grid resources'
else
  fail 'could not list providers; absence is not established'
fi
rm -f "$EVIDENCE_DIR/providers-before.json"

if [[ "$MODE" == resume ]]; then
  prior_network="$EVIDENCE_DIR/prior-attempt/gridnetwork-before.json"
  prior_response="$EVIDENCE_DIR/prior-attempt/gridnetwork-delete-response.json.summary.json"
  prior_delete_options="$EVIDENCE_DIR/prior-attempt/gridnetwork-delete-options.json"
  [[ -s "$prior_delete_options" ]] || prior_delete_options="$EVIDENCE_DIR/prior-attempt/delete-options.json"
  jq -e --arg name "$NETWORK_NAME" --arg run "$RUN_LABEL" --arg release "$RELEASE_NAME" \
    '.metadata.name==$name and .metadata.labels["praxis.fast/qualification-run"]==$run and
     .metadata.labels["app.kubernetes.io/instance"]==$release and
     (.metadata.finalizers|index("grid.praxis.fast/gridnetwork-withdrawal"))!=null and
     (.spec.gridId|type)=="string" and (.spec.gridId|length)>0' "$prior_network" >/dev/null ||
    fail 'resume evidence does not identify this run-owned GridNetwork with its withdrawal finalizer'
  NETWORK_UID="$(jq -r '.metadata.uid' "$prior_network")"
  GRID_ID="$(jq -r '.spec.gridId' "$prior_network")"
  jq -e --arg uid "$NETWORK_UID" '.metadata.uid==$uid and .metadata.deletionTimestamp!=null' "$prior_response" >/dev/null ||
    fail 'prior UID-preconditioned delete response does not match the saved GridNetwork UID'
  jq -e --arg uid "$NETWORK_UID" '.preconditions.uid==$uid' "$prior_delete_options" >/dev/null ||
    fail 'prior GridNetwork DeleteOptions did not contain the saved UID precondition'
  oc get gridnetwork "$NETWORK_NAME" -o json >"$EVIDENCE_DIR/gridnetwork-after-resume.raw.json" 2>/dev/null &&
    fail 'GridNetwork still exists; resume mode will not submit a second delete'
  require_absent gridnetwork "$NETWORK_NAME" ""
  printf 'NotFound\n' >"$EVIDENCE_DIR/gridnetwork-after-resume.txt"
else
  oc get gridnetwork "$NETWORK_NAME" -o json >"$EVIDENCE_DIR/gridnetwork-before.raw.json" || fail 'task GridNetwork is absent'
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,annotations:.metadata.annotations,finalizers:.metadata.finalizers,deletionTimestamp:.metadata.deletionTimestamp},spec:{gridId:.spec.gridId,membership:.spec.membership,signalTransport:.spec.signalTransport,consumerGateways:.spec.consumerGateways,tls:.spec.tls},status:{phase:.status.phase,observedGeneration:.status.observedGeneration,conditions:.status.conditions}}' \
    "$EVIDENCE_DIR/gridnetwork-before.raw.json" >"$EVIDENCE_DIR/gridnetwork-before.json"
  rm "$EVIDENCE_DIR/gridnetwork-before.raw.json"
  jq -e --arg run "$RUN_LABEL" --arg release "$RELEASE_NAME" \
    '.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.labels["app.kubernetes.io/instance"]==$release and
     (.metadata.finalizers|type)=="array" and (.metadata.finalizers|index("grid.praxis.fast/gridnetwork-withdrawal"))!=null and
     (.spec.gridId|type)=="string" and (.spec.gridId|length)>0' "$EVIDENCE_DIR/gridnetwork-before.json" >/dev/null ||
    fail 'GridNetwork ownership or withdrawal-finalizer precondition failed'
  NETWORK_UID="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/gridnetwork-before.json")"
  GRID_ID="$(jq -r '.spec.gridId' "$EVIDENCE_DIR/gridnetwork-before.json")"
fi

oc get gridsite "$SITE_NAME" -o json >"$EVIDENCE_DIR/gridsite-before.raw.json" || fail 'task GridSite is absent'
jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,annotations:.metadata.annotations,finalizers:.metadata.finalizers,deletionTimestamp:.metadata.deletionTimestamp},spec:{gridNetworkRef:.spec.gridNetworkRef}}' \
  "$EVIDENCE_DIR/gridsite-before.raw.json" >"$EVIDENCE_DIR/gridsite-before.json"
rm "$EVIDENCE_DIR/gridsite-before.raw.json"
jq -e --arg run "$RUN_LABEL" --arg release "$RELEASE_NAME" --arg network "$NETWORK_NAME" \
  '.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.labels["app.kubernetes.io/instance"]==$release and .spec.gridNetworkRef==$network' \
  "$EVIDENCE_DIR/gridsite-before.json" >/dev/null || fail 'GridSite ownership or GridNetwork reference check failed'
SITE_UID="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/gridsite-before.json")"

api_ready
if [[ "$MODE" == cleanup ]]; then
  await_operator_ready immediately-before-network-delete
else
  await_operator_ready resumed-after-network-delete
fi
printf 'gridNetworkUID=%s\ngridSiteUID=%s\noperatorDeploymentUID=%s\noperatorPodUID=%s\noperatorPod=%s\n' \
  "$NETWORK_UID" "$SITE_UID" "$DEPLOYMENT_UID" "$OPERATOR_POD_UID" "$OPERATOR_POD" >"$EVIDENCE_DIR/cleanup-identity.txt"
chmod 0600 "$EVIDENCE_DIR/cleanup-identity.txt"
if [[ "$MODE" == cleanup ]]; then
  log "ownership, image digest, provider absence, and operator readiness gates passed; beginning GridNetwork withdrawal"
  delete_resource_with_uid "/apis/grid.praxis.fast/v1alpha1/gridnetworks/$NETWORK_NAME" "$NETWORK_UID" \
    "$EVIDENCE_DIR/gridnetwork-delete-response.json" "$EVIDENCE_DIR/gridnetwork-delete-options.json"
  deadline=$((SECONDS + ${WAIT_TIMEOUT%s}))
  : >"$EVIDENCE_DIR/gridnetwork-finalizer-watch.jsonl"
  while ((SECONDS < deadline)); do
    if oc get gridnetwork "$NETWORK_NAME" -o json >"$EVIDENCE_DIR/gridnetwork-watch.raw.json" 2>/dev/null; then
      jq --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        '{observedAt:$observedAt,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp,finalizers:(.metadata.finalizers//[])}' \
        "$EVIDENCE_DIR/gridnetwork-watch.raw.json" >>"$EVIDENCE_DIR/gridnetwork-finalizer-watch.jsonl"
      jq -e --arg uid "$NETWORK_UID" '.metadata.uid==$uid and .metadata.deletionTimestamp!=null' "$EVIDENCE_DIR/gridnetwork-watch.raw.json" >/dev/null ||
        fail 'GridNetwork UID changed or deletion timestamp was not observed'
    else
      break
    fi
    sleep 2
  done
  rm -f "$EVIDENCE_DIR/gridnetwork-watch.raw.json"
  oc get gridnetwork "$NETWORK_NAME" -o json >"$EVIDENCE_DIR/gridnetwork-after.json" 2>/dev/null && fail 'GridNetwork withdrawal finalizer did not complete before timeout'
else
  log "resume validated that UID $NETWORK_UID was deleted by the prior UID-preconditioned request"
fi
require_absent gridnetwork "$NETWORK_NAME" ""
log "GridNetwork $NETWORK_NAME is absent; checking the SWIM withdrawal receipt in operator logs"

oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-deployment-after-network.json"
jq -e --arg uid "$DEPLOYMENT_UID" --arg image "$OPERATOR_IMAGE" \
  '.metadata.uid==$uid and .spec.replicas==1 and .metadata.generation==.status.observedGeneration and
   (.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1 and
   all(.spec.template.spec.containers[];.image==$image) and any(.status.conditions[]?;.type=="Available" and .status=="True")' \
  "$EVIDENCE_DIR/operator-deployment-after-network.json" >/dev/null || fail 'operator was not still Ready after GridNetwork deletion'
oc logs "$OPERATOR_POD" -n "$RELEASE_NAMESPACE" --since=30m 2>&1 |
  awk -v network="$NETWORK_NAME" -v grid="$GRID_ID" \
    '{line=$0; gsub(/\033\[[0-9;]*m/, "", line);
     if (index(line,"published GridNetwork SWIM withdrawal; removed finalizer") &&
         index(line,"network=\"" network "\"") && index(line,"grid_id=" grid)) {print line}}' \
    >"$EVIDENCE_DIR/gridnetwork-withdrawal-receipt.log"
[[ -s "$EVIDENCE_DIR/gridnetwork-withdrawal-receipt.log" ]] || fail 'operator SWIM withdrawal receipt was not observed; operator remains installed for diagnosis'
log 'operator log confirms SWIM withdrawal publication and automatic GridNetwork finalizer removal'

api_ready
delete_resource_with_uid "/apis/grid.praxis.fast/v1alpha1/gridsites/$SITE_NAME" "$SITE_UID" \
  "$EVIDENCE_DIR/gridsite-delete-response.json" "$EVIDENCE_DIR/gridsite-delete-options.json"
oc get gridnetwork "$NETWORK_NAME" -o name >"$EVIDENCE_DIR/gridnetwork-still-present.txt" 2>&1 && fail 'GridNetwork unexpectedly remains after finalization'
oc get gridsite "$SITE_NAME" -o name >"$EVIDENCE_DIR/gridsite-still-present.txt" 2>&1 && fail 'GridSite unexpectedly remains after deletion'
require_absent gridnetwork "$NETWORK_NAME" ""
require_absent gridsite "$SITE_NAME" ""
rm -f "$EVIDENCE_DIR/gridnetwork-still-present.txt" "$EVIDENCE_DIR/gridsite-still-present.txt"

api_ready
log 'GridNetwork and GridSite are absent; operator is still Ready; uninstalling Helm release'
helm uninstall "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" --wait --timeout="$WAIT_TIMEOUT" >"$EVIDENCE_DIR/helm-uninstall.txt" || fail 'Helm release uninstall failed after Grid finalization'
if helm status "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/helm-status-after.json" 2>/dev/null; then
  fail 'Helm release still exists after uninstall'
fi
if oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-deployment-after-uninstall.json" 2>/dev/null; then
  fail 'operator Deployment remains after Helm uninstall'
fi
require_absent deployment "$DEPLOYMENT_NAME" "$RELEASE_NAMESPACE"
oc get crd -o json | jq '[.items[]|select(.metadata.name|IN("agenttoolproviders.grid.praxis.fast","gridnetworks.grid.praxis.fast","gridsites.grid.praxis.fast","inferenceproviders.grid.praxis.fast","siteinvites.grid.praxis.fast"))|{name:.metadata.name,established:any(.status.conditions[]?;.type=="Established" and .status=="True")}]' >"$EVIDENCE_DIR/grid-crds-after-uninstall.json"
jq -e 'length==5 and all(.[];.established==true)' "$EVIDENCE_DIR/grid-crds-after-uninstall.json" >/dev/null || fail 'expected retained Grid CRDs are missing or not Established'

api_ready
oc get namespace "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-namespace-before-delete.json" || fail 'operator namespace disappeared before its owned cleanup'
jq -e --arg uid "$NAMESPACE_UID" --arg run "$RUN_LABEL" '.metadata.uid==$uid and .metadata.labels["praxis.fast/qualification-run"]==$run' \
  "$EVIDENCE_DIR/operator-namespace-before-delete.json" >/dev/null || fail 'operator namespace UID/ownership changed; refusing namespace cleanup'
delete_resource_with_uid "/api/v1/namespaces/$RELEASE_NAMESPACE" "$NAMESPACE_UID" \
  "$EVIDENCE_DIR/operator-namespace-delete-response.json" "$EVIDENCE_DIR/operator-namespace-delete-options.json"
deadline=$((SECONDS + ${WAIT_TIMEOUT%s}))
: >"$EVIDENCE_DIR/operator-namespace-watch.jsonl"
while ((SECONDS < deadline)); do
  if oc get namespace "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-namespace-watch.raw.json" 2>/dev/null; then
    jq --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      '{observedAt:$observedAt,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp,finalizers:(.spec.finalizers//[])}' \
      "$EVIDENCE_DIR/operator-namespace-watch.raw.json" >>"$EVIDENCE_DIR/operator-namespace-watch.jsonl"
    jq -e --arg uid "$NAMESPACE_UID" '.metadata.uid==$uid and .metadata.deletionTimestamp!=null' \
      "$EVIDENCE_DIR/operator-namespace-watch.raw.json" >/dev/null || fail 'operator namespace UID changed or deletion timestamp was not observed'
  else
    break
  fi
  sleep 2
done
if oc get namespace "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-namespace-after.json" 2>/dev/null; then
  fail 'operator namespace cleanup did not complete before timeout'
fi
require_absent namespace "$RELEASE_NAMESPACE" ""
api_ready

jq -n --arg run "$RUN_LABEL" --arg api "$EXPECTED_API_SERVER" --arg network "$NETWORK_NAME" --arg networkUid "$NETWORK_UID" \
  --arg site "$SITE_NAME" --arg siteUid "$SITE_UID" --arg release "$RELEASE_NAMESPACE/$RELEASE_NAME" \
  --arg image "$OPERATOR_IMAGE" --arg evidence "$EVIDENCE_DIR" --arg resumedFrom "$RESUME_FROM" \
  '{schema:"praxis-extproc.grid-operator-cleanup/v1",status:"passed",run:$run,apiServer:$api,gridNetwork:{name:$network,uid:$networkUid,withdrawalFinalizerObserved:true,operatorReceiptObserved:true},gridSite:{name:$site,uid:$siteUid},helmRelease:$release,operatorImage:$image,crdsRetained:true,operatorNamespaceRemoved:true,taskResourceNamespaceRemoved:false,resumedFrom:(if $resumedFrom=="" then null else $resumedFrom end),evidenceDirectory:$evidence,inferenceTrafficSent:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
for file in "$EVIDENCE_DIR"/*; do
  if [[ -d "$file" ]]; then chmod 0700 "$file"; else chmod 0600 "$file"; fi
done
log 'cleanup passed: withdrawal completed while the pinned Grid operator was Ready, then the release was removed'
printf 'Evidence: %s\n' "$EVIDENCE_DIR"
