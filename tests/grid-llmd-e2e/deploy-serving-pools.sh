#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build and reconcile source-pinned simulator-backed LLMInferenceService pools.
set -Eeuo pipefail
# Markdown backticks in printf format strings are literal; values are passed as arguments.
# shellcheck disable=SC2016
umask 077

env_default() {
  local name="$1" fallback="$2" value
  value="$(printenv "$name" 2>/dev/null || true)"
  if [[ -n "$value" ]]; then printf '%s' "$value"; else printf '%s' "$fallback"; fi
}

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="$(env_default SERVING_POOLS_MODE plan)"
SERVING_POOL_SELECTION="$(env_default SERVING_POOL_SELECTION both)"
EXPECTED_API_SERVER="$(env_default EXPECTED_API_SERVER '')"
KUBE_CONTEXT="$(env_default KUBE_CONTEXT '')"
CLUSTER_ENV="$(env_default CLUSTER_ENV '')"
CLUSTER_PLATFORM="$(env_default GRID_LLMD_CLUSTER_PLATFORM openshift)"
ALLOW_INSECURE_DEV_API="$(env_default ALLOW_INSECURE_DEV_API false)"
RESOURCE_PROFILE="$(env_default RESOURCE_PROFILE strict)"
NAMESPACE="$(env_default GRID_RESOURCE_NAMESPACE grid-llmd-e2e)"
DSC_NAME="$(env_default DATASCIENCECLUSTER_NAME default-dsc)"
ARGO_APPLICATION_NAME="$(env_default ARGO_APPLICATION_NAME ephemeral-rhoai-maas-platform)"
ARGO_APPLICATION_NAMESPACE="$(env_default ARGO_APPLICATION_NAMESPACE openshift-gitops)"
LLMISVC_GATEWAY_NAME="$(env_default LLMISVC_GATEWAY_NAME grid-llmd-ingress)"
LLMISVC_GATEWAY_NAMESPACE="$(env_default LLMISVC_GATEWAY_NAMESPACE "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf maas-system; else printf '%s' "$NAMESPACE"; fi)")"
LLMISVC_GATEWAY_SECTION_NAME="$(env_default LLMISVC_GATEWAY_SECTION_NAME inference)"
POOL_A_GATEWAY_NAME="$(env_default POOL_A_GATEWAY_NAME grid-llmd-pool-a)"
POOL_B_GATEWAY_NAME="$(env_default POOL_B_GATEWAY_NAME grid-llmd-pool-b)"
POOL_GATEWAY_LISTENER_PORT="$(env_default POOL_GATEWAY_LISTENER_PORT 8080)"
POOL_GATEWAY_INFRA_ANNOTATION_KEY="$(env_default POOL_GATEWAY_INFRA_ANNOTATION_KEY "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf networking.istio.io/service-type; else printf service.beta.kubernetes.io/aws-load-balancer-internal; fi)")"
POOL_GATEWAY_INFRA_ANNOTATION_VALUE="$(env_default POOL_GATEWAY_INFRA_ANNOTATION_VALUE "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf ClusterIP; else printf true; fi)")"
LLMISVC_TLS_ENABLED="$(env_default LLMISVC_TLS_ENABLED false)"
RUN_LABEL="$(env_default GRID_RUN_LABEL '')"
SOURCE_LOCKS_MD="$(env_default SOURCE_LOCKS_MD "$REPO_ROOT/evidence/$RUN_LABEL/source-locks.md")"
SOURCE_LOCKS_FILE="$(env_default SOURCE_LOCKS_FILE "${SOURCE_LOCKS_MD%.md}.json")"
TRANSPORT_STATE_FILE="$(env_default LLMISVC_TRANSPORT_STATE_FILE "$REPO_ROOT/evidence/task-state/$RUN_LABEL/llmisvc-transport-state.json")"
EVIDENCE_DIR="$(env_default SERVING_POOLS_EVIDENCE_DIR '')"
WAIT_TIMEOUT="$(env_default SERVING_POOLS_WAIT_TIMEOUT 900s)"
BUILD_TIMEOUT="$(env_default SIMULATOR_BUILD_TIMEOUT 3600s)"
BUILD_STATUS_SYNC_TIMEOUT="$(env_default SIMULATOR_BUILD_STATUS_SYNC_TIMEOUT 120s)"
SIMULATOR_SOURCE_URL="$(env_default SIMULATOR_SOURCE_URL https://github.com/llm-d/llm-d-inference-sim.git)"
SIMULATOR_OCI_SOURCE_URL="$(env_default SIMULATOR_OCI_SOURCE_URL "$SIMULATOR_SOURCE_URL")"
SIMULATOR_SOURCE_REVISION="$(env_default SIMULATOR_SOURCE_REVISION bca276ef64263e214b4822940bb1e58d018cbf9d)"
SIMULATOR_BUILD_CONFIG="$(env_default SIMULATOR_BUILD_CONFIG llm-d-sim-source-build)"
SIMULATOR_IMAGESTREAM="$(env_default SIMULATOR_IMAGESTREAM llm-d-sim)"
SIMULATOR_IMAGE_TAG="$(env_default SIMULATOR_IMAGE_TAG source-bca276ef6426)"
SIMULATOR_IMAGE="$(env_default SIMULATOR_IMAGE '')"
SIMULATOR_PULL_POLICY="$(env_default SIMULATOR_PULL_POLICY IfNotPresent)"
EXTPROC_IMAGE="$(env_default EXTPROC_IMAGE '')"
EXTPROC_SOURCE_REVISION="$(env_default EXTPROC_SOURCE_REVISION e20c802675f549c56bd23d29bc64c2fb2fdc895c)"
GITOPS_SOURCE_REVISION="$(env_default GITOPS_SOURCE_REVISION "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf not-used; else printf 1106cd57746be7be9a37805d9f48e48518cc31b9; fi)")"
GRID_GATEWAY_SOURCE_REVISION="$(env_default GRID_GATEWAY_SOURCE_REVISION 33cb906be2bb0ebdbaf253bd307939a44f41b37b)"
ODH_GRID_OPERATOR_SOURCE_REVISION="$(env_default ODH_GRID_OPERATOR_SOURCE_REVISION ed60534100b4b0ee986b6714d1e7cb6d685b7236)"
MAAS_SOURCE_REVISION="$(env_default MAAS_SOURCE_REVISION edddce0581a0863a3a63f4abfdb6f73fcc2686b8)"
SCHEDULER_CONFIG_NAME="$(env_default SCHEDULER_CONFIG_NAME "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf kserve-config-llm-scheduler; else printf v3-5-2-kserve-config-llm-scheduler; fi)")"
SCHEDULER_CONFIG_NAMESPACE="$(env_default SCHEDULER_CONFIG_NAMESPACE "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf kserve; else printf redhat-ods-applications; fi)")"
MAAS_TENANT_CONFIG_NAMESPACE="$(env_default MAAS_TENANT_CONFIG_NAMESPACE "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf models-as-a-service; else printf redhat-ods-applications; fi)")"
KSERVE_ROUTE_CONTROLLER_NAME="$(env_default KSERVE_ROUTE_CONTROLLER_NAME "$(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf istio.io/gateway-controller; else printf openshift.io/gateway-controller/v1; fi)")"
EPP_IMAGE="$(env_default EPP_IMAGE ghcr.io/llm-d/llm-d-router-endpoint-picker@sha256:024188e998f516879f68532016ca24fabca00afbdcc2af9a61b17634e197df2c)"
EPP_SOURCE_REVISION="$(env_default EPP_SOURCE_REVISION b9393d1a9f1d5a72ecd86f8d2b643ab54c2a9b92)"
EPP_CPU_REQUEST="$(env_default EPP_CPU_REQUEST 20m)"
EPP_MEMORY_REQUEST="$(env_default EPP_MEMORY_REQUEST 128Mi)"
EPP_CPU_LIMIT="$(env_default EPP_CPU_LIMIT 500m)"
EPP_MEMORY_LIMIT="$(env_default EPP_MEMORY_LIMIT 512Mi)"
POOL_A="$(env_default POOL_A_NAME pool-a)"
POOL_B="$(env_default POOL_B_NAME pool-b)"
MODEL_NAME="$(env_default SIMULATOR_MODEL_NAME grid-test-model)"
POOL_A_ORACLE="$(env_default POOL_A_RESPONSE_ORACLE pool-a-response)"
POOL_B_ORACLE="$(env_default POOL_B_RESPONSE_ORACLE pool-b-response)"
SIMULATOR_CPU_REQUEST="$(env_default SIMULATOR_CPU_REQUEST 25m)"
SIMULATOR_MEMORY_REQUEST="$(env_default SIMULATOR_MEMORY_REQUEST 128Mi)"
SIMULATOR_CPU_LIMIT="$(env_default SIMULATOR_CPU_LIMIT 500m)"
SIMULATOR_MEMORY_LIMIT="$(env_default SIMULATOR_MEMORY_LIMIT 512Mi)"
PLATFORM_HELPER="$(env_default PLATFORM_HELPER "$SCRIPT_DIR/cold-deploy.sh")"
SIMULATOR_IMAGE_EVIDENCE_DIR="$(env_default SIMULATOR_IMAGE_EVIDENCE_DIR '')"
SIMULATOR_IMAGE_PROVENANCE_FILE="$(env_default SIMULATOR_IMAGE_PROVENANCE_FILE '')"
SIMULATOR_IMAGE_REPOSITORY="$(env_default SIMULATOR_IMAGE_REPOSITORY '')"
SIMULATOR_DEPLOY_IMAGE_REPOSITORY="$(env_default SIMULATOR_DEPLOY_IMAGE_REPOSITORY '')"
SIMULATOR_IMAGE_PUSH_TLS_VERIFY="$(env_default SIMULATOR_IMAGE_PUSH_TLS_VERIFY true)"
ALLOW_INSECURE_IMAGE_PUSH="$(env_default ALLOW_INSECURE_IMAGE_PUSH false)"
CONTAINER_ENGINE="$(env_default CONTAINER_ENGINE docker)"
IMAGE_INSPECTOR="$(env_default IMAGE_INSPECTOR skopeo)"

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

pool_gateway_name() {
  case "$1" in
    "$POOL_A") printf '%s' "$POOL_A_GATEWAY_NAME" ;;
    "$POOL_B") printf '%s' "$POOL_B_GATEWAY_NAME" ;;
    *) printf 'unknown simulator pool: %s\n' "$1" >&2; return 2 ;;
  esac
}

capture_kind_platform_pending_events() {
  local output="$1"
  oc get events -A --sort-by=.lastTimestamp -o json 2>/dev/null |
    jq '[.items[-300:][]?|select(.type=="Warning" or .reason=="FailedScheduling" or .reason=="OOMKilling")|{namespace:.metadata.namespace,type,reason,message,lastTimestamp,object:.involvedObject}]' |
    python3 "$SCRIPT_DIR/sanitize_diagnostics.py" >"$output" || true
  chmod 0600 "$output" 2>/dev/null || true
}

wait_for_kind_pending_pods() {
  local dir="$1" timeout_seconds=120 poll_seconds=2 started elapsed attempt=0 initial_pending=-1
  local pending_count failed_count unschedulable_count
  mkdir -p "$dir"
  chmod 0700 "$dir"
  : >"$dir/pending-pod-polls.tsv"
  started=$SECONDS
  while :; do
    attempt=$((attempt + 1))
    oc get pods -A -o json >"$dir/pods-current.json"
    jq '[.items[]|select(.status.phase=="Pending" or .status.phase=="Failed" or .status.phase=="Unknown")|{
      namespace:.metadata.namespace,name:.metadata.name,uid:.metadata.uid,phase:.status.phase,reason:.status.reason,nodeName:.spec.nodeName,
      ownerReferences:[.metadata.ownerReferences[]?|{kind,name}],
      podScheduled:([.status.conditions[]?|select(.type=="PodScheduled")|{status,reason,message}][0]//{}),
      containerStates:[.status.containerStatuses[]?|{name,ready,state}]
    }]' "$dir/pods-current.json" >"$dir/pending-failed-pods.json"
    pending_count="$(jq '[.[]|select(.phase=="Pending")]|length' "$dir/pending-failed-pods.json")"
    failed_count="$(jq '[.[]|select(.phase=="Failed" or .phase=="Unknown")]|length' "$dir/pending-failed-pods.json")"
    unschedulable_count="$(jq '[.[]|select(.phase=="Pending" and .podScheduled.status=="False" and .podScheduled.reason=="Unschedulable")]|length' "$dir/pending-failed-pods.json")"
    ((initial_pending>=0)) || initial_pending="$pending_count"
    elapsed=$((SECONDS - started))
    printf '%s\t%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$attempt" "$pending_count" "$failed_count" "$unschedulable_count" \
      >>"$dir/pending-pod-polls.tsv"
    if (( failed_count > 0 || unschedulable_count > 0 )); then
      capture_kind_platform_pending_events "$dir/events-on-pending-failure.json"
      jq -n --argjson attempts "$attempt" --argjson waited "$elapsed" --argjson pending "$pending_count" \
        --argjson failed "$failed_count" --argjson unschedulable "$unschedulable_count" \
        '{status:"FAIL",attempts:$attempts,waitedSeconds:$waited,pendingPods:$pending,failedOrUnknownPods:$failed,unschedulablePods:$unschedulable}' \
        >"$dir/pending-convergence-result.json"
      log "Kind pending-pod gate failed immediately: pending=$pending_count failedOrUnknown=$failed_count unschedulable=$unschedulable_count"
      return 1
    fi
    if (( pending_count == 0 )); then
      jq -n --argjson attempts "$attempt" --argjson waited "$elapsed" --argjson initial "$initial_pending" \
        '{status:"PASS",attempts:$attempts,waitedSeconds:$waited,initialPendingPods:$initial,finalPendingPods:0,timeoutSeconds:120}' \
        >"$dir/pending-convergence-result.json"
      log "Kind pending-pod gate PASS after ${elapsed}s and $attempt polls; initialPending=$initial_pending"
      return 0
    fi
    if (( elapsed >= timeout_seconds )); then
      capture_kind_platform_pending_events "$dir/events-on-pending-failure.json"
      jq -n --argjson attempts "$attempt" --argjson waited "$elapsed" --argjson initial "$initial_pending" \
        --argjson pending "$pending_count" --argjson timeout "$timeout_seconds" \
        '{status:"TIMEOUT",attempts:$attempts,waitedSeconds:$waited,initialPendingPods:$initial,finalPendingPods:$pending,timeoutSeconds:$timeout}' \
        >"$dir/pending-convergence-result.json"
      log "Kind pending-pod gate timed out after ${elapsed}s with $pending_count Pending Pods; see $dir/pending-failed-pods.json and events-on-pending-failure.json"
      return 1
    fi
    sleep "$poll_seconds"
  done
}

wait_for_kind_platform_deployments() {
  local dir="$1" timeout_seconds=180 poll_seconds=2 started elapsed attempt=0 ready_count total_count=8
  local required='["kserve/llmisvc-controller-manager","maas-system/maas-controller","maas-system/maas-api","maas-system/postgres","maas-system/payload-pre-processing","maas-system/payload-processing","opendatahub/ai-gateway-controller","kuadrant-system/authorino"]'
  mkdir -p "$dir"
  chmod 0700 "$dir"
  : >"$dir/platform-deployment-polls.tsv"
  started=$SECONDS
  while :; do
    attempt=$((attempt + 1))
    oc get deployments -A -o json >"$dir/deployments-current.json"
    jq -r --argjson required "$required" '
      $required[] as $ref |
      ($ref|split("/")) as $parts |
      ([.items[]|select(.metadata.namespace==$parts[0] and .metadata.name==$parts[1])][0]//null) as $d |
      if $d!=null and ($d.status.observedGeneration//0)>=($d.metadata.generation//0) and
         ($d.spec.replicas//0)>0 and ($d.status.readyReplicas//0)==($d.spec.replicas//0) and
         any($d.status.conditions[]?;.type=="Available" and .status=="True")
      then "ready\t"+$ref else "waiting\t"+$ref end
    ' "$dir/deployments-current.json" >"$dir/platform-deployments-current.tsv"
    ready_count="$(awk -F '\t' '$1=="ready" {n++} END {print n+0}' "$dir/platform-deployments-current.tsv")"
    elapsed=$((SECONDS - started))
    printf '%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$attempt" "$ready_count" "$total_count" \
      >>"$dir/platform-deployment-polls.tsv"
    if (( ready_count == total_count )); then
      jq -n --argjson attempts "$attempt" --argjson waited "$elapsed" --argjson ready "$ready_count" \
        '{status:"PASS",attempts:$attempts,waitedSeconds:$waited,readyDeployments:$ready,requiredDeployments:8,timeoutSeconds:180}' \
        >"$dir/platform-deployment-readiness-result.json"
      log "Kind required platform Deployments PASS after ${elapsed}s and $attempt polls"
      return 0
    fi
    if (( elapsed >= timeout_seconds )); then
      capture_kind_platform_pending_events "$dir/events-on-deployment-timeout.json"
      jq -n --argjson attempts "$attempt" --argjson waited "$elapsed" --argjson ready "$ready_count" \
        --argjson requiredCount "$total_count" --argjson timeout "$timeout_seconds" \
        '{status:"TIMEOUT",attempts:$attempts,waitedSeconds:$waited,readyDeployments:$ready,requiredDeployments:$requiredCount,timeoutSeconds:$timeout}' \
        >"$dir/platform-deployment-readiness-result.json"
      log "Kind platform Deployments timed out after ${elapsed}s: $ready_count/$total_count Ready; see platform-deployments-current.tsv and events-on-deployment-timeout.json"
      return 1
    fi
    sleep "$poll_seconds"
  done
}

verify_kind_platform() {
  local dir="$1" server ready dep ns name can_i
  local deployments='[]'
  mkdir -p "$dir"
  chmod 0700 "$dir"
  server="$(oc whoami --show-server)"
  ready="$(oc get --raw=/readyz)"
  [[ "$server" == "$EXPECTED_API_SERVER" && "$ready" == *ok* ]] || {
    log "Kind platform API gate failed: server=$server readyz=$ready"; return 1;
  }
  oc get nodes -o json >"$dir/nodes.json"
  jq -e 'length>0 and all(.items[];.status.conditions|any(.type=="Ready" and .status=="True"))' \
    "$dir/nodes.json" >/dev/null || { log 'Kind platform has no Ready node'; return 1; }
  wait_for_kind_platform_deployments "$dir/deployment-convergence"
  wait_for_kind_pending_pods "$dir/pending-convergence"
  oc get pods -A -o json | jq '[.items[]|select(.status.phase=="Pending" or .status.phase=="Failed" or .status.phase=="Unknown")|{namespace:.metadata.namespace,name:.metadata.name,phase:.status.phase,reason:.status.reason}]' >"$dir/pending-failed-pods.json"
  jq -e 'length==0' "$dir/pending-failed-pods.json" >/dev/null || { log 'Kind platform has Pending/Failed/Unknown Pods'; return 1; }
  for dep in kserve/llmisvc-controller-manager maas-system/maas-controller maas-system/maas-api \
    maas-system/postgres maas-system/payload-pre-processing maas-system/payload-processing \
    opendatahub/ai-gateway-controller kuadrant-system/authorino; do
    ns="${dep%%/*}"; name="${dep#*/}"
    oc get deployment "$name" -n "$ns" -o json >"$dir/deployment-${ns}-${name}.json" || {
      log "Kind platform Deployment $ns/$name is missing"; return 1;
    }
    jq -e '
      (.status.observedGeneration//0)>=(.metadata.generation//0) and
      (.status.readyReplicas//0)==(.spec.replicas//0) and
      (.spec.replicas//0)>0 and
      any(.status.conditions[]?;.type=="Available" and .status=="True")
    ' "$dir/deployment-${ns}-${name}.json" >/dev/null || {
      log "Kind platform Deployment $ns/$name is not current and Available"; return 1;
    }
    deployments="$(jq --arg ns "$ns" --arg name "$name" --argjson current "$(cat "$dir/deployment-${ns}-${name}.json")" \
      '. + [{namespace:$ns,name:$name,generation:$current.metadata.generation,observedGeneration:$current.status.observedGeneration,readyReplicas:$current.status.readyReplicas,replicas:$current.spec.replicas,images:[$current.spec.template.spec.containers[].image]}]' <<<"$deployments")"
  done
  oc get crd llminferenceservices.serving.kserve.io -o json >"$dir/llmisvc-crd.json"
  jq -e 'any(.spec.versions[];.name=="v1alpha2" and .served==true and .storage==true)' "$dir/llmisvc-crd.json" >/dev/null || {
    log 'KServe LLMInferenceService CRD does not serve/store v1alpha2'; return 1;
  }
  oc get llminferenceserviceconfig "$SCHEDULER_CONFIG_NAME" -n "$SCHEDULER_CONFIG_NAMESPACE" -o json >"$dir/scheduler-config.json"
  oc get certificate llmisvc-serving-cert -n kserve -o json >"$dir/kserve-webhook-certificate.json"
  jq -e 'any(.status.conditions[]?;.type=="Ready" and .status=="True")' "$dir/kserve-webhook-certificate.json" >/dev/null || {
    log 'KServe webhook serving certificate is not Ready'; return 1;
  }
  oc get endpointslices -n kserve -l kubernetes.io/service-name=llmisvc-webhook-server-service -o json >"$dir/kserve-webhook-endpointslices.json"
  jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true and (.addresses|length)>0))' "$dir/kserve-webhook-endpointslices.json" >/dev/null || {
    log 'KServe admission/conversion webhook Service has no ready endpoint'; return 1;
  }
  oc get aitenant models-as-a-service -n ai-tenants -o json >"$dir/maas-bootstrap-aitenant.json"
  jq -e 'any(.status.conditions[]?;.type=="Ready" and .status=="True")' "$dir/maas-bootstrap-aitenant.json" >/dev/null || {
    log 'MaaS default bootstrap AITenant is not Ready'; return 1;
  }
  oc get maastenantconfig default-tenant -n "$MAAS_TENANT_CONFIG_NAMESPACE" -o json >"$dir/maas-default-tenant-config.json"
  can_i="$(oc auth can-i get leases --as=system:serviceaccount:maas-system:maas-controller -n maas-system)"
  [[ "$can_i" == yes ]] || { log 'MaaS controller cannot read its leader-election Lease'; return 1; }
  printf '%s\n' "$can_i" >"$dir/maas-controller-lease-rbac-can-i.txt"
  jq -n --arg server "$server" --arg readyz "$ready" --argjson deployments "$deployments" \
    '{status:"PASS",clusterPlatform:"kind",server:$server,readyz:$readyz,failedGates:0,deployments:$deployments,actualLlmInferenceServiceAPI:{group:"serving.kserve.io",version:"v1alpha2",controller:"kserve/llmisvc-controller-manager",webhookEndpointReady:true},maasControllerLeaseRBAC:true,inferenceTrafficSent:false}' \
    >"$dir/platform-gate-summary.json"
  log 'Kind KServe/MaaS/AIGC/Authorino platform gate PASS; controller, scheduler config, v1alpha2 API, webhook endpoint, default MaaS tenant and lease RBAC are verified'
}

verify_kind_capacity() {
  local dir="$EVIDENCE_DIR/capacity-preflight" pending_count ready_nodes status baseline
  mkdir -p "$dir"
  chmod 0700 "$dir"
  oc get nodes -o json >"$dir/nodes.json"
  oc describe nodes >"$dir/node-capacity-and-allocated-requests.txt"
  wait_for_kind_pending_pods "$dir/pending-convergence"
  oc get pods -A -o json | jq '[.items[]|select(.status.phase=="Pending" or .status.phase=="Failed" or .status.phase=="Unknown")|{namespace:.metadata.namespace,name:.metadata.name,phase:.status.phase,reason:.status.reason,nodeName:.spec.nodeName}]' >"$dir/pending-failed-pods.json"
  pending_count="$(jq 'length' "$dir/pending-failed-pods.json")"
  ready_nodes="$(jq '[.items[]|select(.status.conditions|any(.type=="Ready" and .status=="True"))]|length' "$dir/nodes.json")"
  ((ready_nodes>0)) || { log 'Kind capacity gate has no Ready nodes'; return 1; }
  [[ "$pending_count" == 0 ]] || { log "Kind capacity gate has $pending_count Pending/Failed/Unknown Pods"; return 1; }
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    status=preflight-pass-reduced-dev; baseline=WAIVED_FOR_KIND_DEV_SMOKE
  else
    status=preflight-pass; baseline=REQUIRED
  fi
  jq -n --arg status "$status" --arg baseline "$baseline" --arg profile "$RESOURCE_PROFILE" \
    --argjson readyNodes "$ready_nodes" --argjson pendingPods "$pending_count" \
    '{status:$status,clusterPlatform:"kind",resourceProfile:$profile,capacityBaseline:$baseline,readyNodes:$readyNodes,pendingPods:$pendingPods,workerSizeBaselineEnforced:($profile!="reduced-dev"),inferenceTrafficSent:false}' \
    >"$dir/report.json"
  log "Kind capacity preflight PASS: readyNodes=$ready_nodes pendingPods=$pending_count baseline=$baseline; allocated requests captured"
}

verify_pool_gateways_ready() {
  local pool gateway gateway_file services_file service endpoints_file
  CURRENT_PHASE='pool-gateway-readiness'
  for pool in "$POOL_A" "$POOL_B"; do
    gateway="$(pool_gateway_name "$pool")"
    gateway_file="$EVIDENCE_DIR/$pool-pool-gateway.json"
    services_file="$EVIDENCE_DIR/$pool-pool-gateway-services.json"
    endpoints_file="$EVIDENCE_DIR/$pool-pool-gateway-endpointslices.json"
    oc get gateway "$gateway" -n "$LLMISVC_GATEWAY_NAMESPACE" -o json >"$gateway_file"
    jq -e --arg run "$RUN_LABEL" --arg pool "$pool" --arg listener "$LLMISVC_GATEWAY_SECTION_NAME" \
      --argjson port "$POOL_GATEWAY_LISTENER_PORT" '
      .metadata.labels["praxis.fast/qualification-run"]==$run and
      .metadata.labels["praxis.fast/simulator-pool"]==$pool and
      (.metadata.generation as $g |
        any(.status.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$g) and
        any(.status.conditions[]?;.type=="Programmed" and .status=="True" and .observedGeneration==$g)) and
      any(.spec.listeners[]?;.name==$listener and .port==$port and .protocol=="HTTP")
    ' "$gateway_file" >/dev/null || {
      log "pool Gateway $gateway is not owned by this run or lacks a current HTTP listener"; return 1;
    }
    oc get services -n "$LLMISVC_GATEWAY_NAMESPACE" \
      -l "gateway.networking.k8s.io/gateway-name=$gateway" -o json >"$services_file"
    jq -e --argjson port "$POOL_GATEWAY_LISTENER_PORT" --arg key "$POOL_GATEWAY_INFRA_ANNOTATION_KEY" \
      --arg value "$POOL_GATEWAY_INFRA_ANNOTATION_VALUE" '
      [.items[]|select(any(.spec.ports[]?;.port==$port) and .metadata.annotations[$key]==$value)]|length==1
    ' "$services_file" >/dev/null || {
      log "pool Gateway $gateway lacks one listener Service with the required internal infrastructure annotation"; return 1;
    }
    service="$(jq -r --argjson port "$POOL_GATEWAY_LISTENER_PORT" --arg key "$POOL_GATEWAY_INFRA_ANNOTATION_KEY" \
      --arg value "$POOL_GATEWAY_INFRA_ANNOTATION_VALUE" \
      '[.items[]|select(any(.spec.ports[]?;.port==$port) and .metadata.annotations[$key]==$value)][0].metadata.name' "$services_file")"
    oc get endpointslices -n "$LLMISVC_GATEWAY_NAMESPACE" \
      -l "kubernetes.io/service-name=$service" -o json >"$endpoints_file"
    jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$endpoints_file" >/dev/null || {
      log "pool Gateway $gateway listener Service $service has no ready endpoint"; return 1;
    }
    jq -n --arg pool "$pool" --arg gateway "$gateway" --arg service "$service" \
      --arg namespace "$LLMISVC_GATEWAY_NAMESPACE" --arg listener "$LLMISVC_GATEWAY_SECTION_NAME" \
      --argjson port "$POOL_GATEWAY_LISTENER_PORT" \
      '{pool:$pool,gateway:{name:$gateway,namespace:$namespace,listener:$listener,port:$port},service:$service,endpoint:("http://"+$service+"."+$namespace+".svc.cluster.local:"+($port|tostring)),ready:true}' \
      >"$EVIDENCE_DIR/$pool-pool-gateway-readiness.json"
    log "Pool Gateway $gateway and its run-owned internal listener Service $service are Ready"
  done
}

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Serving pool plan (no cluster calls):
  1. Verify API identity/server/readiness, task namespace ownership, and platform gates.
  2. Build llm-d-sim from the exact Git commit into a task-owned OpenShift ImageStream.
  3. Verify OCI source/revision labels and resolve an immutable internal image digest.
  4. Copy the RHOAI 3.5.2 scheduler LLMInferenceServiceConfig into the task namespace.
  5. Pin the EPP image, reduce only its resources, and set the pool failure mode to FailClose.
  6. Require two already-installed, run-owned pool Gateways and their internal listener Services.
  7. Create two independent LLMInferenceService, InferencePool, EPP, backend Service,
     EndpointSlice, simulator Pod, and simulator-control ConfigMap graphs. Each service
     references its matching pool Gateway. The supported cluster-wide DSCI TLS flag is
     set only after confirming there are no existing LLMInferenceServices; its prior state is saved.
  8. Verify current-generation readiness, immutable runtime image IDs, and disjoint
     ready simulator Pod UIDs.

This helper does not send inference traffic, create Grid providers, enable Praxis
consumer configuration, install ExtProc, or modify shared Grid, MaaS, or ingress
resources. Install the run-owned pool Gateways before this helper. Apply mode sets the
supported DSCI LLMInferenceService TLS field to the HTTP protocol required by this
InferencePool path and saves the original state for cleanup.
Modes: plan, platform (API/capacity/platform gates only), build, apply, verify,
collect, cleanup. Platform mode does not require the task namespace or pools.
API TLS bypass requires CLUSTER_ENV=ephemeral-dev and ALLOW_INSECURE_DEV_API=true.
PLAN
  exit 0
fi

for bin in oc jq awk sha256sum sed timeout python3; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
python3 -c 'import yaml' >/dev/null 2>&1 || { printf 'PyYAML is required for sanitized EPP Pod YAML evidence\n' >&2; exit 2; }
[[ "$MODE" =~ ^(platform|build|apply|verify|collect|cleanup)$ ]] || { printf 'unsupported mode: %s\n' "$MODE" >&2; exit 2; }
[[ "$SERVING_POOL_SELECTION" == both || "$SERVING_POOL_SELECTION" == pool-a || "$SERVING_POOL_SELECTION" == pool-b ]] || {
  printf 'SERVING_POOL_SELECTION must be both, pool-a, or pool-b\n' >&2; exit 2;
}
[[ "$CLUSTER_PLATFORM" == openshift || "$CLUSTER_PLATFORM" == kind ]] || { printf 'GRID_LLMD_CLUSTER_PLATFORM must be openshift or kind\n' >&2; exit 2; }
if [[ "$CLUSTER_PLATFORM" == kind ]]; then
  [[ "$CLUSTER_ENV" == kind ]] || { printf 'GRID_LLMD_CLUSTER_PLATFORM=kind requires CLUSTER_ENV=kind\n' >&2; exit 2; }
fi
[[ -n "$EXPECTED_API_SERVER" ]] || { printf 'EXPECTED_API_SERVER is required\n' >&2; exit 2; }
[[ -n "$KUBE_CONTEXT" ]] || { printf 'KUBE_CONTEXT is required for non-plan serving pool modes\n' >&2; exit 2; }
[[ "$RESOURCE_PROFILE" == strict || "$RESOURCE_PROFILE" == reduced-dev ]] || {
  printf 'RESOURCE_PROFILE must be strict or reduced-dev\n' >&2; exit 2;
}
if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev || ("$CLUSTER_PLATFORM" == kind && "$CLUSTER_ENV" == kind) ]] || {
    printf 'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev or the explicit Kind platform\n' >&2
    exit 2
  }
fi
[[ -n "$RUN_LABEL" ]] || { printf 'GRID_RUN_LABEL is required for task ownership checks\n' >&2; exit 2; }
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'GRID_RUN_LABEL must be a valid Kubernetes label value\n' >&2; exit 2; }
[[ "$LLMISVC_TLS_ENABLED" == false ]] || { printf 'This IPP/EPP simulator path requires LLMISVC_TLS_ENABLED=false; InferencePool supports http or h2c backends only\n' >&2; exit 2; }
[[ "$LLMISVC_GATEWAY_NAME" =~ ^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$ ]] || { printf 'LLMISVC_GATEWAY_NAME must be a DNS subdomain\n' >&2; exit 2; }
[[ "$LLMISVC_GATEWAY_NAMESPACE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || { printf 'LLMISVC_GATEWAY_NAMESPACE must be a DNS label\n' >&2; exit 2; }
[[ "$LLMISVC_GATEWAY_SECTION_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || { printf 'LLMISVC_GATEWAY_SECTION_NAME must be a DNS label\n' >&2; exit 2; }
for gateway_name in "$POOL_A_GATEWAY_NAME" "$POOL_B_GATEWAY_NAME"; do
  [[ "$gateway_name" =~ ^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$ ]] || {
    printf 'POOL_A_GATEWAY_NAME and POOL_B_GATEWAY_NAME must be DNS subdomains\n' >&2
    exit 2
  }
done
if [[ "$POOL_A_GATEWAY_NAME" == "$POOL_B_GATEWAY_NAME" ]]; then
  printf 'independent simulator pools require distinct task Gateway names\n' >&2
  exit 2
fi
[[ "$POOL_GATEWAY_LISTENER_PORT" =~ ^[0-9]+$ && "$POOL_GATEWAY_LISTENER_PORT" -ge 1 && "$POOL_GATEWAY_LISTENER_PORT" -le 65535 ]] || {
  printf 'POOL_GATEWAY_LISTENER_PORT must be a valid TCP port\n' >&2
  exit 2
}
[[ "$POOL_GATEWAY_INFRA_ANNOTATION_KEY" =~ ^[A-Za-z0-9.-]+/[A-Za-z0-9][-A-Za-z0-9_.]*$ &&
   "$POOL_GATEWAY_INFRA_ANNOTATION_VALUE" =~ ^[A-Za-z0-9][-A-Za-z0-9_.]*$ ]] || {
  printf 'pool Gateway infrastructure annotation is invalid\n' >&2
  exit 2
}
[[ "$SIMULATOR_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || { printf 'SIMULATOR_SOURCE_REVISION must be a full Git SHA\n' >&2; exit 2; }
[[ "$EPP_IMAGE" == *@sha256:* ]] || { printf 'EPP_IMAGE must use an immutable sha256 digest\n' >&2; exit 2; }
[[ "$EPP_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || { printf 'EPP_SOURCE_REVISION must be a full Git SHA\n' >&2; exit 2; }
if [[ -n "$EXTPROC_IMAGE" ]]; then
  [[ "$EXTPROC_IMAGE" == *@sha256:* ]] || { printf 'EXTPROC_IMAGE must use an immutable sha256 digest when supplied\n' >&2; exit 2; }
fi
for source_revision in "$EXTPROC_SOURCE_REVISION" "$GRID_GATEWAY_SOURCE_REVISION" "$ODH_GRID_OPERATOR_SOURCE_REVISION" "$MAAS_SOURCE_REVISION"; do
  [[ "$source_revision" =~ ^[0-9a-f]{40}$ ]] || { printf 'all declared source revisions must be full Git SHAs\n' >&2; exit 2; }
done
if [[ "$CLUSTER_PLATFORM" != kind ]]; then
  [[ "$GITOPS_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || { printf 'GITOPS_SOURCE_REVISION must be a full Git SHA outside Kind\n' >&2; exit 2; }
fi

verify_source_lock_revision() {
  local component="$1" revision="$2" locked_revision
  if [[ "$CLUSTER_PLATFORM" == kind && "$component" == gitops ]]; then
    printf 'GitOps is not used by the existing-platform Kind serving path; source pin is excluded\n' >&2
    return 0
  fi
  locked_revision="$(jq -er --arg component "$component" '.components[$component].resolved_sha // empty' "$SOURCE_LOCKS_FILE" 2>/dev/null || true)"
  [[ -n "$locked_revision" && "$locked_revision" == "$revision" ]] || {
    printf 'declared %s source revision does not match SOURCE_LOCKS_FILE\n' "$component" >&2
    return 1
  }
}

[[ -s "$SOURCE_LOCKS_FILE" ]] || { printf 'SOURCE_LOCKS_FILE must name the current run source-locks.json before simulator build or pool operations\n' >&2; exit 2; }
verify_source_lock_revision llm-d-sim "$SIMULATOR_SOURCE_REVISION"
verify_source_lock_revision epp "$EPP_SOURCE_REVISION"
verify_source_lock_revision extproc "$EXTPROC_SOURCE_REVISION"
verify_source_lock_revision gitops "$GITOPS_SOURCE_REVISION"
verify_source_lock_revision grid "$GRID_GATEWAY_SOURCE_REVISION"
verify_source_lock_revision grid-operator "$ODH_GRID_OPERATOR_SOURCE_REVISION"
verify_source_lock_revision maas "$MAAS_SOURCE_REVISION"
[[ "$BUILD_TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'SIMULATOR_BUILD_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
[[ "$BUILD_STATUS_SYNC_TIMEOUT" =~ ^[1-9][0-9]*s$ ]] || { printf 'SIMULATOR_BUILD_STATUS_SYNC_TIMEOUT must be an integer followed by s\n' >&2; exit 2; }
case "${BUILD_TIMEOUT: -1}" in
  s) BUILD_TIMEOUT_SECONDS="${BUILD_TIMEOUT%?}" ;;
  m) BUILD_TIMEOUT_SECONDS="$(( ${BUILD_TIMEOUT%?} * 60 ))" ;;
  h) BUILD_TIMEOUT_SECONDS="$(( ${BUILD_TIMEOUT%?} * 3600 ))" ;;
esac
BUILD_STATUS_SYNC_TIMEOUT_SECONDS="${BUILD_STATUS_SYNC_TIMEOUT%s}"
[[ "$WAIT_TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'SERVING_POOLS_WAIT_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
[[ "$SIMULATOR_CPU_REQUEST" =~ ^[0-9]+m$ && "$EPP_CPU_REQUEST" =~ ^[0-9]+m$ ]] || {
  printf 'simulator and EPP CPU requests must use explicit millicore quantities\n' >&2; exit 2;
}
if [[ -z "$EVIDENCE_DIR" ]]; then EVIDENCE_DIR="$REPO_ROOT/evidence/serving-pools-$(date -u +%Y%m%dT%H%M%SZ)"; fi
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists; choose a new UTC timestamp\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
CURRENT_PHASE=initialization

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  printf '%s\n' "$line" | tee -a "$EVIDENCE_DIR/RUN-LOG.md" "$REPO_ROOT/RUN-LOG.md"
}

on_error() {
  local code="$1" failed_phase="$CURRENT_PHASE" classification api_server api_probe
  trap - ERR
  case "$failed_phase" in
    simulator-build|simulator-image-provenance) classification='artifact/provenance or deployment automation' ;;
    capacity-preflight) classification='platform/capacity preflight' ;;
    platform-readiness) classification='platform readiness' ;;
    api-access) classification='authentication/environment' ;;
    *) classification='EPP/simulator or test harness' ;;
  esac
  api_server="$(timeout 10s oc --context="$KUBE_CONTEXT" whoami --show-server 2>&1 || true)"
  if [[ "$api_server" != "$EXPECTED_API_SERVER" ]]; then
    printf 'capturedAt=%s\nexpectedServer=%s\nobservedServer=%s\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$EXPECTED_API_SERVER" "$api_server" \
      >"$EVIDENCE_DIR/api-readyz-on-failure.txt"
    chmod 0600 "$EVIDENCE_DIR/api-readyz-on-failure.txt"
    case "$api_server" in
      *Unauthorized*|*"You must be logged in"*) classification='authentication/environment' ;;
      *) classification='platform/capacity (API endpoint unavailable or target mismatch)' ;;
    esac
  elif api_probe="$(timeout 10s oc --context="$KUBE_CONTEXT" get --raw=/readyz 2>&1)"; then
    printf 'capturedAt=%s\nserver=%s\nreadiness=%s\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$api_server" "$api_probe" \
      >"$EVIDENCE_DIR/api-readyz-on-failure.txt"
    chmod 0600 "$EVIDENCE_DIR/api-readyz-on-failure.txt"
    if [[ "$api_probe" != *ok* ]]; then
      classification='platform/readiness (API /readyz did not return ok)'
    fi
  else
    printf 'capturedAt=%s\nserver=%s\nprobeError=%s\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$api_server" "$api_probe" \
      >"$EVIDENCE_DIR/api-readyz-on-failure.txt"
    chmod 0600 "$EVIDENCE_DIR/api-readyz-on-failure.txt"
    case "$api_probe" in
      *Unauthorized*|*"You must be logged in"*) classification='authentication/environment' ;;
      *"dial tcp"*|*"no such host"*|*"i/o timeout"*|*"connection refused"*|*"Unable to connect"*)
        classification='platform/capacity (API endpoint unavailable)' ;;
    esac
  fi
  log "Serving pool mode $MODE stopped with exit=$code at phase=$failed_phase classification=$classification"
  {
    printf '# Serving pool bug capture\n\n'
    printf -- '- First failed phase: %s\n' "$failed_phase"
    printf -- '- Mode: %s\n' "$MODE"
    printf -- '- Exit code: %s\n' "$code"
    printf -- '- Classification: %s\n' "$classification"
    printf -- '- Simulator source: %s at %s\n' "$SIMULATOR_SOURCE_URL" "$SIMULATOR_SOURCE_REVISION"
    printf -- '- Simulator OCI source label: %s\n' "$SIMULATOR_OCI_SOURCE_URL"
    printf -- '- EPP source revision: %s\n' "$EPP_SOURCE_REVISION"
    printf -- '- Namespace: %s\n' "$NAMESPACE"
    printf -- '- Evidence: %s\n' "$EVIDENCE_DIR"
    printf '\nNo bearer token, Secret data, database URL, private key, or kubeconfig is recorded. No inference traffic is sent by this helper.\n'
  } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  if declare -F capture_after_failure >/dev/null; then
    capture_after_failure || log 'Post-failure resource collection was incomplete; see bounded evidence files'
  fi
  CURRENT_PHASE="$failed_phase"
  jq -n --arg mode "$MODE" --arg phase "$failed_phase" --arg evidence "$EVIDENCE_DIR" --arg classification "$classification" \
    --argjson exitCode "$code" '{mode:$mode,status:"blocked",phase:$phase,classification:$classification,exitCode:$exitCode,evidenceDirectory:$evidence,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
  {
    printf '# Final result\n\n'
    printf -- '- Status: BLOCKED\n- Mode: %s\n- First failed phase: %s\n- Classification: %s\n' "$MODE" "$failed_phase" "$classification"
    printf -- '- Inference traffic sent: no\n- Evidence directory: %s\n' "$EVIDENCE_DIR"
  } >"$EVIDENCE_DIR/FINAL-RESULT.md"
  exit "$code"
}
trap 'on_error $?' ERR

api_access() {
  local context user server ready insecure tls_mode
  CURRENT_PHASE=api-access
context="$KUBE_CONTEXT"
  user="$(oc whoami)"
  server="$(oc whoami --show-server)"
  ready="$(oc get --raw=/readyz)"
  [[ "$server" == "$EXPECTED_API_SERVER" ]] || { log "API server mismatch: expected $EXPECTED_API_SERVER"; return 1; }
  [[ "$ready" == *ok* ]] || { log 'API /readyz is not healthy'; return 1; }
  insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
  if [[ "$insecure" == true ]]; then
    [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
      log 'API access blocked: kubeconfig bypasses TLS without explicit ephemeral-dev opt-in'
      return 1
    }
    tls_mode=explicit-ephemeral-dev-unverified
  else
    tls_mode=kubeconfig-verified
  fi
  printf 'capturedAt=%s\nuser=%s\nserver=%s\ncontext=%s\nreadyz=%s\napiTlsMode=%s\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$user" "$server" "$context" "$ready" "$tls_mode" \
    >"$EVIDENCE_DIR/api-access-summary.txt"
  chmod 0600 "$EVIDENCE_DIR/api-access-summary.txt"
  log "API identity=$user server=$server readyz=ok tlsMode=$tls_mode"
}

verify_namespace() {
  local labels
  CURRENT_PHASE=task-namespace-ownership
  labels="$(oc get namespace "$NAMESPACE" -o json | jq -r '[.metadata.labels["app.kubernetes.io/part-of"],.metadata.labels["praxis.fast/qualification-run"]]|@tsv')"
  [[ "$labels" == $'grid-llmd-e2e\t'"$RUN_LABEL" ]] || {
    log "namespace $NAMESPACE does not have expected task ownership labels"
    return 1
  }
  oc get namespace "$NAMESPACE" -o json | jq '{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels}' >"$EVIDENCE_DIR/task-namespace.json"
  log "Task namespace ownership verified: $NAMESPACE run=$RUN_LABEL"
}

verify_platform() {
  local dir="${1:-$EVIDENCE_DIR/platform-preflight}" expected_api_server="$EXPECTED_API_SERVER"
  CURRENT_PHASE=platform-readiness
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    verify_kind_platform "$dir"
    return
  fi
  [[ -x "$PLATFORM_HELPER" ]] || { log "platform verifier is missing or not executable: $PLATFORM_HELPER"; return 1; }
  if ! CLUSTER_ENV="$CLUSTER_ENV" RESOURCE_PROFILE="$RESOURCE_PROFILE" ALLOW_INSECURE_DEV_API="$ALLOW_INSECURE_DEV_API" \
    EXPECTED_API_SERVER="$expected_api_server" COLD_DEPLOY_MODE=verify-platform \
    COLD_DEPLOY_EVIDENCE_DIR="$dir" \
    "$PLATFORM_HELPER" --mode verify-platform --expected-api-server "$expected_api_server"; then
    log "platform verifier returned nonzero for evidence path $dir"
    return 1
  fi
  if ! jq -e '.status=="PASS" and .failedGates==0' "$dir/platform-gate-summary.json" >/dev/null; then
    log "platform gate summary did not pass: $dir/platform-gate-summary.json"
    return 1
  fi
  log 'Fresh RHOAI/MaaS/Authorino/monitoring/webhook platform gate PASS'
}

verify_capacity_mode() {
  local dir="$EVIDENCE_DIR/capacity-preflight" expected_status=preflight-pass expected_baseline=REQUIRED
  local args=(--mode preflight --resource-profile "$RESOURCE_PROFILE" --expected-api-server "$EXPECTED_API_SERVER")
  CURRENT_PHASE=capacity-preflight
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    verify_kind_capacity
    return
  fi
  [[ -x "$PLATFORM_HELPER" ]] || { log "capacity preflight helper is missing or not executable: $PLATFORM_HELPER"; return 1; }
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    expected_status=preflight-pass-reduced-dev
    expected_baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE
  fi
  CLUSTER_ENV="$CLUSTER_ENV" RESOURCE_PROFILE="$RESOURCE_PROFILE" ALLOW_INSECURE_DEV_API="$ALLOW_INSECURE_DEV_API" \
    KUBE_CONTEXT="$KUBE_CONTEXT" \
    COLD_DEPLOY_MODE=preflight COLD_DEPLOY_EVIDENCE_DIR="$dir" \
    "$PLATFORM_HELPER" "${args[@]}"
  jq -e --arg status "$expected_status" --arg baseline "$expected_baseline" --arg profile "$RESOURCE_PROFILE" \
    '.status==$status and .capacityBaseline==$baseline and .resourceProfile==$profile' "$dir/report.json" >/dev/null || {
    log "capacity preflight did not record the required status=$expected_status baseline=$expected_baseline"
    return 1
  }
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    log 'DEV-SMOKE / REDUCED-CAPACITY preflight passed; worker size baseline waived, while Ready workers and zero Pending Pods remain required'
  else
    log 'Full worker capacity baseline preflight PASS'
  fi
}

capture_llmisvc_transport_state() {
  local inventory="$EVIDENCE_DIR/llmisvc-transport-workloads.json" dsc_json="$EVIDENCE_DIR/datasciencecluster-transport-before.json"
  local other_count original_present original_value resolved_value dsc_uid dsc_generation saved_uid
  mkdir -p "$(dirname "$TRANSPORT_STATE_FILE")"
  oc get llminferenceservices.serving.kserve.io -A -o json | jq '[.items[]|{
    namespace:.metadata.namespace,name:.metadata.name,
    qualificationRun:.metadata.labels["praxis.fast/qualification-run"]
  }]' >"$inventory"
  other_count="$(jq --arg ns "$NAMESPACE" --arg run "$RUN_LABEL" '[.[]|select(.namespace!=$ns or .qualificationRun!=$run)]|length' "$inventory")"
  [[ "$other_count" == 0 ]] || {
    log "refusing to change cluster-wide LLMInferenceService TLS with $other_count non-task LLMInferenceServices present"
    return 1
  }
  if [[ -e "$TRANSPORT_STATE_FILE" ]]; then
    jq -e --arg run "$RUN_LABEL" --argjson target "$LLMISVC_TLS_ENABLED" \
      '.runLabel==$run and .targetTLS==$target' "$TRANSPORT_STATE_FILE" >/dev/null || {
        log "saved LLMInferenceService transport state does not match this run: $TRANSPORT_STATE_FILE"
        return 1
      }
    oc get datasciencecluster "$DSC_NAME" -o json >"$EVIDENCE_DIR/datasciencecluster-transport-current.json"
    dsc_uid="$(jq -r '.metadata.uid // empty' "$EVIDENCE_DIR/datasciencecluster-transport-current.json")"
    saved_uid="$(jq -r '.source.uid // empty' "$TRANSPORT_STATE_FILE")"
    [[ -n "$saved_uid" && "$dsc_uid" == "$saved_uid" ]] || {
      log "saved transport state lacks the original DSCI UID or live DSCI identity changed; refusing to patch it"
      return 1
    }
  else
    oc get datasciencecluster "$DSC_NAME" -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},kserve:.spec.components.kserve}' >"$dsc_json"
    dsc_uid="$(jq -r '.metadata.uid // empty' "$dsc_json")"
    dsc_generation="$(jq -r '.metadata.generation // 0' "$dsc_json")"
    [[ -n "$dsc_uid" ]] || { log 'DataScienceCluster has no UID; refusing to save transport state'; return 1; }
    original_present="$(jq '(.kserve // {})|has("enableLLMInferenceServiceTLS")' "$dsc_json")"
    original_value="$(jq -c 'if (.kserve // {})|has("enableLLMInferenceServiceTLS") then .kserve.enableLLMInferenceServiceTLS else null end' "$dsc_json")"
    resolved_value="$(read_effective_llmisvc_tls)"
    [[ "$resolved_value" == true || "$resolved_value" == false ]] || {
      log 'could not determine the current resolved LLMInferenceService TLS setting'
      return 1
    }
    jq -n --arg run "$RUN_LABEL" --arg server "$EXPECTED_API_SERVER" --arg dsc "$DSC_NAME" \
      --arg uid "$dsc_uid" --argjson generation "$dsc_generation" \
      --arg capturedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
      --argjson target "$LLMISVC_TLS_ENABLED" --argjson wasSet "$original_present" \
      --argjson originalValue "$original_value" --argjson resolvedValue "$resolved_value" \
      '{runLabel:$run,apiServer:$server,capturedAt:$capturedAt,targetTLS:$target,source:{kind:"DataScienceCluster",name:$dsc,uid:$uid,generation:$generation,field:"spec.components.kserve.enableLLMInferenceServiceTLS"},original:{fieldPresent:$wasSet,fieldValue:$originalValue,resolvedConfigValue:$resolvedValue}}' \
      >"$TRANSPORT_STATE_FILE"
    chmod 0600 "$TRANSPORT_STATE_FILE"
  fi
  cp "$TRANSPORT_STATE_FILE" "$EVIDENCE_DIR/llmisvc-transport-state.json"
  chmod 0600 "$EVIDENCE_DIR/llmisvc-transport-state.json"
}

read_effective_llmisvc_tls() {
  # DSCI schema documents TLS as enabled when this optional field is unset.
  oc get datasciencecluster "$DSC_NAME" -o json | jq -r '
    (.spec.components.kserve // {}) as $kserve |
    if ($kserve|has("enableLLMInferenceServiceTLS")) then $kserve.enableLLMInferenceServiceTLS else true end
  '
}

wait_for_kserve_tls_reconcile() {
  local expected="$LLMISVC_TLS_ENABLED" attempt=0 current generation observed controller_ready
  while ((attempt<45)); do
    current="$(read_effective_llmisvc_tls)"
    generation="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.metadata.generation}')"
    observed="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.status.observedGeneration}')"
    controller_ready="$(oc get deployment kserve-controller-manager -n redhat-ods-applications -o json | jq -r '
      . as $deployment |
      ((.status.observedGeneration // 0) >= (.metadata.generation // 0)) and
      ((.status.readyReplicas // 0) == (.spec.replicas // 0)) and
      any(.status.conditions[]?;.type=="Available" and .status=="True")
    ')"
    if [[ "$observed" =~ ^[0-9]+$ && "$generation" =~ ^[0-9]+$ &&
          "$current" == "$expected" && "$observed" -ge "$generation" &&
          "$controller_ready" == true ]]; then
      return 0
    fi
    sleep 2
    attempt=$((attempt+1))
  done
  log "KServe did not reconcile DSCI generation/TLS=$expected with current ready controller within 90s"
  return 1
}

wait_for_dsc_field_observed() {
  local expected="$1" attempt=0 current generation observed
  while ((attempt<45)); do
    current="$(read_effective_llmisvc_tls)"
    generation="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.metadata.generation}')"
    observed="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.status.observedGeneration}')"
    if [[ "$current" == "$expected" && "$observed" =~ ^[0-9]+$ && "$observed" -ge "$generation" ]]; then
      return 0
    fi
    sleep 2
    attempt=$((attempt+1))
  done
  log "DataScienceCluster did not observe the restored LLMInferenceService TLS field=$expected within 90s"
  return 1
}

record_llmisvc_transport_state() {
  oc get datasciencecluster "$DSC_NAME" -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},kserve:{managementState:.spec.components.kserve.managementState,enableLLMInferenceServiceTLS:.spec.components.kserve.enableLLMInferenceServiceTLS,effectiveTLS:(if (.spec.components.kserve // {})|has("enableLLMInferenceServiceTLS") then .spec.components.kserve.enableLLMInferenceServiceTLS else true end)},status:{observedGeneration:.status.observedGeneration,conditions:[.status.conditions[]?|select(.type=="Ready" or .type=="KserveReady")|{type,status,reason}]}}' \
    >"$EVIDENCE_DIR/datasciencecluster-transport-after.json"
  oc get deployment kserve-controller-manager -n redhat-ods-applications -o json | jq '{metadata:{name:.metadata.name,generation:.metadata.generation},specReplicas:.spec.replicas,status:{observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:[.status.conditions[]?|select(.type=="Available")|{type,status,reason}]}}' \
    >"$EVIDENCE_DIR/kserve-controller-after.json"
  oc get application "$ARGO_APPLICATION_NAME" -n "$ARGO_APPLICATION_NAMESPACE" -o json 2>/dev/null | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace},sync:.status.sync.status,health:.status.health.status,revision:.status.sync.revision}' \
    >"$EVIDENCE_DIR/argo-transport-state.json" || printf '{"status":"application-not-found"}\n' >"$EVIDENCE_DIR/argo-transport-state.json"
}

configure_llmisvc_transport() {
  local current patch
  CURRENT_PHASE=llmisvc-transport-config
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    [[ "$LLMISVC_TLS_ENABLED" == false ]] || { log 'Kind serving probe currently requires the KServe runtime TLS setting to be disabled for the HTTP simulator backend'; return 1; }
    oc get configmap inferenceservice-config -n kserve -o json >"$EVIDENCE_DIR/kserve-inferenceservice-config.json"
    jq -e '(.data.ingress|fromjson) as $ingress | ($ingress.enableLLMInferenceServiceTLS//false)==false and ($ingress.urlScheme//"http")=="http"' \
      "$EVIDENCE_DIR/kserve-inferenceservice-config.json" >/dev/null || {
      log 'KServe inferenceservice-config does not resolve to HTTP with LLMInferenceService TLS disabled'; return 1;
    }
    jq -n --arg config "kserve/inferenceservice-config" --argjson tls "$LLMISVC_TLS_ENABLED" \
      '{clusterPlatform:"kind",configurationResource:$config,effectiveWorkloadTLS:$tls,clusterWideMutation:false,reason:"upstream Kind KServe uses its inferenceservice-config; no DSCI exists"}' \
      >"$EVIDENCE_DIR/kind-serving-transport.json"
    log 'Verified Kind KServe inferenceservice-config for HTTP simulator serving; no DSCI or shared transport mutation is used'
    return
  fi
  capture_llmisvc_transport_state
  current="$(read_effective_llmisvc_tls)"
  if [[ "$current" != "$LLMISVC_TLS_ENABLED" ]]; then
    patch="$(jq -n --argjson enabled "$LLMISVC_TLS_ENABLED" '{spec:{components:{kserve:{enableLLMInferenceServiceTLS:$enabled}}}}')"
    oc patch datasciencecluster "$DSC_NAME" --type=merge -p "$patch" >"$EVIDENCE_DIR/datasciencecluster-transport-patch.txt"
    log "Applied temporary ephemeral-cluster DSCI setting: $DSC_NAME spec.components.kserve.enableLLMInferenceServiceTLS=$LLMISVC_TLS_ENABLED"
  else
    log "Supported task transport setting already equals LLMISVC_TLS_ENABLED=$LLMISVC_TLS_ENABLED"
  fi
  if ! wait_for_kserve_tls_reconcile; then
    log 'DSCI field/controller reconciliation did not complete; restoring the saved original transport field before serving resources'
    if restore_llmisvc_transport; then
      log 'Original DSCI transport field restored after DSCI reconciliation timeout'
    else
      log 'Original DSCI transport restoration failed after reconciliation timeout; inspect saved UID/state and do not apply serving pools'
    fi
    CURRENT_PHASE=llmisvc-transport-config
    return 1
  fi
  record_llmisvc_transport_state
  log "DSCI transport field and current KServe controller generation reconciled: LLMInferenceService TLS=$LLMISVC_TLS_ENABLED; full readiness is checked next; Grid/Gateway TLS remains enabled"
}

verify_llmisvc_transport() {
  local dsc_value
  CURRENT_PHASE=llmisvc-transport-verification
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    configure_llmisvc_transport
    return
  fi
  capture_llmisvc_transport_state
  dsc_value="$(read_effective_llmisvc_tls)"
  [[ "$dsc_value" == "$LLMISVC_TLS_ENABLED" ]] || {
    log "LLMInferenceService DSCI TLS mismatch: DSCI=$dsc_value expected=$LLMISVC_TLS_ENABLED"
    return 1
  }
  wait_for_kserve_tls_reconcile
  record_llmisvc_transport_state
  log "LLMInferenceService DSCI TLS setting and current KServe controller verified: TLS=$LLMISVC_TLS_ENABLED"
}

ensure_no_llmisvc_before_transport_apply() {
  local count inventory task_owned_count
  CURRENT_PHASE=llmisvc-transport-ownership-gate
  inventory="$EVIDENCE_DIR/llmisvc-transport-workloads-before-apply.json"
  oc get llminferenceservices.serving.kserve.io -A -o json | jq '[.items[]|{
    namespace:.metadata.namespace,name:.metadata.name,
    qualificationRun:.metadata.labels["praxis.fast/qualification-run"]
  }]' >"$inventory"
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    count="$(jq --arg ns "$NAMESPACE" --arg run "$RUN_LABEL" '[.[]|select(.namespace!=$ns or .qualificationRun!=$run)]|length' "$inventory")"
    task_owned_count="$(jq --arg ns "$NAMESPACE" --arg run "$RUN_LABEL" '[.[]|select(.namespace==$ns and .qualificationRun==$run)]|length' "$inventory")"
    [[ "$count" == 0 ]] || {
      log "refusing to proceed with $count LLMInferenceServices outside this task namespace/run"; return 1;
    }
  else
    count="$(jq 'length' "$inventory")"
    [[ "$count" == 0 ]] || {
      log "refusing to change cluster-wide LLMInferenceService TLS while $count LLMInferenceService resources already exist; inspect/clean only exact task-owned resources first"
      return 1
    }
  fi
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    if [[ "$task_owned_count" == 0 ]]; then
      log 'No existing LLMInferenceServices; Kind KServe transport will be read from its current inferenceservice-config without cluster-wide mutation'
    else
      log "Found $task_owned_count LLMInferenceServices owned by this task/run; Kind KServe transport remains read-only with no cluster-wide mutation"
    fi
  else
    log 'No existing LLMInferenceServices; safe to change the supported cluster-wide DSCI transport field for this run'
  fi
}

verify_post_transport_platform_gate() {
  local dir="$EVIDENCE_DIR/platform-post-transport" gate_ok=false failed_phase=post-transport-platform-readiness
  CURRENT_PHASE=post-transport-platform-readiness
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    verify_kind_platform "$dir"
    return
  fi
  if verify_platform "$dir" && \
    jq -e 'any(.[];.type=="KserveReady" and .status=="True")' "$dir/rhoai-readiness.json" >/dev/null && \
    jq -e '
      . as $slices |
      ["kserve-webhook-server-service","llmisvc-webhook-server-service","odh-model-controller-webhook-service"] as $required |
      ([ $slices[].service ]|unique|sort)==($required|sort) and
      all($required[]; . as $service | any($slices[];.service==$service and .ready>0 and .total>0))
    ' "$dir/webhook-endpoints.json" >/dev/null && \
    jq -e 'length==0' "$dir/post-reconciliation-pending-pods.json" >/dev/null; then
    gate_ok=true
  fi
  if [[ "$gate_ok" == true ]]; then
    log 'Post-DSCI full platform gate PASS: KserveReady=True, all current webhook endpoints Ready, and zero Pending Pods'
    return 0
  fi
  log 'Post-DSCI readiness gate failed; attempting to restore the original DSCI transport field before any serving resource is applied'
  if restore_llmisvc_transport; then
    log 'Original DSCI transport field restored after post-patch readiness failure'
  else
    log 'Original DSCI transport restoration failed; inspect the saved UID/state and do not apply serving pools'
  fi
  CURRENT_PHASE="$failed_phase"
  return 1
}

restore_llmisvc_transport() {
  local current workload_count was_set original_value resolved current_uid saved_uid generation observed patch
  CURRENT_PHASE=llmisvc-transport-restore
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    jq -n --arg run "$RUN_LABEL" --arg server "$EXPECTED_API_SERVER" \
      '{runLabel:$run,apiServer:$server,clusterPlatform:"kind",restored:true,clusterWideMutation:false,reason:"no DataScienceCluster exists in the upstream Kind topology"}' \
      >"$EVIDENCE_DIR/kind-serving-transport-restore.json"
    log 'Kind has no DSCI serving-TLS field to restore; shared platform state was not mutated'
    return
  fi
  [[ -f "$TRANSPORT_STATE_FILE" ]] || { log 'No saved LLMInferenceService transport state; nothing to restore'; return 0; }
  jq -e --arg run "$RUN_LABEL" '.runLabel==$run' "$TRANSPORT_STATE_FILE" >/dev/null || {
    log 'refusing to restore LLMInferenceService transport state from a different task run'
    return 1
  }
  if ! oc get llminferenceservices.serving.kserve.io -A -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,qualificationRun:.metadata.labels["praxis.fast/qualification-run"]}]' \
    >"$EVIDENCE_DIR/llmisvc-transport-workloads-after-cleanup.json"; then
    log 'could not inventory LLMInferenceServices; refusing to restore cluster-wide DSCI transport state without that ownership check'
    return 1
  fi
  workload_count="$(jq 'length' "$EVIDENCE_DIR/llmisvc-transport-workloads-after-cleanup.json")"
  [[ "$workload_count" == 0 ]] || {
    log "kept LLMInferenceService TLS=$LLMISVC_TLS_ENABLED because $workload_count LLMInferenceServices remain"
    return 1
  }
  saved_uid="$(jq -r '.source.uid // empty' "$TRANSPORT_STATE_FILE")"
  if ! current_uid="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.metadata.uid}')"; then
    log 'could not read the live DataScienceCluster UID; refusing transport restoration'
    return 1
  fi
  [[ -n "$saved_uid" && "$current_uid" == "$saved_uid" ]] || {
    log 'refusing to restore DSCI transport state because the live DataScienceCluster UID differs from the saved run identity'
    return 1
  }
  current="$(read_effective_llmisvc_tls)"
  [[ "$current" == "$LLMISVC_TLS_ENABLED" ]] || {
    log "refusing to restore transport state because current DSCI value $current differs from this run's target $LLMISVC_TLS_ENABLED"
    return 1
  }
  was_set="$(jq -r '.original.fieldPresent' "$TRANSPORT_STATE_FILE")"
  original_value="$(jq -r '.original.fieldValue' "$TRANSPORT_STATE_FILE")"
  if [[ "$was_set" == true ]]; then
    patch="$(jq -n --argjson enabled "$original_value" '{spec:{components:{kserve:{enableLLMInferenceServiceTLS:$enabled}}}}')"
    if ! oc patch datasciencecluster "$DSC_NAME" --type=merge -p "$patch" >"$EVIDENCE_DIR/datasciencecluster-transport-restore.txt"; then
      log 'failed to patch the original DSCI transport value'
      return 1
    fi
    resolved="$original_value"
  else
    if ! oc patch datasciencecluster "$DSC_NAME" --type=json -p='[{"op":"remove","path":"/spec/components/kserve/enableLLMInferenceServiceTLS"}]' >"$EVIDENCE_DIR/datasciencecluster-transport-restore.txt"; then
      log 'failed to remove the temporary DSCI transport field and restore its original absent state'
      return 1
    fi
    resolved="$(jq -r '.original.resolvedConfigValue' "$TRANSPORT_STATE_FILE")"
  fi
  if ! wait_for_dsc_field_observed "$resolved"; then
    log 'restored DSCI transport field was not observed by the controller'
    return 1
  fi
  current="$(read_effective_llmisvc_tls)"
  [[ "$current" == "$resolved" ]] || { log "KServe did not restore TLS state to $resolved"; return 1; }
  generation="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.metadata.generation}')"
  observed="$(oc get datasciencecluster "$DSC_NAME" -o jsonpath='{.status.observedGeneration}')"
  jq -n --arg uid "$current_uid" --argjson generation "$generation" --argjson observed "$observed" \
    --argjson effectiveTLS "$resolved" '{uid:$uid,generation:$generation,observedGeneration:$observed,effectiveTLS:$effectiveTLS,restored:true}' \
    >"$EVIDENCE_DIR/datasciencecluster-transport-restore-status.json"
  if ! record_llmisvc_transport_state; then
    log 'could not save the verified post-restore DSCI/controller snapshot'
    return 1
  fi
  log "Restored original LLMInferenceService TLS state: fieldPresent=$was_set fieldValue=$original_value resolved=$resolved"
}

write_invocation() {
  local extproc_head extproc_branch helper_sha source_lock_sha
  [[ -f "$SOURCE_LOCKS_MD" ]] || { log "run-specific source lock is missing: $SOURCE_LOCKS_MD"; return 1; }
  extproc_head="$(git -C "$REPO_ROOT" rev-parse HEAD)"
  extproc_branch="$(git -C "$REPO_ROOT" branch --show-current)"
  helper_sha="$(sha256sum "$SCRIPT_DIR/deploy-serving-pools.sh" | awk '{print $1}')"
  source_lock_sha="$(sha256sum "$SOURCE_LOCKS_MD" | awk '{print $1}')"
  cat >"$EVIDENCE_DIR/INVOCATION.md" <<INVOCATION
# Serving pool invocation

- Captured: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- Cluster: $EXPECTED_API_SERVER
- Cluster environment: $CLUSTER_ENV
- Resource profile: $RESOURCE_PROFILE
- Result label: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'DEV-SMOKE / REDUCED-CAPACITY'; else printf 'STRICT / FULL-CAPACITY'; fi)
- Capacity baseline disposition: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'WAIVED_FOR_EPHEMERAL_DEV_SMOKE'; else printf 'REQUIRED'; fi)
- API TLS mode: recorded in api-access-summary.txt; no token or kubeconfig was recorded
- Task namespace/run: $NAMESPACE / $RUN_LABEL
- Serving-pool selection: $SERVING_POOL_SELECTION
- ExtProc checkout: $extproc_branch at $extproc_head; existing dirty worktree preserved
- Serving helper SHA-256: $helper_sha
- Run-specific source lock: $SOURCE_LOCKS_MD (sha256 $source_lock_sha)
- Run-specific source-lock JSON: $SOURCE_LOCKS_FILE
- Simulator source: $SIMULATOR_SOURCE_URL at $SIMULATOR_SOURCE_REVISION
- Simulator OCI source label: $SIMULATOR_OCI_SOURCE_URL
- EPP image: $EPP_IMAGE
- EPP source revision: $EPP_SOURCE_REVISION
- ExtProc runtime: $(if [[ -n "$EXTPROC_IMAGE" ]]; then printf 'image pin supplied but not deployed by this serving-pool helper; source lock revision for later phase: %s' "$EXTPROC_SOURCE_REVISION"; else printf 'not part of the serving-pool phase; source lock revision for later deployment: %s' "$EXTPROC_SOURCE_REVISION"; fi)
- GitOps source revision: $GITOPS_SOURCE_REVISION
- Grid Gateway source revision: $GRID_GATEWAY_SOURCE_REVISION
- ODH Grid operator source revision: $ODH_GRID_OPERATOR_SOURCE_REVISION
- MaaS source revision: $MAAS_SOURCE_REVISION
- Pools: $POOL_A, $POOL_B
- LLMInferenceService Gateway references: $LLMISVC_GATEWAY_NAMESPACE/$(pool_gateway_name "$POOL_A") and $LLMISVC_GATEWAY_NAMESPACE/$(pool_gateway_name "$POOL_B") section $LLMISVC_GATEWAY_SECTION_NAME
- LLMInferenceService internal TLS: $LLMISVC_TLS_ENABLED; original DSCI state: $TRANSPORT_STATE_FILE
- Build timeout: $BUILD_TIMEOUT
- Readiness timeout: $WAIT_TIMEOUT
- Simulator requests: $SIMULATOR_CPU_REQUEST CPU / $SIMULATOR_MEMORY_REQUEST memory
- EPP requests: $EPP_CPU_REQUEST CPU / $EPP_MEMORY_REQUEST memory
- Inference traffic: none from this helper
INVOCATION
  chmod 0600 "$EVIDENCE_DIR/INVOCATION.md"
  cp "$SOURCE_LOCKS_MD" "$EVIDENCE_DIR/source-lock.md"
  cp "$SOURCE_LOCKS_FILE" "$EVIDENCE_DIR/source-locks.json"
  chmod 0600 "$EVIDENCE_DIR/source-locks.json"
  chmod 0600 "$EVIDENCE_DIR/source-lock.md"
  write_image_availability
}

write_image_availability() {
  local sim_state=unresolved sim_target
  sim_target="$SIMULATOR_SOURCE_URL@$SIMULATOR_SOURCE_REVISION"
  if [[ "$SIMULATOR_IMAGE" == *@sha256:* ]]; then sim_state=resolved; fi
  if [[ "$sim_state" == resolved ]]; then sim_target="$sim_target -> $SIMULATOR_IMAGE"; fi
  cat >"$EVIDENCE_DIR/image-availability.md" <<AVAILABILITY
# Image availability

Captured: $(date -u +%Y-%m-%dT%H:%M:%SZ)

| Component | Immutable image or source target | Source relationship | State |
|---|---|---|---|
| Praxis ExtProc | $(if [[ -n "$EXTPROC_IMAGE" ]]; then printf '%s' "$EXTPROC_IMAGE"; else printf 'not supplied to serving-pool phase'; fi) | Source lock revision for later deployment: $EXTPROC_SOURCE_REVISION | Not deployed by this helper; build and verify the qualification image before Gateway traffic |
| llm-d EPP | $EPP_IMAGE | Release provenance maps to source $EPP_SOURCE_REVISION | Available; deployed into pool graphs only in apply mode |
| llm-d-sim | $sim_target | $(if [[ "$CLUSTER_PLATFORM" == kind ]]; then printf 'shared Docker/Podman builder verifies registry digest, OCI source/revision/context labels and amd64 metadata'; else printf 'BuildConfig pins the Git ref; final image must carry OCI source label %s, matching revision, and amd64 metadata' "$SIMULATOR_OCI_SOURCE_URL"; fi) | $sim_state |
AVAILABILITY
  chmod 0600 "$EVIDENCE_DIR/image-availability.md"
}

capture_inventory() {
  CURRENT_PHASE=inventory
  oc get nodes -o json | jq '[.items[]|{name:.metadata.name,roles:.metadata.labels,ready:any(.status.conditions[]?;.type=="Ready" and .status=="True"),allocatable:.status.allocatable}]' >"$EVIDENCE_DIR/nodes.json"
  oc get nodes -l node-role.kubernetes.io/worker -o json | jq '[.items[]|{name:.metadata.name,allocatableCPU:.status.allocatable.cpu,allocatableMemory:.status.allocatable.memory}]' >"$EVIDENCE_DIR/worker-capacity.json"
  oc describe nodes -l node-role.kubernetes.io/worker | awk '/^Name:|^Allocated resources:|^[[:space:]]+cpu[[:space:]]|^[[:space:]]+memory[[:space:]]/' >"$EVIDENCE_DIR/worker-allocated-resources.txt"
  oc get pods -A --field-selector=status.phase=Pending -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,node:.spec.nodeName,reason:.status.reason,message:.status.message}]' >"$EVIDENCE_DIR/pending-pods.json"
  oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json | jq '[.items[-100:][]?|{type,reason,message,lastTimestamp,involvedObject:{kind:.involvedObject.kind,name:.involvedObject.name}}]' >"$EVIDENCE_DIR/task-events.json"
  oc get llminferenceservice,inferencepools.inference.networking.k8s.io,service,endpointslice,deployment -n "$NAMESPACE" -o wide \
    >"$EVIDENCE_DIR/serving-resources.txt" 2>&1 || true
}

write_build_resources() {
  cat >"$EVIDENCE_DIR/simulator-imagestream.yaml" <<YAML
apiVersion: image.openshift.io/v1
kind: ImageStream
metadata:
  name: $SIMULATOR_IMAGESTREAM
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  lookupPolicy:
    local: false
YAML
  cat >"$EVIDENCE_DIR/simulator-buildconfig.yaml" <<YAML
apiVersion: build.openshift.io/v1
kind: BuildConfig
metadata:
  name: $SIMULATOR_BUILD_CONFIG
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
  annotations:
    praxis.fast/source-url: "$SIMULATOR_SOURCE_URL"
    praxis.fast/oci-source-url: "$SIMULATOR_OCI_SOURCE_URL"
    praxis.fast/source-revision: "$SIMULATOR_SOURCE_REVISION"
spec:
  runPolicy: Serial
  completionDeadlineSeconds: 3600
  source:
    type: Git
    git:
      uri: "$SIMULATOR_SOURCE_URL"
      ref: "$SIMULATOR_SOURCE_REVISION"
  strategy:
    type: Docker
    dockerStrategy:
      dockerfilePath: Dockerfile
      forcePull: true
      buildArgs:
      - name: TARGETOS
        value: linux
      - name: TARGETARCH
        value: amd64
  output:
    to:
      kind: ImageStreamTag
      name: "$SIMULATOR_IMAGESTREAM:$SIMULATOR_IMAGE_TAG"
    imageLabels:
    - name: org.opencontainers.image.source
      value: "$SIMULATOR_OCI_SOURCE_URL"
    - name: org.opencontainers.image.revision
      value: "$SIMULATOR_SOURCE_REVISION"
    - name: org.opencontainers.image.version
      value: "$SIMULATOR_SOURCE_REVISION"
  resources:
    requests:
      cpu: 100m
      memory: 512Mi
    limits:
      cpu: "1"
      memory: 2Gi
YAML
  chmod 0600 "$EVIDENCE_DIR/simulator-imagestream.yaml" "$EVIDENCE_DIR/simulator-buildconfig.yaml"
}

verify_task_ownership() {
  local kind="$1" name="$2" namespace="$3" current
  if oc get "$kind" "$name" -n "$namespace" -o json >/dev/null 2>&1; then
    current="$(oc get "$kind" "$name" -n "$namespace" -o json | jq -r '.metadata.labels["praxis.fast/qualification-run"] // ""')"
    [[ "$current" == "$RUN_LABEL" ]] || {
      log "$kind $namespace/$name exists without this run's ownership label; refusing to change it"
      return 1
    }
  fi
}

image_digest_from_tag() {
  local image_name digest revision source architecture
  image_name="$(oc get imagestreamtag "$SIMULATOR_IMAGESTREAM:$SIMULATOR_IMAGE_TAG" -n "$NAMESPACE" -o json | jq -r '.image.metadata.name // empty')"
  [[ "$image_name" =~ ^sha256:[0-9a-f]{64}$ ]] || { log 'simulator ImageStreamTag has no immutable sha256 identity'; return 1; }
  digest="$image_name"
  oc get image "$digest" -o json | jq '{name:.metadata.name,dockerImageReference:.dockerImageReference,labels:.dockerImageMetadata.Config.Labels,architecture:.dockerImageMetadata.Architecture}' >"$EVIDENCE_DIR/simulator-image-metadata.json"
  revision="$(jq -r '.labels["org.opencontainers.image.revision"] // empty' "$EVIDENCE_DIR/simulator-image-metadata.json")"
  source="$(jq -r '.labels["org.opencontainers.image.source"] // empty' "$EVIDENCE_DIR/simulator-image-metadata.json")"
  architecture="$(jq -r '.architecture // empty' "$EVIDENCE_DIR/simulator-image-metadata.json")"
  [[ "$revision" == "$SIMULATOR_SOURCE_REVISION" ]] || { log "simulator image revision label mismatch: $revision"; return 1; }
  [[ "$source" == "$SIMULATOR_OCI_SOURCE_URL" ]] || { log "simulator OCI source label mismatch: expected=$SIMULATOR_OCI_SOURCE_URL observed=$source"; return 1; }
  [[ "$architecture" == amd64 ]] || { log "simulator image architecture is not amd64: $architecture"; return 1; }
  SIMULATOR_IMAGE="image-registry.openshift-image-registry.svc:5000/$NAMESPACE/$SIMULATOR_IMAGESTREAM@$digest"
  printf '%s\n' "$SIMULATOR_IMAGE" >"$EVIDENCE_DIR/simulator-image-pin.txt"
  if ! grep -Fq -- '- Simulator image pin:' "$EVIDENCE_DIR/INVOCATION.md"; then
    printf -- '- Simulator image pin: %s\n' "$SIMULATOR_IMAGE" >>"$EVIDENCE_DIR/INVOCATION.md"
  fi
  write_image_availability
  log "Simulator artifact qualified: source=$SIMULATOR_SOURCE_REVISION digest=$digest"
}

verify_simulator_image_pin() {
  local requested="$SIMULATOR_IMAGE"
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    [[ "$requested" == *@sha256:* && -s "$SIMULATOR_IMAGE_PROVENANCE_FILE" ]] || {
      log 'Kind simulator image must be digest-pinned and have verified shared-builder provenance'; return 1;
    }
    jq -e --arg image "$requested" --arg revision "$SIMULATOR_SOURCE_REVISION" --arg source "$SIMULATOR_OCI_SOURCE_URL" \
      --slurpfile locks "$SOURCE_LOCKS_FILE" \
      '.verified==true and .image==$image and .source_sha==$revision and .source_sha==$locks[0].components["llm-d-sim"].resolved_sha and .build_context_sha256==$locks[0].components["llm-d-sim"].build_context_sha256 and .oci_revision_label==$revision and .oci_source_label==$source and .build_context_sha256==.oci_build_context_label' \
      "$SIMULATOR_IMAGE_PROVENANCE_FILE" >/dev/null || { log 'Kind simulator source/image provenance does not match this source lock'; return 1; }
    jq -e --arg image "$requested" --arg revision "$SIMULATOR_SOURCE_REVISION" --slurpfile locks "$SOURCE_LOCKS_FILE" \
      '.source_sha==$revision and .deployed_image==$image and .build_context_sha256==$locks[0].components["llm-d-sim"].build_context_sha256 and .build_platform=="linux/amd64"' \
      "$(dirname "$SIMULATOR_IMAGE_PROVENANCE_FILE")/build-metadata-llm-d-sim.json" >/dev/null || {
      log 'Kind simulator image build metadata does not match the source lock or amd64 platform'; return 1;
    }
    cp "$SIMULATOR_IMAGE_PROVENANCE_FILE" "$EVIDENCE_DIR/simulator-image-provenance.json"
    printf '%s\n' "$SIMULATOR_IMAGE" >"$EVIDENCE_DIR/simulator-image-pin.txt"
    write_image_availability
    log "Kind simulator artifact qualified from the source-locked build: $SIMULATOR_IMAGE"
    return
  fi
  image_digest_from_tag
  if [[ -n "$requested" && "$requested" != "$SIMULATOR_IMAGE" ]]; then
    log "requested simulator image does not match the qualified task ImageStream digest: requested=$requested qualified=$SIMULATOR_IMAGE"
    return 1
  fi
}

wait_for_build_terminal() {
  local build_name="$1" build_deadline_seconds="$2" phase wait_seconds deadline
  # Build execution can legitimately take the full build budget. The status
  # sync timeout is an additional allowance for the Build API to publish its
  # terminal phase after the original build deadline; it is not a second build
  # timeout when the log stream exits early.
  wait_seconds="$((build_deadline_seconds + BUILD_STATUS_SYNC_TIMEOUT_SECONDS - SECONDS))"
  ((wait_seconds > 0)) || wait_seconds="$BUILD_STATUS_SYNC_TIMEOUT_SECONDS"
  deadline=$((SECONDS + wait_seconds))
  while ((SECONDS <= deadline)); do
    phase="$(oc get build "$build_name" -n "$NAMESPACE" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
    case "$phase" in
      Complete) log "Build API reports $build_name Complete"; return 0 ;;
      Failed|Error|Cancelled|Canceled)
        log "Build API reports terminal phase=$phase for $build_name"
        return 1
        ;;
      *) sleep 2 ;;
    esac
  done
  log "Timed out waiting for Build API terminal phase for $build_name; last phase=${phase:-unknown}"
  return 1
}

build_simulator() {
  local tag_exists=false last_version build_name oc_bin phase build_deadline_seconds
  CURRENT_PHASE=simulator-build
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then
    [[ -n "$SIMULATOR_IMAGE_EVIDENCE_DIR" && -n "$SIMULATOR_IMAGE_REPOSITORY" && -n "$SIMULATOR_DEPLOY_IMAGE_REPOSITORY" ]] || {
      log 'Kind simulator build requires separate evidence, push repository, and in-cluster deployment repository inputs'; return 1;
    }
    [[ ! -e "$SIMULATOR_IMAGE_PROVENANCE_FILE" ]] || { log 'Kind simulator provenance already exists; refusing to overwrite a prior build attempt'; return 1; }
    SIMULATOR_IMAGE="$(env COMPONENT=llm-d-sim SOURCE_COMPONENT=llm-d-sim SOURCE_LOCKS_FILE="$SOURCE_LOCKS_FILE" \
      IMAGE_REPOSITORY="$SIMULATOR_IMAGE_REPOSITORY" DEPLOY_IMAGE_REPOSITORY="$SIMULATOR_DEPLOY_IMAGE_REPOSITORY" \
      E2E_RUN_ID="$RUN_LABEL" EVIDENCE_DIR="$SIMULATOR_IMAGE_EVIDENCE_DIR" DOCKERFILE=Dockerfile \
      CONTAINER_ENGINE="$CONTAINER_ENGINE" IMAGE_INSPECTOR="$IMAGE_INSPECTOR" \
      IMAGE_PUSH_TLS_VERIFY="$SIMULATOR_IMAGE_PUSH_TLS_VERIFY" ALLOW_INSECURE_IMAGE_PUSH="$ALLOW_INSECURE_IMAGE_PUSH" \
      CLUSTER_ENV=kind OCI_SOURCE_URL="$SIMULATOR_OCI_SOURCE_URL" \
      bash "$SCRIPT_DIR/../shared/build_and_push.sh" 2>"$EVIDENCE_DIR/simulator-image-builder-stderr.txt")"
    SIMULATOR_IMAGE_PROVENANCE_FILE="$SIMULATOR_IMAGE_EVIDENCE_DIR/image-provenance-llm-d-sim.json"
    verify_simulator_image_pin
    return
  fi
  write_build_resources
  oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/simulator-imagestream.yaml" -o json >"$EVIDENCE_DIR/imagestream-server-dry-run.json"
  oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/simulator-buildconfig.yaml" -o json >"$EVIDENCE_DIR/buildconfig-server-dry-run.json"
  verify_task_ownership imagestream "$SIMULATOR_IMAGESTREAM" "$NAMESPACE"
  verify_task_ownership buildconfig "$SIMULATOR_BUILD_CONFIG" "$NAMESPACE"
  oc apply --server-side --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/simulator-imagestream.yaml" >"$EVIDENCE_DIR/imagestream-apply.txt"
  oc apply --server-side --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/simulator-buildconfig.yaml" >"$EVIDENCE_DIR/buildconfig-apply.txt"
  if oc get imagestreamtag "$SIMULATOR_IMAGESTREAM:$SIMULATOR_IMAGE_TAG" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/simulator-imagestreamtag.json" 2>/dev/null; then
    if jq -e --arg revision "$SIMULATOR_SOURCE_REVISION" \
      '.image.dockerImageMetadata.Config.Labels["org.opencontainers.image.revision"]==$revision' \
      "$EVIDENCE_DIR/simulator-imagestreamtag.json" >/dev/null; then tag_exists=true; fi
  fi
  if [[ "$tag_exists" != true ]]; then
    last_version="$(oc get buildconfig "$SIMULATOR_BUILD_CONFIG" -n "$NAMESPACE" -o jsonpath='{.status.lastVersion}')"
    if [[ -n "$last_version" && "$last_version" != 0 ]]; then
      build_name="$SIMULATOR_BUILD_CONFIG-$last_version"
      phase="$(oc get build "$build_name" -n "$NAMESPACE" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
      if [[ "$phase" == New || "$phase" == Pending || "$phase" == Running ]]; then
        local active_source_revision active_started_at active_started_epoch active_elapsed_seconds active_remaining_seconds
        active_source_revision="$(oc get build "$build_name" -n "$NAMESPACE" -o json | jq -r '.spec.revision.git.commit // .spec.source.git.ref // empty')"
        [[ "$active_source_revision" == "$SIMULATOR_SOURCE_REVISION" ]] || {
          log "an active simulator Build already exists for a different source revision: build=$build_name phase=$phase"
          return 1
        }
        active_started_at="$(oc get build "$build_name" -n "$NAMESPACE" -o jsonpath='{.status.startTimestamp}')"
        [[ -n "$active_started_at" ]] || active_started_at="$(oc get build "$build_name" -n "$NAMESPACE" -o jsonpath='{.metadata.creationTimestamp}')"
        active_started_epoch="$(date -u -d "$active_started_at" +%s 2>/dev/null)" || {
          log "could not parse the active Build start timestamp for $build_name"
          return 1
        }
        active_elapsed_seconds="$(( $(date -u +%s) - active_started_epoch ))"
        active_remaining_seconds="$((BUILD_TIMEOUT_SECONDS - active_elapsed_seconds))"
        ((active_remaining_seconds > 0)) || active_remaining_seconds=0
        build_deadline_seconds=$((SECONDS + active_remaining_seconds))
        log "Adopting active Build $build_name for the pinned source revision; elapsed=${active_elapsed_seconds}s remaining-build-budget=${active_remaining_seconds}s; will not start a duplicate build"
      else
        build_name=""
      fi
    else
      build_name=""
    fi
    if [[ -z "$build_name" ]]; then
      log "Starting OpenShift build from immutable source revision $SIMULATOR_SOURCE_REVISION"
      oc_bin="$(type -P oc)"
      [[ -n "$oc_bin" ]] || { log 'could not resolve the oc executable for the bounded build command'; return 1; }
      build_deadline_seconds=$((SECONDS + BUILD_TIMEOUT_SECONDS))
      if timeout --foreground --signal=TERM --kill-after=30s "$BUILD_TIMEOUT" \
        "$oc_bin" --context="$KUBE_CONTEXT" start-build "buildconfig/$SIMULATOR_BUILD_CONFIG" -n "$NAMESPACE" --follow \
        >"$EVIDENCE_DIR/build-output.txt" 2>&1; then
        :
      else
        local build_status="$?"
        tail -n 80 "$EVIDENCE_DIR/build-output.txt" >"$EVIDENCE_DIR/build-output-tail.txt" || true
        last_version="$(oc get buildconfig "$SIMULATOR_BUILD_CONFIG" -n "$NAMESPACE" -o jsonpath='{.status.lastVersion}')"
        if [[ -n "$last_version" && "$last_version" != 0 ]]; then
          build_name="$SIMULATOR_BUILD_CONFIG-$last_version"
          phase="$(oc get build "$build_name" -n "$NAMESPACE" -o jsonpath='{.status.phase}' 2>/dev/null || true)"
        else
          phase=""
        fi
        case "$phase" in
          New|Pending|Running)
            log "OpenShift build-log stream exited with code=$build_status while build phase=$phase; preserve and continue watching $build_name without starting another build"
            log "bounded build output is in build-output-tail.txt"
            ;;
          Complete)
            log "OpenShift build-log stream exited with code=$build_status after the Build API reported Complete; continue immutable output verification"
            ;;
          *)
            log "OpenShift simulator build command failed with exit=$build_status and Build phase=${phase:-unknown}; bounded output is in build-output-tail.txt"
            return "$build_status"
            ;;
        esac
      fi
      tail -n 40 "$EVIDENCE_DIR/build-output.txt" >"$EVIDENCE_DIR/build-output-tail.txt"
    fi
  else
    log 'Existing source-labelled simulator image matches the pinned revision; build was skipped'
  fi
  last_version="$(oc get buildconfig "$SIMULATOR_BUILD_CONFIG" -n "$NAMESPACE" -o jsonpath='{.status.lastVersion}')"
  [[ -n "$last_version" && "$last_version" != 0 ]] || { log 'BuildConfig has no completed build version'; return 1; }
  build_name="$SIMULATOR_BUILD_CONFIG-$last_version"
  [[ -n "${build_deadline_seconds:-}" ]] || build_deadline_seconds="$SECONDS"
  wait_for_build_terminal "$build_name" "$build_deadline_seconds"
  oc get build "$build_name" -n "$NAMESPACE" -o json | jq '{name:.metadata.name,phase:.status.phase,source:.spec.source,gitCommit:(.spec.revision.git.commit // .spec.source.git.ref // .status.revision.git.commit // ""),output:.status.outputDockerImageReference,outputDigest:.status.output.to.imageDigest,reason:.status.reason,message:.status.message,conditions:.status.conditions}' >"$EVIDENCE_DIR/simulator-build-status.json"
  [[ "$(jq -r '.phase' "$EVIDENCE_DIR/simulator-build-status.json")" == Complete ]] || { log "simulator build $build_name did not complete"; return 1; }
  [[ "$(jq -r '.gitCommit // empty' "$EVIDENCE_DIR/simulator-build-status.json")" == "$SIMULATOR_SOURCE_REVISION" ]] || {
    log 'simulator Build did not report the pinned source commit; refusing to qualify its output'
    return 1
  }
  image_digest_from_tag
  log "Simulator build verified: $build_name; output is digest-pinned"
}

make_task_scheduler_config() {
  local source_file="$EVIDENCE_DIR/rhoai-scheduler-config-source.json"
  local target_file="$EVIDENCE_DIR/task-scheduler-config.json"
  CURRENT_PHASE=scheduler-config-render
  oc get llminferenceserviceconfig "$SCHEDULER_CONFIG_NAME" -n "$SCHEDULER_CONFIG_NAMESPACE" -o json >"$source_file"
  jq --arg namespace "$NAMESPACE" --arg run "$RUN_LABEL" --arg image "$EPP_IMAGE" \
     --arg cpuReq "$EPP_CPU_REQUEST" --arg memReq "$EPP_MEMORY_REQUEST" \
     --arg cpuLimit "$EPP_CPU_LIMIT" --arg memLimit "$EPP_MEMORY_LIMIT" \
     --arg source "$SCHEDULER_CONFIG_NAME" --arg clusterPlatform "$CLUSTER_PLATFORM" '
    del(.status,.metadata.uid,.metadata.resourceVersion,.metadata.generation,.metadata.creationTimestamp,.metadata.managedFields,.metadata.finalizers,.metadata.ownerReferences)
    | .metadata.namespace=$namespace
    | .metadata.labels=(.metadata.labels // {})
    | .metadata.labels["app.kubernetes.io/managed-by"]="grid-llmd-e2e-run"
    | .metadata.labels["app.kubernetes.io/part-of"]="grid-llmd-e2e"
    | .metadata.labels["praxis.fast/qualification-run"]=$run
    | .metadata.annotations=(.metadata.annotations // {})
    | .metadata.annotations["praxis.fast/copied-from-config"]=$source
    | .spec.router.scheduler.pool.spec.endpointPickerRef.failureMode="FailClose"
    | (.spec.router.scheduler.template.containers[]|select(.name=="main")|.image)=$image
    | (.spec.router.scheduler.template.containers[]|select(.name=="main")|.resources)={requests:{cpu:$cpuReq,memory:$memReq},limits:{cpu:$cpuLimit,memory:$memLimit}}
    | (.spec.router.scheduler.template.containers[]|select(.name=="main")|.command) |= map(select(contains("model-server-metrics-scheme")|not))
    | if $clusterPlatform=="kind" then
        (.spec.router.scheduler.template.containers[]|select(.name=="main")|.command) |=
          (map(select(.!="--secure-serving=false"))+["--secure-serving=false"])
      else . end
  ' "$source_file" >"$target_file"
  chmod 0600 "$source_file" "$target_file"
  jq -e --arg image "$EPP_IMAGE" --arg cpu "$EPP_CPU_REQUEST" --arg mem "$EPP_MEMORY_REQUEST" '
    .spec.router.scheduler.template.containers[]|select(.name=="main")|
    .image==$image and .resources.requests.cpu==$cpu and .resources.requests.memory==$mem
  ' "$target_file" >/dev/null
  sha256sum "$source_file" "$target_file" >"$EVIDENCE_DIR/scheduler-config-digests.txt"
  oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e -f "$target_file" -o json >"$EVIDENCE_DIR/scheduler-config-server-dry-run.json"
  verify_task_ownership llminferenceserviceconfig "$SCHEDULER_CONFIG_NAME" "$NAMESPACE"
  log "Task scheduler config rendered from read-only RHOAI source; EPP pinned to $EPP_IMAGE"
}

write_control_configmap() {
  local pool="$1" oracle="$2" file config
  file="$EVIDENCE_DIR/$pool-simulator-control.yaml"
  config="$EVIDENCE_DIR/$pool-simulator-config.yaml"
  cat >"$config" <<CONFIG
model: "$MODEL_NAME"
served-model-name:
- "$oracle"
- "$MODEL_NAME"
mode: echo
port: 8000
fake-metrics:
  running-requests: 0
  waiting-requests: 0
  kv-cache-usage: 0
enable-request-id-headers: true
CONFIG
  chmod 0600 "$config"
  cat >"$file" <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: $pool-simulator-control
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
    praxis.fast/simulator-pool: "$pool"
data:
  config.yaml: |
$(sed 's/^/    /' "$config")
YAML
  chmod 0600 "$file"
}

write_llmisvc() {
  local pool="$1" oracle="$2" file hash gateway_name
  file="$EVIDENCE_DIR/$pool-llmisvc.yaml"
  gateway_name="$(pool_gateway_name "$pool")"
  hash="$(sha256sum "$EVIDENCE_DIR/$pool-simulator-config.yaml" | awk '{print $1}')"
  cat >"$file" <<YAML
apiVersion: serving.kserve.io/v1alpha2
kind: LLMInferenceService
metadata:
  name: $pool
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
    praxis.fast/simulator-pool: "$pool"
    praxis.fast/response-oracle: "$oracle"
  annotations:
    serving.kserve.io/config-llm-scheduler: "$SCHEDULER_CONFIG_NAME"
spec:
  model:
    name: "$MODEL_NAME"
    uri: file:///models
  storageInitializer:
    enabled: false
  replicas: 1
  labels:
    praxis.fast/qualification-run: "$RUN_LABEL"
    praxis.fast/simulator-pool: "$pool"
  annotations:
    praxis.fast/simulator-control-sha256: "$hash"
  router:
    route: {}
    gateway:
      refs:
      - name: "$gateway_name"
        namespace: "$LLMISVC_GATEWAY_NAMESPACE"
        sectionName: "$LLMISVC_GATEWAY_SECTION_NAME"
    scheduler:
      replicas: 1
      labels:
        praxis.fast/qualification-run: "$RUN_LABEL"
        praxis.fast/simulator-pool: "$pool"
  template:
    containers:
    - name: main
      image: "$SIMULATOR_IMAGE"
      imagePullPolicy: "$SIMULATOR_PULL_POLICY"
      command:
      - /app/llm-d-inference-sim
      args:
      - --config
      - /etc/llm-d-sim/config.yaml
      - --v=4
      ports:
      - name: http
        containerPort: 8000
        protocol: TCP
      resources:
        requests:
          cpu: "$SIMULATOR_CPU_REQUEST"
          memory: "$SIMULATOR_MEMORY_REQUEST"
        limits:
          cpu: "$SIMULATOR_CPU_LIMIT"
          memory: "$SIMULATOR_MEMORY_LIMIT"
      volumeMounts:
      - name: simulator-control
        mountPath: /etc/llm-d-sim
        readOnly: true
      livenessProbe:
        httpGet:
          path: /health
          port: 8000
        initialDelaySeconds: 5
        periodSeconds: 10
      readinessProbe:
        httpGet:
          path: /health
          port: 8000
        initialDelaySeconds: 3
        periodSeconds: 5
      securityContext:
        allowPrivilegeEscalation: false
        capabilities:
          drop:
          - ALL
        runAsNonRoot: true
        seccompProfile:
          type: RuntimeDefault
    volumes:
    - name: simulator-control
      configMap:
        name: $pool-simulator-control
YAML
  chmod 0600 "$file"
}

capture_pool() {
  local pool="$1" output uid control_file control_hash
  output="$EVIDENCE_DIR/$pool-snapshot.json"
  oc get llminferenceservice "$pool" -n "$NAMESPACE" -o json | jq '{
    metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},
    spec:{model:.spec.model,storageInitializer:.spec.storageInitializer,replicas:.spec.replicas,router:.spec.router,labels:.spec.labels,simulatorControlSha256:.spec.annotations["praxis.fast/simulator-control-sha256"],template:{containers:[.spec.template.containers[]?|{name,image,imagePullPolicy,resources,command,args,ports,volumeMounts,livenessProbe,readinessProbe}],volumes:.spec.template.volumes}},
    status:{observedGeneration:.status.observedGeneration,conditions:.status.conditions,appliedConfigs:.status.appliedConfigs,workloads:.status.workloads,router:.status.router}
  }' >"$output"
  uid="$(jq -r '.metadata.uid' "$output")"
  oc get deployments -n "$NAMESPACE" -o json | jq --arg uid "$uid" '
    [.items[]|select(any(.metadata.ownerReferences[]?;.uid==$uid))|{
      metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
      spec:{replicas:.spec.replicas,selector:.spec.selector,containers:[.spec.template.spec.containers[]?|{name,image,resources,ports}]},
      status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}
    }]
  ' >"$EVIDENCE_DIR/$pool-owned-deployments.json"
  oc get services -n "$NAMESPACE" -o json | jq --arg prefix "$pool" '
    [.items[]|select(.metadata.name|startswith($prefix))|{
      metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
      spec:{type:.spec.type,selector:.spec.selector,ports:.spec.ports}
    }]
  ' >"$EVIDENCE_DIR/$pool-services.json"
  oc get inferencepools.inference.networking.k8s.io -n "$NAMESPACE" -o json | jq --arg prefix "$pool" '
    [.items[]|select(.metadata.name|startswith($prefix))|{
      metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
      spec:.spec,status:.status
    }]
  ' >"$EVIDENCE_DIR/$pool-inferencepools.json"
  if oc get inferencepools.inference.networking.x-k8s.io -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/$pool-inferencepools-alpha2-raw.json" 2>/dev/null; then
    jq --arg prefix "$pool" '[.items[]|select(.metadata.name|startswith($prefix))|{
      metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
      spec:.spec,status:.status
    }]' "$EVIDENCE_DIR/$pool-inferencepools-alpha2-raw.json" >"$EVIDENCE_DIR/$pool-inferencepools-alpha2.json"
    rm -f "$EVIDENCE_DIR/$pool-inferencepools-alpha2-raw.json"
  else
    printf '[]\n' >"$EVIDENCE_DIR/$pool-inferencepools-alpha2.json"
  fi
  oc get httproutes.gateway.networking.k8s.io -n "$NAMESPACE" -o json | jq --arg uid "$uid" '
    [.items[]|select(any(.metadata.ownerReferences[]?;.uid==$uid))|{
      metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
      spec:.spec,status:.status
    }]
  ' >"$EVIDENCE_DIR/$pool-http-routes.json"
  oc get endpointslices -n "$NAMESPACE" -o json | jq --arg prefix "$pool" '
    [.items[]|select(.metadata.labels["kubernetes.io/service-name"]|startswith($prefix))|{
      name:.metadata.name,service:.metadata.labels["kubernetes.io/service-name"],
      endpoints:[.endpoints[]?|{conditions:.conditions,targetRef:.targetRef,addresses:.addresses}]
    }]
  ' >"$EVIDENCE_DIR/$pool-endpointslices.json"
  oc get pods -n "$NAMESPACE" -o json | jq --arg pool "$pool" '
    [.items[]|select(.metadata.labels["praxis.fast/simulator-pool"]==$pool)|{
      metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences,annotations:.metadata.annotations},
      phase:.status.phase,conditions:.status.conditions,
      volumes:.spec.volumes,
      containers:[.spec.containers[]?|{name,image,volumeMounts}],
      containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]
    }]
  ' >"$EVIDENCE_DIR/$pool-pods.json"
  control_file="$EVIDENCE_DIR/$pool-simulator-control-live.json"
  oc get configmap "$pool-simulator-control" -n "$NAMESPACE" -o json | jq '{
    metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,resourceVersion:.metadata.resourceVersion,labels:.metadata.labels},
    data:.data
  }' >"$control_file"
  control_hash="$(jq -j -r '.data["config.yaml"]' "$control_file" | sha256sum | awk '{print $1}')"
  jq -n --arg pool "$pool" --arg namespace "$NAMESPACE" --arg image "$SIMULATOR_IMAGE" \
    --arg revision "$SIMULATOR_SOURCE_REVISION" --arg epp "$EPP_IMAGE" --arg controlHash "$control_hash" \
    '{pool:$pool,namespace:$namespace,simulatorImage:$image,simulatorSourceRevision:$revision,eppImage:$epp,simulatorControlSha256:$controlHash,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/$pool-artifact-identity.json"
}

apply_one_pool() {
  local pool="$1" oracle="$2" uid attempt=0 ready=false
  CURRENT_PHASE="apply-$pool"
  write_control_configmap "$pool" "$oracle"
  write_llmisvc "$pool" "$oracle"
  oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/$pool-simulator-control.yaml" -o json >"$EVIDENCE_DIR/$pool-control-server-dry-run.json"
  oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/$pool-llmisvc.yaml" -o json >"$EVIDENCE_DIR/$pool-llmisvc-server-dry-run.json"
  verify_task_ownership configmap "$pool-simulator-control" "$NAMESPACE"
  verify_task_ownership llminferenceservice "$pool" "$NAMESPACE"
  oc apply --server-side --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/$pool-simulator-control.yaml" >"$EVIDENCE_DIR/$pool-control-apply.txt"
  oc apply --server-side --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/$pool-llmisvc.yaml" >"$EVIDENCE_DIR/$pool-llmisvc-apply.txt"
  log "Applied task-owned $pool LLMInferenceService and isolated simulator control; no request was sent"
  oc wait "llminferenceservice/$pool" -n "$NAMESPACE" --for=condition=Ready --timeout="$WAIT_TIMEOUT" >"$EVIDENCE_DIR/$pool-llmisvc-wait.txt"
  uid="$(oc get llminferenceservice "$pool" -n "$NAMESPACE" -o json | jq -r '.metadata.uid')"
  while ((attempt < 90)); do
    if oc get deployments -n "$NAMESPACE" -o json | jq -e --arg uid "$uid" '
      [.items[]|select(any(.metadata.ownerReferences[]?;.uid==$uid))]
      | length>=2 and all(.[];
        (.spec.replicas//0)>0 and
        (.status.observedGeneration//0)>=(.metadata.generation//0) and
        (.status.readyReplicas//0)==(.spec.replicas//0) and
        any(.status.conditions[]?;.type=="Available" and .status=="True"))
    ' >/dev/null; then ready=true; break; fi
    sleep 5
    attempt=$((attempt + 1))
  done
  [[ "$ready" == true ]] || { log "$pool child Deployments did not all become Available at the current generation"; return 1; }
  capture_pool "$pool"
  log "$pool LLMInferenceService and all owned Deployments are Ready"
}

verify_pool_a_ready_before_pool_b() {
  local snapshot="$EVIDENCE_DIR/pool-a-before-pool-b.json"
  CURRENT_PHASE='pool-b-dependency-gate'
  oc get llminferenceservice "$POOL_A" -n "$NAMESPACE" -o json >"$snapshot"
  verify_task_ownership llminferenceservice "$POOL_A" "$NAMESPACE"
  jq -e --arg run "$RUN_LABEL" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .status.observedGeneration==.metadata.generation and
    any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==.metadata.generation)
  ' "$snapshot" >/dev/null || { log 'Pool B cannot be applied before the run-owned Pool A LLMInferenceService is current and Ready'; return 1; }
}

capture_pool_membership() {
  local pool="$1"
  oc get pods -n "$NAMESPACE" -o json | jq --arg pool "$pool" '[.items[]|select(.metadata.labels["praxis.fast/simulator-pool"]==$pool)|{
    metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,annotations:.metadata.annotations},
    phase:.status.phase,conditions:.status.conditions,volumes:.spec.volumes,
    containers:[.spec.containers[]?|{name,image,volumeMounts}],
    containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]
  }]' >"$EVIDENCE_DIR/$pool-pods.json"
  oc get endpointslices -n "$NAMESPACE" -o json | jq --arg prefix "$pool" '[.items[]|select(.metadata.labels["kubernetes.io/service-name"]|startswith($prefix))|{
    name:.metadata.name,service:.metadata.labels["kubernetes.io/service-name"],
    endpoints:[.endpoints[]?|{conditions:.conditions,targetRef:.targetRef,addresses:.addresses}]
  }]' >"$EVIDENCE_DIR/$pool-endpointslices.json"
}

verify_pool_graph() {
  local pool="$1" oracle="$2" snapshot gateway_name
  snapshot="$EVIDENCE_DIR/$pool-snapshot.json"
  gateway_name="$(pool_gateway_name "$pool")"
  local sim_digest epp_digest backend_service epp_service control_hash control_capture_hash sim_pod_uids endpoint_uids route_group uid_attempt=0
  sim_digest="$(printf '%s' "$SIMULATOR_IMAGE" | sed 's/^.*@//')"
  epp_digest="$(printf '%s' "$EPP_IMAGE" | sed 's/^.*@//')"
  backend_service="$pool-kserve-workload-svc"
  epp_service="$pool-epp-service"
  control_hash="$(oc get configmap "$pool-simulator-control" -n "$NAMESPACE" -o json | jq -j -r '.data["config.yaml"]' | sha256sum | awk '{print $1}')"
  control_capture_hash="$(jq -j -r '.data["config.yaml"]' "$EVIDENCE_DIR/$pool-simulator-control-live.json" | sha256sum | awk '{print $1}')"
  [[ "$control_hash" == "$control_capture_hash" && "$control_hash" == "$(jq -r '.simulatorControlSha256' "$EVIDENCE_DIR/$pool-artifact-identity.json")" ]] || {
    log "$pool live simulator control digest differs from captured metadata"
    return 1
  }
  jq -e --arg pool "$pool" --arg run "$RUN_LABEL" '
    .metadata.name==($pool+"-simulator-control") and
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .metadata.labels["praxis.fast/simulator-pool"]==$pool and
    (.data["config.yaml"]|type)=="string"
  ' "$EVIDENCE_DIR/$pool-simulator-control-live.json" >/dev/null || { log "$pool simulator control ConfigMap ownership/content check failed"; return 1; }
  jq -e --arg pool "$pool" --arg digest "$sim_digest" --arg hash "$control_hash" \
    --arg gateway "$gateway_name" --arg gatewayNamespace "$LLMISVC_GATEWAY_NAMESPACE" \
    --arg sectionName "$LLMISVC_GATEWAY_SECTION_NAME" \
    --arg cpu "$SIMULATOR_CPU_REQUEST" --arg memory "$SIMULATOR_MEMORY_REQUEST" \
    --arg schedulerConfig "$SCHEDULER_CONFIG_NAME" --arg namespace "$NAMESPACE" --arg run "$RUN_LABEL" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .metadata.labels["praxis.fast/simulator-pool"]==$pool and
    .status.observedGeneration==.metadata.generation and
    any(.status.conditions[]?;.type=="Ready" and .status=="True") and
    any(.status.appliedConfigs[]?;.name==$schedulerConfig and .namespace==$namespace) and
    any(.spec.template.containers[]?;.name=="main" and (.image|endswith($digest)) and .resources.requests.cpu==$cpu and .resources.requests.memory==$memory) and
    (.spec.router.scheduler.replicas//0)==1 and
    (.spec.router.scheduler.labels["praxis.fast/simulator-pool"]==$pool) and
    any(.spec.router.gateway.refs[]?;.name==$gateway and .namespace==$gatewayNamespace and .sectionName==$sectionName) and
    .spec.simulatorControlSha256==$hash
  ' "$snapshot" >/dev/null || { log "$pool LLMInferenceService generation/readiness/config/image/control checks failed"; return 1; }
  jq -e --arg pool "$pool" --arg gateway "$gateway_name" --arg gatewayNamespace "$LLMISVC_GATEWAY_NAMESPACE" \
    --arg sectionName "$LLMISVC_GATEWAY_SECTION_NAME" --arg controllerName "$KSERVE_ROUTE_CONTROLLER_NAME" '
    length>=1 and any(.[]|select(.metadata.name==($pool+"-kserve-route")); . as $route |
      any(.spec.parentRefs[]?;.name==$gateway and (.namespace//.metadata.namespace)==$gatewayNamespace and .sectionName==$sectionName) and
      any(.spec.rules[]?.backendRefs[]?;.kind=="InferencePool" and ((.group//"")=="inference.networking.k8s.io" or (.group//"")=="inference.networking.x-k8s.io") and .name==($pool+"-inference-pool")) and
      any(.status.parents[]?;
        .controllerName==$controllerName and
        .parentRef.name==$gateway and (.parentRef.namespace//$gatewayNamespace)==$gatewayNamespace and
        any(.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$route.metadata.generation) and
        any(.conditions[]?;.type=="ResolvedRefs" and .status=="True" and .observedGeneration==$route.metadata.generation)
      )
    )
  ' "$EVIDENCE_DIR/$pool-http-routes.json" >/dev/null || { log "$pool KServe HTTPRoute is absent, stale, rejected, unresolved, or not attached to the configured task Gateway"; return 1; }
  jq -e --arg oracle "$oracle" '.data["config.yaml"]|contains("- \""+$oracle+"\"")' \
    "$EVIDENCE_DIR/$pool-simulator-control-live.json" >/dev/null || { log "$pool simulator control lacks its response oracle"; return 1; }
  jq -e '
    length>=2 and all(.[];
      (.spec.replicas//0)>0 and
      (.status.observedGeneration//0)>=(.metadata.generation//0) and
      (.status.readyReplicas//0)==(.spec.replicas//0) and
      any(.status.conditions[]?;.type=="Available" and .status=="True"))
  ' "$EVIDENCE_DIR/$pool-owned-deployments.json" >/dev/null || { log "$pool does not have two ready, current-generation owned Deployments"; return 1; }
  jq -e --arg epp "$EPP_IMAGE" 'any(.[].spec.containers[]?;.image==$epp)' \
    "$EVIDENCE_DIR/$pool-owned-deployments.json" >/dev/null || { log "$pool EPP Deployment image differs from the pin"; return 1; }
  jq -e --arg simDigest "$sim_digest" --arg eppDigest "$epp_digest" --arg pool "$pool" --arg hash "$control_hash" '
    any(.[].containerStatuses[]?;.ready==true and (.imageID|endswith($simDigest))) and
    any(.[].containerStatuses[]?;.ready==true and (.imageID|endswith($eppDigest))) and
    any(.[];
      .metadata.annotations["praxis.fast/simulator-control-sha256"]==$hash and
      any(.volumes[]?;.name=="simulator-control" and .configMap.name==($pool+"-simulator-control")) and
      any(.containers[]?;.name=="main" and any(.volumeMounts[]?;.name=="simulator-control" and .mountPath=="/etc/llm-d-sim")) and
      (if .metadata.labels["app.kubernetes.io/component"]=="llminferenceservice-workload" then
        any(.volumes[]?;.name=="tls-certs" and .secret.secretName==($pool+"-kserve-self-signed-certs")) and
        any(.containers[]?;.name=="main" and any(.volumeMounts[]?;.name=="tls-certs" and .mountPath=="/var/run/kserve/tls"))
       else true end)
    )
  ' "$EVIDENCE_DIR/$pool-pods.json" >/dev/null || { log "$pool simulator control mount/digest or runtime image checks failed"; return 1; }
  jq -e --argjson enabled "$LLMISVC_TLS_ENABLED" '
    .data["config.yaml"] as $config |
    if $enabled then
      ($config|contains("ssl-certfile: /var/run/kserve/tls/tls.crt")) and
      ($config|contains("ssl-keyfile: /var/run/kserve/tls/tls.key"))
    else
      (($config|contains("ssl-certfile:"))|not) and (($config|contains("ssl-keyfile:"))|not)
    end
  ' "$EVIDENCE_DIR/$pool-simulator-control-live.json" >/dev/null || {
    log "$pool simulator protocol config does not match LLMISVC_TLS_ENABLED=$LLMISVC_TLS_ENABLED"
    return 1
  }
  jq -e --arg backend "$backend_service" --arg epp "$epp_service" '
    any(.[].name==$backend) and any(.[].name==$epp)
  ' "$EVIDENCE_DIR/$pool-services.json" >/dev/null || { log "$pool backend or EPP Service is absent"; return 1; }
  jq -e --arg backend "$backend_service" --arg epp "$epp_service" '
    any(.[]|select(.service==$backend);any(.endpoints[]?;.conditions.ready==true and .targetRef.kind=="Pod" and (.targetRef.uid//"")!="")) and
    any(.[]|select(.service==$epp);any(.endpoints[]?;.conditions.ready==true and .targetRef.kind=="Pod" and (.targetRef.uid//"")!=""))
  ' "$EVIDENCE_DIR/$pool-endpointslices.json" >/dev/null || { log "$pool backend/EPP EndpointSlices lack ready Pod targets"; return 1; }
  jq -e --arg pool "$pool" '
    any(.[];.metadata.name==($pool+"-inference-pool") and
      .spec.endpointPickerRef.name==($pool+"-epp-service") and
      .spec.endpointPickerRef.failureMode=="FailClose" and
      .spec.appProtocol=="http" and
      any(.spec.targetPorts[]?;.number==8000))
  ' "$EVIDENCE_DIR/$pool-inferencepools.json" >/dev/null || { log "$pool InferencePool has the wrong EPP reference/failure mode"; return 1; }
  route_group="$(jq -r --arg pool "$pool" '
    [.[]|select(.metadata.name==($pool+"-kserve-route"))|.spec.rules[]?.backendRefs[]?|select(.kind=="InferencePool" and .name==($pool+"-inference-pool"))|.group][0] // ""
  ' "$EVIDENCE_DIR/$pool-http-routes.json")"
  case "$route_group" in
    inference.networking.x-k8s.io)
      jq -e --arg pool "$pool" --arg gateway "$gateway_name" --arg namespace "$LLMISVC_GATEWAY_NAMESPACE" '
        any(.[]|select(.metadata.name==($pool+"-inference-pool")); . as $poolObject | any(.status.parent[]?;
          .parentRef.name==$gateway and .parentRef.namespace==$namespace and
          any(.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$poolObject.metadata.generation) and
          any(.conditions[]?;.type=="ResolvedRefs" and .status=="True" and .observedGeneration==$poolObject.metadata.generation)
        ))
      ' "$EVIDENCE_DIR/$pool-inferencepools-alpha2.json" >/dev/null || { log "$pool legacy InferencePool API parent status is not current Accepted/ResolvedRefs"; return 1; }
      ;;
    inference.networking.k8s.io)
      jq -e --arg pool "$pool" --arg gateway "$gateway_name" --arg namespace "$LLMISVC_GATEWAY_NAMESPACE" '
        any(.[]|select(.metadata.name==($pool+"-inference-pool")); . as $poolObject | any(.status.parents[]?;
          .parentRef.name==$gateway and (.parentRef.namespace//$namespace)==$namespace and
          any(.conditions[]?;.type=="Accepted" and .status=="True" and .observedGeneration==$poolObject.metadata.generation) and
          any(.conditions[]?;.type=="ResolvedRefs" and .status=="True" and .observedGeneration==$poolObject.metadata.generation)
        ))
      ' "$EVIDENCE_DIR/$pool-inferencepools.json" >/dev/null || { log "$pool InferencePool v1 parent status is not current Accepted/ResolvedRefs"; return 1; }
      ;;
    *) log "$pool HTTPRoute references an unsupported InferencePool API group: ${route_group:-missing}"; return 1 ;;
  esac
  jq -e --arg pool "$pool" --arg backend "$backend_service" '
    any(.[];.metadata.name==$backend and .spec.selector["app.kubernetes.io/name"]==$pool)
  ' "$EVIDENCE_DIR/$pool-services.json" >/dev/null || { log "$pool backend Service selector is not unique to its simulator pool"; return 1; }
  jq -e --arg pool "$pool" '
    any(.[];.metadata.name==($pool+"-inference-pool") and .spec.selector.matchLabels["app.kubernetes.io/name"]==$pool)
  ' "$EVIDENCE_DIR/$pool-inferencepools.json" >/dev/null || { log "$pool InferencePool selector is not unique to its workload"; return 1; }
  while ((uid_attempt<30)); do
    capture_pool_membership "$pool"
    sim_pod_uids="$(jq -r --arg digest "$sim_digest" '
      [.[]|select(.phase=="Running" and any(.conditions[]?;.type=="Ready" and .status=="True") and any(.containerStatuses[]?;.name=="main" and .ready==true and (.imageID|endswith($digest))))|.metadata.uid]|unique|sort|join("\n")
    ' "$EVIDENCE_DIR/$pool-pods.json")"
    endpoint_uids="$(jq -r --arg backend "$backend_service" '
      [.[]|select(.service==$backend)|.endpoints[]?|select(.conditions.ready==true and .targetRef.kind=="Pod")|.targetRef.uid//empty]|unique|sort|join("\n")
    ' "$EVIDENCE_DIR/$pool-endpointslices.json")"
    [[ -n "$sim_pod_uids" && -n "$endpoint_uids" && "$sim_pod_uids" == "$endpoint_uids" ]] && break
    sleep 2
    uid_attempt=$((uid_attempt+1))
  done
  [[ -n "$sim_pod_uids" && -n "$endpoint_uids" && "$sim_pod_uids" == "$endpoint_uids" ]] || {
    log "$pool backend EndpointSlice target UID set differs from its Ready simulator Pod UID set"
    return 1
  }
  log "$pool backend EndpointSlice target UID set matches its Ready simulator Pod UID set"
  log "$pool LLMInferenceService, InferencePool/EPP/backend graph, runtime images, control mount, and response oracle verified"
}

verify_all() {
  local pool_a_uids pool_b_uids overlap pool oracle
  CURRENT_PHASE=serving-graph-verification
  for pool in "$POOL_A" "$POOL_B"; do
    if [[ "$pool" == "$POOL_A" ]]; then oracle="$POOL_A_ORACLE"; else oracle="$POOL_B_ORACLE"; fi
    capture_pool "$pool"
    verify_pool_graph "$pool" "$oracle"
  done
  pool_a_uids="$(jq -r '[.[]|select(.service=="'"$POOL_A"'-kserve-workload-svc")|.endpoints[]?|select(.conditions.ready==true)|.targetRef.uid]|unique|sort|join(",")' "$EVIDENCE_DIR/$POOL_A-endpointslices.json")"
  pool_b_uids="$(jq -r '[.[]|select(.service=="'"$POOL_B"'-kserve-workload-svc")|.endpoints[]?|select(.conditions.ready==true)|.targetRef.uid]|unique|sort|join(",")' "$EVIDENCE_DIR/$POOL_B-endpointslices.json")"
  [[ -n "$pool_a_uids" && -n "$pool_b_uids" ]] || { log 'at least one simulator Pod UID set is missing'; return 1; }
  overlap="$(comm -12 <(tr ',' '\n' <<<"$pool_a_uids" | sort -u) <(tr ',' '\n' <<<"$pool_b_uids" | sort -u) | paste -sd, -)"
  [[ -z "$overlap" ]] || { log "simulator Pod UID sets overlap: $overlap"; return 1; }
  jq -n --arg poolA "$POOL_A" --arg poolB "$POOL_B" \
    --arg uidsA "$pool_a_uids" --arg uidsB "$pool_b_uids" \
    '{status:"PASS",poolA:{name:$poolA,readySimulatorPodUIDs:($uidsA|split(","))},poolB:{name:$poolB,readySimulatorPodUIDs:($uidsB|split(","))},disjoint:true,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/serving-graph-result.json"
  log "Two pool serving graph PASS; simulator Pod UIDs are disjoint; no inference traffic sent"
}

capture_after_failure() {
  CURRENT_PHASE=collect
  capture_inventory || true
  for pool in "$POOL_A" "$POOL_B"; do
    if oc get llminferenceservice "$pool" -n "$NAMESPACE" -o json >/dev/null 2>&1; then
      capture_pool "$pool" || true
      capture_epp_diagnostics "$pool" || true
    fi
  done
  oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json |
    jq '[.items[-150:][]?|{type,reason,message,lastTimestamp,involvedObject:{kind:.involvedObject.kind,name:.involvedObject.name}}]' |
    python3 "$SCRIPT_DIR/sanitize_diagnostics.py" >"$EVIDENCE_DIR/events-bounded.json" || true
  oc get pods -n "$NAMESPACE" -o json | jq '[.items[]|{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,nodeName:.spec.nodeName,phase:.status.phase,reason:.status.reason,conditions:[.status.conditions[]?|{type,status,reason}],images:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]' >"$EVIDENCE_DIR/pods-bounded.json" || true
}

capture_epp_diagnostics() {
  local pool="$1" dir pods_file deployment_file service_file service_name pod container
  dir="$EVIDENCE_DIR/$pool-epp-diagnostics"
  mkdir -p "$dir"
  chmod 0700 "$dir"
  pods_file="$dir/pods-sanitized.json"
  oc get pods -n "$NAMESPACE" -o json |
    jq --arg pool "$pool" --arg epp "$EPP_IMAGE" '[.items[]|
      select((any(.spec.containers[]?;.image==$epp)) or (.metadata.name|startswith($pool+"-kserve-router-scheduler")))|
      {metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
       spec:{nodeName:.spec.nodeName,serviceAccountName:.spec.serviceAccountName,containers:[.spec.containers[]?|{name,image,imagePullPolicy,resources,ports,readinessProbe,livenessProbe,startupProbe,volumeMounts,env:[.env[]?|{name,source:(.valueFrom//null),literalValuePresent:(has("value") and (.value|length>0))}]}],
         initContainers:[.spec.initContainers[]?|{name,image,imagePullPolicy,resources,ports,readinessProbe,livenessProbe,startupProbe,volumeMounts,env:[.env[]?|{name,source:(.valueFrom//null),literalValuePresent:(has("value") and (.value|length>0))}]}],
         volumes:[.spec.volumes[]?|{name,configMap:(.configMap.name//null),secret:(.secret.secretName//null),projected:(if .projected then "present" else null end),emptyDir:(if .emptyDir then true else null end)}]},
       status:{phase:.status.phase,podIP:.status.podIP,hostIP:.status.hostIP,conditions:[.status.conditions[]?|{type,status,reason,lastTransitionTime}],
         containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount,state:{waitingReason:.state.waiting.reason,runningStartedAt:.state.running.startedAt,terminatedReason:.state.terminated.reason,exitCode:.state.terminated.exitCode},lastState:{terminatedReason:.lastState.terminated.reason,exitCode:.lastState.terminated.exitCode,finishedAt:.lastState.terminated.finishedAt}}],
         initContainerStatuses:[.status.initContainerStatuses[]?|{name,image,imageID,ready,restartCount,state:{waitingReason:.state.waiting.reason,runningStartedAt:.state.running.startedAt,terminatedReason:.state.terminated.reason,exitCode:.state.terminated.exitCode},lastState:{terminatedReason:.lastState.terminated.reason,exitCode:.lastState.terminated.exitCode,finishedAt:.lastState.terminated.finishedAt}}]}}]' >"$pods_file"
  python3 - "$pods_file" "$dir/pods-sanitized.yaml" <<'PY'
import json, sys
from pathlib import Path
import yaml

source, target = map(Path, sys.argv[1:])
with source.open(encoding="utf-8") as stream:
    value = json.load(stream)
target.write_text(yaml.safe_dump(value, sort_keys=False), encoding="utf-8")
PY
  chmod 0600 "$pods_file" "$dir/pods-sanitized.yaml"

  deployment_file="$dir/deployments-sanitized.json"
  oc get deployments -n "$NAMESPACE" -o json |
    jq --arg pool "$pool" '[.items[]|select(.metadata.name|startswith($pool))|{
      metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},
      spec:{nodeSelector:.spec.template.spec.nodeSelector,affinity:.spec.template.spec.affinity,replicas:.spec.replicas,containers:[.spec.template.spec.containers[]?|{name,image,resources,ports,readinessProbe,livenessProbe,startupProbe}]},
      status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:[.status.conditions[]?|{type,status,reason,lastTransitionTime,lastUpdateTime,observedGeneration} ]}}]' >"$deployment_file"

  service_name="$pool-epp-service"
  service_file="$dir/epp-service.json"
  if oc get service "$service_name" -n "$NAMESPACE" -o json >"$dir/epp-service-live.json" 2>/dev/null; then
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},spec:{type:.spec.type,selector:.spec.selector,ports:.spec.ports},clusterIP:.spec.clusterIP}' \
      "$dir/epp-service-live.json" >"$service_file"
    oc get endpointslices -n "$NAMESPACE" -l "kubernetes.io/service-name=$service_name" -o json |
      jq '[.items[]|{metadata:{name:.metadata.name,labels:.metadata.labels},addressType,ports,endpoints:[.endpoints[]?|{addresses,conditions,targetRef:{kind:.targetRef.kind,name:.targetRef.name,uid:.targetRef.uid}}]}]' >"$dir/epp-endpointslices.json"
  else
    printf 'EPP Service %s not found\n' "$service_name" >"$dir/epp-service-missing.txt"
  fi

  oc get llminferenceservice "$pool" -n "$NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},status:{observedGeneration:.status.observedGeneration,conditions:[.status.conditions[]?|{type,status,reason,lastTransitionTime,observedGeneration}],appliedConfigs:.status.appliedConfigs,workloads:.status.workloads,router:.status.router}}' >"$dir/llminferenceservice-status.json"
  if oc get inferencepool "$pool-inference-pool" -n "$NAMESPACE" -o json >"$dir/inferencepool-live.json" 2>/dev/null; then
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:.spec,status:{observedGeneration:.status.observedGeneration,conditions:[.status.conditions[]?|{type,status,reason,lastTransitionTime,observedGeneration}],endpointPicker:.status.endpointPicker}}' \
      "$dir/inferencepool-live.json" >"$dir/inferencepool-status.json"
  fi

  oc get clusteroperators -o json |
    jq '[.items[]|{name:.metadata.name,conditions:[.status.conditions[]?|{type,status,reason,lastTransitionTime}]}]' >"$dir/clusteroperators.json"
  oc get --raw=/readyz >"$dir/apiserver-readyz.txt" 2>&1 || true
  oc get pods -A -o json |
    jq '[.items[]|select(.status.phase=="Pending" or .status.phase=="Failed" or .status.phase=="Unknown")|
      {namespace:.metadata.namespace,name:.metadata.name,phase:.status.phase,reason:.status.reason,nodeName:.spec.nodeName,conditions:[.status.conditions[]?|{type,status,reason}]}]' >"$dir/pending-failed-pods.json"
  oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json |
    jq '[.items[-150:][]?|{type,reason,message,lastTimestamp,involvedObject:{kind:.involvedObject.kind,name:.involvedObject.name}}]' |
    python3 "$SCRIPT_DIR/sanitize_diagnostics.py" >"$dir/events.json"

  jq -r '.[].metadata.name' "$pods_file" | while IFS= read -r pod; do
    [[ -n "$pod" ]] || continue
    oc describe pod "$pod" -n "$NAMESPACE" 2>&1 |
      python3 "$SCRIPT_DIR/sanitize_diagnostics.py" >"$dir/$pod-describe-sanitized.txt" || true
    jq -r --arg pod "$pod" '.[]|select(.metadata.name==$pod)|.spec.containers[]?.name' "$pods_file" |
      while IFS= read -r container; do
        [[ -n "$container" ]] || continue
        oc logs "$pod" -n "$NAMESPACE" -c "$container" --since=20m --tail=1000 2>&1 |
          python3 "$SCRIPT_DIR/sanitize_diagnostics.py" >"$dir/$pod-$container-logs-sanitized.txt" || true
      done
  done
  find "$dir" -maxdepth 1 -type f -exec chmod 0600 {} +
  log "$pool EPP failure diagnostics captured: sanitized Pod YAML/describe/logs, probes, deployment conditions, service/endpoints, imageIDs, placement/resources, events, API/ClusterOperator readiness, and pool status"
}

cleanup_owned() {
  CURRENT_PHASE=task-owned-cleanup
  verify_platform
  local pool
  for pool in "$POOL_A" "$POOL_B"; do
    if oc get llminferenceservice "$pool" -n "$NAMESPACE" -o json >/dev/null 2>&1; then
      verify_task_ownership llminferenceservice "$pool" "$NAMESPACE"
      oc delete llminferenceservice "$pool" -n "$NAMESPACE" --wait=true --timeout="$WAIT_TIMEOUT"
      log "Deleted task-owned LLMInferenceService $NAMESPACE/$pool; controller-owned children are garbage-collected"
    fi
    if oc get configmap "$pool-simulator-control" -n "$NAMESPACE" -o json >/dev/null 2>&1; then
      verify_task_ownership configmap "$pool-simulator-control" "$NAMESPACE"
      oc delete configmap "$pool-simulator-control" -n "$NAMESPACE" --wait=true --timeout=120s
      log "Deleted task-owned simulator control ConfigMap $NAMESPACE/$pool-simulator-control"
    fi
  done
  if oc get llminferenceserviceconfig "$SCHEDULER_CONFIG_NAME" -n "$NAMESPACE" -o json >/dev/null 2>&1; then
    verify_task_ownership llminferenceserviceconfig "$SCHEDULER_CONFIG_NAME" "$NAMESPACE"
    oc delete llminferenceserviceconfig "$SCHEDULER_CONFIG_NAME" -n "$NAMESPACE" --wait=true --timeout=120s
    log "Deleted task-owned scheduler config $NAMESPACE/$SCHEDULER_CONFIG_NAME"
  fi
  restore_llmisvc_transport
}

api_access
write_invocation
if [[ "$MODE" == platform ]]; then
  verify_capacity_mode
  verify_platform
  log 'Platform-only readiness gate passed; task namespace and serving-pool graph are not required at this phase'
  exit 0
fi
if [[ "$MODE" == build || "$MODE" == apply ]]; then verify_capacity_mode; fi
verify_namespace
capture_inventory
case "$MODE" in
  collect)
    capture_after_failure
    log 'Read-only collect completed; inference traffic was not sent'
    ;;
  build)
    verify_platform
    build_simulator
    ;;
  apply)
    verify_platform
    verify_pool_gateways_ready
    ensure_no_llmisvc_before_transport_apply
    configure_llmisvc_transport
    verify_post_transport_platform_gate
    CURRENT_PHASE=simulator-image-provenance
    verify_simulator_image_pin
    [[ "$SIMULATOR_IMAGE" == *@sha256:* ]] || { log 'SIMULATOR_IMAGE is not digest-pinned'; exit 1; }
    make_task_scheduler_config
    oc apply --server-side --field-manager=grid-llmd-e2e -f "$EVIDENCE_DIR/task-scheduler-config.json" >"$EVIDENCE_DIR/scheduler-config-apply.txt"
    log "Applied owned scheduler config $NAMESPACE/$SCHEDULER_CONFIG_NAME; source config in $SCHEDULER_CONFIG_NAMESPACE was read only"
    case "$SERVING_POOL_SELECTION" in
      pool-a)
        write_control_configmap "$POOL_A" "$POOL_A_ORACLE"
        verify_task_ownership configmap "$POOL_A-simulator-control" "$NAMESPACE"
        apply_one_pool "$POOL_A" "$POOL_A_ORACLE"
        capture_pool "$POOL_A"
        verify_pool_graph "$POOL_A" "$POOL_A_ORACLE"
        log 'Pool A setup graph is ready; Pool B was not created so its authorized setup probe remains a separate gate'
        ;;
      pool-b)
        verify_pool_a_ready_before_pool_b
        write_control_configmap "$POOL_B" "$POOL_B_ORACLE"
        verify_task_ownership configmap "$POOL_B-simulator-control" "$NAMESPACE"
        apply_one_pool "$POOL_B" "$POOL_B_ORACLE"
        verify_all
        ;;
      both)
        write_control_configmap "$POOL_A" "$POOL_A_ORACLE"
        write_control_configmap "$POOL_B" "$POOL_B_ORACLE"
        verify_task_ownership configmap "$POOL_A-simulator-control" "$NAMESPACE"
        verify_task_ownership configmap "$POOL_B-simulator-control" "$NAMESPACE"
        apply_one_pool "$POOL_A" "$POOL_A_ORACLE"
        apply_one_pool "$POOL_B" "$POOL_B_ORACLE"
        verify_all
        ;;
    esac
    ;;
  verify)
    verify_platform
    verify_pool_gateways_ready
    verify_llmisvc_transport
    CURRENT_PHASE=simulator-image-provenance
    verify_simulator_image_pin
    verify_all
    ;;
  cleanup)
    cleanup_owned
    capture_inventory
    log 'Task-owned serving cleanup completed; Grid, RHOAI, MaaS, Gateway, and operator resources were preserved'
    ;;
esac

if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
  RESULT_LABEL='DEV-SMOKE / REDUCED-CAPACITY'
  if [[ "$CLUSTER_PLATFORM" == kind ]]; then CAPACITY_BASELINE=WAIVED_FOR_KIND_DEV_SMOKE; else CAPACITY_BASELINE=WAIVED_FOR_EPHEMERAL_DEV_SMOKE; fi
else
  RESULT_LABEL='STRICT / FULL-CAPACITY'
  CAPACITY_BASELINE=REQUIRED
fi
jq -n --arg mode "$MODE" --arg namespace "$NAMESPACE" --arg evidence "$EVIDENCE_DIR" --arg poolSelection "$SERVING_POOL_SELECTION" \
  --arg resourceProfile "$RESOURCE_PROFILE" --arg capacityBaseline "$CAPACITY_BASELINE" \
  --arg resultLabel "$RESULT_LABEL" \
  '{mode:$mode,status:"complete",namespace:$namespace,servingPoolSelection:$poolSelection,evidenceDirectory:$evidence,resourceProfile:$resourceProfile,resultLabel:$resultLabel,capacityBaseline:$capacityBaseline,inferenceTrafficSent:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
case "$MODE" in
  build) FINAL_STATUS='SIMULATOR_IMAGE_BUILT; serving pools not applied' ;;
  apply)
    case "$SERVING_POOL_SELECTION" in
      pool-a) FINAL_STATUS='POOL_A_GRAPH_READY; Pool B and setup probe not run' ;;
      pool-b|both) FINAL_STATUS='SERVING_GRAPH_READY; traffic not qualified' ;;
    esac
    ;;
  verify) FINAL_STATUS='SERVING_GRAPH_READY; traffic not qualified' ;;
  collect) FINAL_STATUS=DIAGNOSTICS_COLLECTED ;;
  cleanup) FINAL_STATUS=TASK_OWNED_SERVING_RESOURCES_CLEANED ;;
esac
printf '# Final result\n\n- Status: %s\n- Resource profile: %s\n- Result label: %s\n- Capacity baseline: %s\n- Inference traffic sent: no\n- Evidence directory: %s\n' \
  "$FINAL_STATUS" "$RESOURCE_PROFILE" "$RESULT_LABEL" "$CAPACITY_BASELINE" "$EVIDENCE_DIR" \
  >"$EVIDENCE_DIR/FINAL-RESULT.md"
log "Serving pool mode $MODE completed; inference traffic sent=false"
find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
