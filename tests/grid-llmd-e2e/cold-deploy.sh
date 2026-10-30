#!/usr/bin/env bash
# shellcheck disable=SC2016
# SPDX-License-Identifier: Apache-2.0
# Repeatable cold platform bootstrap; inference qualification remains in xtask.
set -Eeuo pipefail
umask 077

env_default() {
  local name="$1" fallback="$2" value
  value="$(printenv "$name" 2>/dev/null || true)"
  if [[ -z "$value" ]]; then printf '%s' "$fallback"; else printf '%s' "$value"; fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="$(env_default COLD_DEPLOY_MODE plan)"
EVIDENCE_DIR="$(env_default COLD_DEPLOY_EVIDENCE_DIR '')"
RUN_LOG_PATH="$(env_default COLD_DEPLOY_RUN_LOG_PATH "$REPO_ROOT/RUN-LOG.md")"
ODH_GITOPS_DIR="$(env_default ODH_GITOPS_DIR '')"
ODH_GITOPS_REVISION="$(env_default ODH_GITOPS_REVISION '')"
PLATFORM_VALUES_FILE="$(env_default PLATFORM_VALUES_FILE "$SCRIPT_DIR/cold-platform-values.yaml")"
MAAS_SOURCE_DIR="$(env_default MAAS_SOURCE_DIR '')"
MAAS_SOURCE_REVISION="$(env_default MAAS_SOURCE_REVISION '')"
EXPECTED_API_SERVER="$(env_default EXPECTED_API_SERVER '')"
KUBE_CONTEXT="$(env_default KUBE_CONTEXT '')"
CLUSTER_ENV="$(env_default CLUSTER_ENV '')"
ALLOW_INSECURE_DEV_API="$(env_default ALLOW_INSECURE_DEV_API false)"
RESOURCE_PROFILE="$(env_default RESOURCE_PROFILE strict)"
CHART_PROFILE=maas
PROMETHEUS_CPU_REQUEST=100m
THANOS_CPU_REQUEST=50m
ALLOW_POC_DATABASE="$(env_default ALLOW_POC_DATABASE false)"
MIN_READY_WORKERS="$(env_default MIN_READY_WORKERS 2)"
MIN_FULL_SIZE_WORKERS="$(env_default MIN_FULL_SIZE_WORKERS 2)"
MIN_TOTAL_WORKER_CPU="$(env_default MIN_TOTAL_WORKER_CPU 20)"
MIN_WORKER_CPU="$(env_default MIN_WORKER_CPU 8)"
MIN_WORKER_MEMORY_GIB="$(env_default MIN_WORKER_MEMORY_GIB 32)"
INSTALL_TIMEOUT_SECONDS="$(env_default INSTALL_TIMEOUT_SECONDS 1800)"
PLATFORM_TIMEOUT_SECONDS="$(env_default PLATFORM_TIMEOUT_SECONDS 3600)"
MONITORING_OVERRIDE_TIMEOUT_SECONDS="$(env_default MONITORING_OVERRIDE_TIMEOUT_SECONDS 180)"
AUTHORINO_CERT_TIMEOUT_SECONDS="$(env_default AUTHORINO_CERT_TIMEOUT_SECONDS 300)"
AUTHORINO_NAMESPACE="$(env_default AUTHORINO_NAMESPACE kuadrant-system)"
AUTHORINO_NAME="$(env_default AUTHORINO_NAME authorino)"
AUTHORINO_SERVICE_NAME="$(env_default AUTHORINO_SERVICE_NAME authorino-authorino-authorization)"
AUTHORINO_TLS_SECRET="$(env_default AUTHORINO_TLS_SECRET authorino-server-cert)"
DSCI_METRICS_REPLICAS="$(env_default DSCI_METRICS_REPLICAS 1)"
DSCI_METRICS_STORAGE_SIZE="$(env_default DSCI_METRICS_STORAGE_SIZE 5Gi)"
DSCI_METRICS_RETENTION="$(env_default DSCI_METRICS_RETENTION 24h)"
RHOAI_MONITORING_NAMESPACE="$(env_default RHOAI_MONITORING_NAMESPACE redhat-ods-monitoring)"
PROMETHEUS_STATEFULSET_NAME="$(env_default PROMETHEUS_STATEFULSET_NAME prometheus-data-science-monitoringstack)"
PROMETHEUS_SERVICE_NAME="$(env_default PROMETHEUS_SERVICE_NAME data-science-monitoringstack-prometheus)"
TEMP_HELPER_DIR=''
CURRENT_PHASE=initialization
RUN_STARTED_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
RHOAI_NAMESPACE="$(env_default RHOAI_NAMESPACE redhat-ods-operator)"
RHOAI_CSV="$(env_default RHOAI_CSV rhods-operator.3.5.2)"
GITOPS_OPERATOR_NAMESPACE="$(env_default GITOPS_OPERATOR_NAMESPACE openshift-gitops-operator)"
APPLICATION_NAMESPACE="$(env_default APPLICATION_NAMESPACE openshift-gitops)"
APPLICATION_NAME="$(env_default APPLICATION_NAME ephemeral-rhoai-maas-platform)"
APP_REPO_URL="$(env_default ODH_GITOPS_REPO_URL https://github.com/hexfusion/odh-gitops.git)"
APP_CHART_PATH="$(env_default ODH_GITOPS_CHART_PATH charts/rhai-on-openshift-chart)"

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

while (($#)); do
  case "$1" in
    --mode) MODE="$2"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="$2"; shift 2 ;;
    --gitops-dir) ODH_GITOPS_DIR="$2"; shift 2 ;;
    --gitops-revision) ODH_GITOPS_REVISION="$2"; shift 2 ;;
    --values-file) PLATFORM_VALUES_FILE="$2"; shift 2 ;;
    --maas-source-dir) MAAS_SOURCE_DIR="$2"; shift 2 ;;
    --maas-source-revision) MAAS_SOURCE_REVISION="$2"; shift 2 ;;
    --rhoai-csv) RHOAI_CSV="$2"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="$2"; shift 2 ;;
    --resource-profile) RESOURCE_PROFILE="$2"; shift 2 ;;
    --authorino-namespace) AUTHORINO_NAMESPACE="$2"; shift 2 ;;
    --authorino-name) AUTHORINO_NAME="$2"; shift 2 ;;
    --authorino-service) AUTHORINO_SERVICE_NAME="$2"; shift 2 ;;
    --authorino-tls-secret) AUTHORINO_TLS_SECRET="$2"; shift 2 ;;
    --authorino-cert-timeout) AUTHORINO_CERT_TIMEOUT_SECONDS="$2"; shift 2 ;;
    --metrics-replicas) DSCI_METRICS_REPLICAS="$2"; shift 2 ;;
    --metrics-storage-size) DSCI_METRICS_STORAGE_SIZE="$2"; shift 2 ;;
    --metrics-retention) DSCI_METRICS_RETENTION="$2"; shift 2 ;;
    *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
  esac
done

case "$RESOURCE_PROFILE" in
  strict)
    CHART_PROFILE=maas
    PROMETHEUS_CPU_REQUEST=100m
    THANOS_CPU_REQUEST=50m
    ;;
  reduced-dev)
    [[ "$CLUSTER_ENV" == ephemeral-dev ]] || {
      printf 'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev\n' >&2
      exit 2
    }
    # Upstream GitOps main does not carry the local reduced-dev profile. Keep
    # its supported MaaS profile and apply the narrow E2E-owned CR overlay.
    CHART_PROFILE=maas
    PROMETHEUS_CPU_REQUEST=50m
    THANOS_CPU_REQUEST=10m
    ;;
  *) printf 'RESOURCE_PROFILE must be strict or reduced-dev\n' >&2; exit 2 ;;
esac
if [[ "$(env_default ALLOW_UNDER_BASELINE_DEV_SMOKE false)" == true && "$RESOURCE_PROFILE" != reduced-dev ]]; then
  printf 'ALLOW_UNDER_BASELINE_DEV_SMOKE is no longer a profile selector; use RESOURCE_PROFILE=reduced-dev explicitly\n' >&2
  exit 2
fi

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Cold deployment plan (no cluster calls):
  1. Verify API/context, ClusterVersion, ClusterOperators, storage, and worker capacity.
  2. Install only RHOAI stable-3.5 / rhods-operator.3.5.2 by manual OLM approval.
  3. Install OpenShift GitOps with its pinned CSV.
  4. Render the selected GitOps resource profile before the capacity gate, then apply the pinned Argo Application.
  5. Bootstrap Authorino service-ca TLS through staged Argo desired state.
  6. Set DSCI metrics through pinned values and the MaaS database from the exact installed RHOAI CSV image digest.
  7. Require RHOAI, MaaS, KServe, Authorino, and webhook readiness before Grid deployment.
  8. Continue with task-owned Grid/Enrollment/Gateway/ExtProc/pools, then xtask.

Modes: plan, preflight, render-profile, apply-platform, reconcile-platform-config,
rollback-resource-profile, configure-maas-db, verify-platform, collect.
Insecure API requires CLUSTER_ENV=ephemeral-dev and ALLOW_INSECURE_DEV_API=true.
RESOURCE_PROFILE defaults to strict. The strict worker baseline is a hard gate:
at least two Ready workers
with 8 CPU and 32 GiB allocatable each, and at least 20 allocatable worker CPUs
in total. Select RESOURCE_PROFILE=reduced-dev (or --resource-profile reduced-dev)
only for CLUSTER_ENV=ephemeral-dev. It declares Prometheus 50m, Thanos 10m,
and a 25m downstream Grid operator request, preserves other request fields,
and waives only the full-size-worker threshold. It still requires at least two
Ready workers, healthy ClusterOperators, the default StorageClass, zero
Pending Pods at preflight/convergence, and all platform/inference readiness.
It is labeled DEV-SMOKE / REDUCED-CAPACITY. API TLS bypass remains a separate
opt-in and is required only when the kubeconfig itself bypasses CA verification.
The application Gateway TLS checks remain enabled in the E2E runner.
PLAN
  exit 0
fi

command -v oc >/dev/null || { printf 'oc is required\n' >&2; exit 2; }
command -v jq >/dev/null || { printf 'jq is required\n' >&2; exit 2; }
if [[ -z "$EVIDENCE_DIR" ]]; then
  EVIDENCE_DIR="$REPO_ROOT/evidence/cold-deploy-$(date -u +%Y%m%dT%H%M%SZ)"
fi
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists; choose a new UTC timestamp\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
: >"$EVIDENCE_DIR/RUN-LOG.md"
TEMP_HELPER_DIR="$(mktemp -d "${TMPDIR:-/tmp}/grid-llmd-deploy.XXXXXX")"
chmod 0700 "$TEMP_HELPER_DIR"

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  printf '%s\n' "$line" | tee -a "$EVIDENCE_DIR/RUN-LOG.md" "$RUN_LOG_PATH"
}

cleanup_temp() {
  if [[ -n "$TEMP_HELPER_DIR" && -d "$TEMP_HELPER_DIR" ]]; then
    rm -rf "$TEMP_HELPER_DIR"
  fi
}

finish_run() {
  local exit_code=$? outcome=not-run failure_class=none capacity_baseline
  trap - EXIT
  cleanup_temp
  capacity_baseline=REQUIRED
  [[ "$RESOURCE_PROFILE" != reduced-dev ]] || capacity_baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE
  if [[ -n "${EVIDENCE_DIR:-}" && -d "$EVIDENCE_DIR" ]]; then
    if ((exit_code == 0)); then
      case "$MODE" in
        preflight)
          if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then outcome=preflight-pass-reduced-dev; else outcome=preflight-pass; fi
          ;;
        verify-platform|collect) outcome=platform-observed ;;
        *) outcome=completed ;;
      esac
    else
      outcome=blocked
      case "$CURRENT_PHASE" in
        capacity-preflight|platform-readiness) failure_class=platform/capacity-or-readiness ;;
        operator-install:*) failure_class=platform/operator-installation ;;
        database-prerequisite) failure_class=maas-configuration-or-deployment ;;
        api-access) failure_class=authentication/environment ;;
        *) failure_class=automation-or-deployment ;;
      esac
      {
        printf '# BUG Capture\n\n'
        printf -- '- First failing phase: `%s`\n' "$CURRENT_PHASE"
        printf -- '- Failure time: %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        printf -- '- Classification: %s\n' "$failure_class"
        printf -- '- Mode: `%s`\n' "$MODE"
        printf -- '- Exit code: `%s`\n' "$exit_code"
        printf -- '- Source commit: `%s`\n' "$(git -C "$REPO_ROOT" rev-parse HEAD 2>/dev/null || printf unavailable)"
        printf -- '- Evidence: `capacity-gate.json`, `pending-pods.txt`, `relevant-events.txt`, `nodes.txt`, and `RUN-LOG.md` when present.\n'
        printf '\nNo credentials, Secret data, private keys, or raw helper output are captured.\n'
      } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
    fi
    jq -n --arg mode "$MODE" --arg outcome "$outcome" --arg phase "$CURRENT_PHASE" \
      --arg failureClass "$failure_class" --arg started "$RUN_STARTED_UTC" \
      --arg finished "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg evidence "$EVIDENCE_DIR" \
      --arg capacityBaseline "$capacity_baseline" --arg resourceProfile "$RESOURCE_PROFILE" \
      --argjson exitCode "$exit_code" \
      '{mode:$mode,status:$outcome,phase:$phase,failureClass:$failureClass,capacityBaseline:$capacityBaseline,resourceProfile:$resourceProfile,exitCode:$exitCode,started:$started,finished:$finished,evidenceDirectory:$evidence}' \
      >"$EVIDENCE_DIR/report.json"
    cat >"$EVIDENCE_DIR/FINAL-RESULT.md" <<EOF
# Cold deployment attempt result

- Mode: $MODE
- Status: $outcome
- Last phase: $CURRENT_PHASE
- Exit code: $exit_code
- Started: $RUN_STARTED_UTC
- Finished: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Evidence: $EVIDENCE_DIR
- Resource profile: $RESOURCE_PROFILE
- Result label: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'DEV-SMOKE / REDUCED-CAPACITY'; else printf 'STRICT / FULL-CAPACITY'; fi)
- Capacity baseline: $capacity_baseline
- Full RHOAI capacity qualification: $(if [[ "$capacity_baseline" == REQUIRED ]]; then printf 'required by this attempt'; else printf 'not claimed; under-baseline dev smoke only'; fi)

This platform bootstrap result is not a complete Grid/Enrollment/ExtProc/two-pool signal qualification.
EOF
    if declare -F log >/dev/null; then
      log "Run ended with status=$outcome phase=$CURRENT_PHASE exit=$exit_code"
    fi
  fi
  exit "$exit_code"
}
trap finish_run EXIT

write_resource_profile_record() {
  jq -n --arg profile "$RESOURCE_PROFILE" --arg chartProfile "$CHART_PROFILE" \
    --arg clusterEnv "$CLUSTER_ENV" --arg gitopsRevision "$ODH_GITOPS_REVISION" \
    --arg valuesFile "$PLATFORM_VALUES_FILE" --arg valuesSha "$(if [[ -f "$PLATFORM_VALUES_FILE" ]]; then sha256sum "$PLATFORM_VALUES_FILE" | awk '{print $1}'; else printf unavailable; fi)" \
    --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
    --arg gridOperator "$(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 25m; else printf 50m; fi)" \
    '{resourceProfile:$profile,chartProfile:$chartProfile,clusterEnvironment:$clusterEnv,resultLabel:(if $profile=="reduced-dev" then "DEV-SMOKE / REDUCED-CAPACITY" else "STRICT / FULL-CAPACITY" end),capacityBaseline:(if $profile=="reduced-dev" then "WAIVED_FOR_EPHEMERAL_DEV_SMOKE" else "REQUIRED" end),gitopsRevision:$gitopsRevision,platformValuesFile:$valuesFile,platformValuesSha256:$valuesSha,resourceOverrides:{prometheusCpuRequest:$prometheus,thanosCpuRequest:$thanos,downstreamGridOperatorCpuRequest:$gridOperator},strictRollbackValues:{profile:"maas",prometheusCpuRequest:"100m",thanosCpuRequest:"50m",downstreamGridOperatorCpuRequest:"50m"},preserved:["Prometheus and Thanos memory requests/limits","config-reloader requests","one Prometheus replica","5Gi storage","24h retention","all required platform and serving components"]}' \
    >"$EVIDENCE_DIR/resource-profile.json"
  cat >"$EVIDENCE_DIR/resource-profile.md" <<EOF
# Selected resource profile

- Selected profile: $RESOURCE_PROFILE
- Chart profile: $CHART_PROFILE
- Cluster classification: $CLUSTER_ENV
- Result label: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'DEV-SMOKE / REDUCED-CAPACITY'; else printf 'STRICT / FULL-CAPACITY'; fi)
- Capacity baseline: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'WAIVED_FOR_EPHEMERAL_DEV_SMOKE'; else printf 'REQUIRED'; fi)
- Prometheus CPU request: $PROMETHEUS_CPU_REQUEST (strict rollback: 100m)
- Thanos CPU request: $THANOS_CPU_REQUEST (strict rollback: 50m)
- Downstream Grid operator CPU request: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf '25m'; else printf '50m'; fi) (strict rollback: 50m)
- Memory requests/limits, config-reloader resources, replicas, 5Gi storage, and 24h retention are preserved.
- API TLS bypass is independent and is recorded in INVOCATION.md only if the current kubeconfig requires it.

The MonitoringStack values are applied declaratively by the selected pinned odh-gitops chart profile. The ExtProc Grid-operator helper selects its
resource request from this same RESOURCE_PROFILE. No ad hoc MonitoringStack or Deployment patch is used by the profile.
EOF
  log "Resource profile recorded: $RESOURCE_PROFILE; Prometheus=$PROMETHEUS_CPU_REQUEST Thanos=$THANOS_CPU_REQUEST GridOperator=$(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 25m; else printf 50m; fi)"
}

verify_access() {
  local context user server insecure ca_path ready api_mode
  CURRENT_PHASE=api-access
  if [[ -z "$KUBE_CONTEXT" ]]; then
    KUBE_CONTEXT="$(command oc config current-context 2>/dev/null || true)"
  fi
  context="$KUBE_CONTEXT"
  [[ -n "$context" ]] || { printf 'oc context missing; request authorized reauthentication\n' >&2; return 1; }
  user="$(oc whoami 2>/dev/null)" || { printf 'oc authentication failed; request authorized reauthentication\n' >&2; return 1; }
  server="$(oc whoami --show-server 2>/dev/null)" || return 1
  if [[ -n "$EXPECTED_API_SERVER" && "$server" != "$EXPECTED_API_SERVER" ]]; then
    printf 'current API server does not match EXPECTED_API_SERVER\n' >&2
    return 1
  fi
  insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
  ca_path="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.certificate-authority}' 2>/dev/null || true)"
  if [[ "$insecure" == true ]]; then
    [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
      printf 'insecure kubeconfig rejected without explicit ephemeral-dev opt-in\n' >&2
      return 1
    }
    api_mode=explicit-insecure-dev
  else
    [[ -z "$ca_path" || -f "$ca_path" ]] || { printf 'configured kubeconfig CA file missing\n' >&2; return 1; }
    api_mode=kubeconfig-or-system-CA-verification
  fi
  [[ "$MIN_READY_WORKERS" == 2 && "$MIN_FULL_SIZE_WORKERS" == 2 && "$MIN_WORKER_CPU" == 8 && "$MIN_WORKER_MEMORY_GIB" == 32 && "$MIN_TOTAL_WORKER_CPU" == 20 ]] || {
    printf 'capacity thresholds are fixed at 2 Ready workers, 2 workers with 8 CPU/32 GiB each, and 20 aggregate allocatable worker CPUs; overrides are not supported\n' >&2
    return 1
  }
  ready="$(oc get --raw=/readyz 2>/dev/null)" || { printf 'OpenShift API /readyz failed\n' >&2; return 1; }
  [[ "$ready" == *ok* ]] || { printf 'OpenShift API /readyz did not return ok\n' >&2; return 1; }
  cat >"$EVIDENCE_DIR/INVOCATION.md" <<EOF
# Cold deployment invocation

- Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- Selected context: $context
- Global kubeconfig current context (informational only): $(command oc config current-context 2>/dev/null || printf unavailable)
- Context pin: KUBE_CONTEXT=$KUBE_CONTEXT
- API server: $server
- User: $user
- API TLS mode: $api_mode
- Cluster environment: $CLUSTER_ENV
- Resource profile: $RESOURCE_PROFILE
- Result label: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'DEV-SMOKE / REDUCED-CAPACITY'; else printf 'STRICT / FULL-CAPACITY'; fi)
- Explicit insecure development opt-in: $ALLOW_INSECURE_DEV_API
- Capacity baseline disposition: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'WAIVED_FOR_EPHEMERAL_DEV_SMOKE'; else printf 'REQUIRED'; fi)
- Full baseline requirement: at least $MIN_READY_WORKERS Ready workers, at least $MIN_FULL_SIZE_WORKERS with $MIN_WORKER_CPU CPU and ${MIN_WORKER_MEMORY_GIB}GiB allocatable each, and $MIN_TOTAL_WORKER_CPU aggregate allocatable worker CPU cores; three full-size workers are preferred
- RHOAI CSV resource name: $RHOAI_CSV
- OpenShift GitOps CSV: openshift-gitops-operator.v1.20.8
- odh-gitops revision: $ODH_GITOPS_REVISION
- ExtProc source commit: $(git -C "$REPO_ROOT" rev-parse HEAD)
- ExtProc worktree: $(if [[ -n "$(git -C "$REPO_ROOT" status --porcelain)" ]]; then printf 'dirty; preserved'; else printf clean; fi)
- Platform values SHA-256: $(if [[ -f "$PLATFORM_VALUES_FILE" ]]; then sha256sum "$PLATFORM_VALUES_FILE" | awk '{print $1}'; else printf missing; fi)

Credential values, tokens, kubeconfig contents, Secret data, and private keys are excluded.
EOF
  printf 'selected_context=%s\nglobal_current_context=%s\nuser=%s\nserver=%s\nreadyz=%s\napi_tls_mode=%s\ncluster_env=%s\nresource_profile=%s\n' \
    "$context" "$(command oc config current-context 2>/dev/null || printf unavailable)" "$user" "$server" "$ready" "$api_mode" "$CLUSTER_ENV" "$RESOURCE_PROFILE" \
    >"$EVIDENCE_DIR/api-access-summary.txt"
  {
    printf '# Source lock for this cold platform attempt\n\n'
    printf -- '- Praxis ExtProc: `%s`\n' "$(git -C "$REPO_ROOT" rev-parse HEAD)"
    printf -- '- Installed RHOAI CSV: `%s`\n' "$RHOAI_CSV"
    printf -- '- odh-gitops requested revision: `%s`\n' "${ODH_GITOPS_REVISION:-not-supplied}"
    printf -- '- MaaS requested revision: `%s`\n' "${MAAS_SOURCE_REVISION:-not-supplied}"
    printf -- '- Selected resource profile: `%s` (chart profile `%s`)\n' "$RESOURCE_PROFILE" "$CHART_PROFILE"
    printf -- '- Profile request targets: Prometheus `%s`, Thanos `%s`, downstream Grid operator `%s`\n' "$PROMETHEUS_CPU_REQUEST" "$THANOS_CPU_REQUEST" "$(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 25m; else printf 50m; fi)"
    printf -- '- Cold deployment script SHA-256: `%s`\n' "$(sha256sum "$SCRIPT_DIR/cold-deploy.sh" | awk '{print $1}')"
    printf -- '- Platform values SHA-256: `%s`\n' "$(sha256sum "$PLATFORM_VALUES_FILE" | awk '{print $1}')"
    printf '\nThis source lock covers the current platform phase. The final qualification lock must include Grid, downstream operator, ExtProc runtime, EPP, and simulator revisions and image digests.\n'
  } >"$EVIDENCE_DIR/source-lock.md"
  log "API access verified for $context; API TLS mode recorded"
}

capture_inventory() {
  CURRENT_PHASE=read-only-snapshot
  oc get clusterversion version -o yaml >"$EVIDENCE_DIR/clusterversion.yaml"
  oc get infrastructure.config.openshift.io cluster -o json | jq '{infrastructureName:.status.infrastructureName,platform:.status.platform,awsRegion:.status.platformStatus.aws.region}' >"$EVIDENCE_DIR/infrastructure-summary.json"
  oc get clusteroperators -o wide >"$EVIDENCE_DIR/clusteroperators.txt"
  oc get clusteroperators -o json | jq '[.items[]|{name:.metadata.name,conditions:[.status.conditions[]?|select(.type=="Available" or .type=="Progressing" or .type=="Degraded")|{type,status,reason}]}]' >"$EVIDENCE_DIR/clusteroperator-conditions.json"
  oc get nodes -o wide >"$EVIDENCE_DIR/nodes.txt"
  oc get nodes -o json | jq '[.items[]|{name:.metadata.name,roles:(.metadata.labels|with_entries(select(.key|startswith("node-role.kubernetes.io/")))),instanceType:.metadata.labels["node.kubernetes.io/instance-type"],ready:([.status.conditions[]?|select(.type=="Ready")|.status][0]),taints:.spec.taints,capacity:.status.capacity,allocatable:.status.allocatable}]' >"$EVIDENCE_DIR/node-capacity.json"
  oc describe nodes -l node-role.kubernetes.io/worker | awk '/^Name:|^Allocated resources:|^[[:space:]]+cpu[[:space:]]|^[[:space:]]+memory[[:space:]]/' >"$EVIDENCE_DIR/worker-allocated-resources.txt"
  oc get machinesets.machine.openshift.io -n openshift-machine-api -o custom-columns='NAME:.metadata.name,INSTANCE:.spec.template.spec.providerSpec.value.instanceType,REGION:.spec.template.spec.providerSpec.value.placement.region,ZONE:.spec.template.spec.providerSpec.value.placement.availabilityZone,DESIRED:.spec.replicas,READY:.status.readyReplicas' >"$EVIDENCE_DIR/machinesets.txt" 2>&1 || true
  oc get storageclasses >"$EVIDENCE_DIR/storageclasses.txt"
  oc get catalogsources.operators.coreos.com -n openshift-marketplace -o json \
    | jq '[.items[]|{name:.metadata.name,namespace:.metadata.namespace,lastObservedState:.status.connectionState.lastObservedState,conditions:[.status.conditions[]?|{type,status,reason}]}]' \
    >"$EVIDENCE_DIR/catalog-sources.json"
  oc get pods -A --field-selector=status.phase=Pending -o wide >"$EVIDENCE_DIR/pending-pods.txt"
  oc get events -A --sort-by=.lastTimestamp >"$EVIDENCE_DIR/events.txt"
  oc get subscriptions.operators.coreos.com -A -o wide >"$EVIDENCE_DIR/subscriptions.txt" 2>&1 || true
  oc get csv -A -o wide >"$EVIDENCE_DIR/csvs.txt" 2>&1 || true
  oc get datasciencecluster,dsci,maastenantconfig,aitenant,gridnetwork,gridsite,siteinvite,gateway,httproute,inferencepool,llminferenceservice -A -o wide >"$EVIDENCE_DIR/qualification-inventory.txt" 2>&1 || true
  oc get pods -A -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,phase:.status.phase,node:.spec.nodeName,ownerReferences:[.metadata.ownerReferences[]?|{kind,name}],containers:[.spec.containers[]?|{name,requests:(.resources.requests//{}),limits:(.resources.limits//{})}],initContainers:[.spec.initContainers[]?|{name,requests:(.resources.requests//{}),limits:(.resources.limits//{})}]}]' \
    >"$EVIDENCE_DIR/live-pod-resource-requests.json"
  if oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack -o json \
    >"$EVIDENCE_DIR/monitoringstack-before-profile-raw.json" 2>/dev/null; then
    jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,ownerReferences:[.metadata.ownerReferences[]?|{apiVersion,kind,name,uid}],spec:.spec}' \
      "$EVIDENCE_DIR/monitoringstack-before-profile-raw.json" >"$EVIDENCE_DIR/monitoringstack-profile-baseline.json"
    jq -S '.spec' "$EVIDENCE_DIR/monitoringstack-before-profile-raw.json" \
      >"$EVIDENCE_DIR/monitoringstack-spec-before-profile.json"
    capture_prometheus_statefulset_resources "$EVIDENCE_DIR/prometheus-statefulset-resources-before-profile.json" \
      || printf 'not present before profile reconciliation\n' >"$EVIDENCE_DIR/prometheus-statefulset-resources-before-profile.status"
  else
    printf 'MonitoringStack CRD/resource not present at pre-mutation inventory\n' \
      >"$EVIDENCE_DIR/monitoringstack-profile-baseline.status"
  fi
  log 'Read-only inventory captured; Secret data was not requested'
}

capacity_gate() {
  CURRENT_PHASE=capacity-preflight
  local cv workers ready qualified total_worker_cpu pending degraded unhealthy storage baseline disposition catalog_total catalog_ready control_plane_taints
  cv="$(oc get clusterversion version -o json | jq -r '[.status.conditions[]? | select((.type=="Available" and .status=="True") or (.type=="Progressing" and .status=="False") or (.type=="Failing" and .status=="False"))] | length==3')"
  workers="$(oc get nodes -o json | jq '
    def cpuMilli: if endswith("m") then rtrimstr("m")|tonumber else tonumber*1000 end;
    def memBytes: capture("(?<n>[0-9]+)(?<u>Ki|Mi|Gi|Ti|K|M|G|T)") as $q | ($q.n|tonumber) * (if $q.u=="Ki" then 1024 elif $q.u=="Mi" then 1048576 elif $q.u=="Gi" then 1073741824 elif $q.u=="Ti" then 1099511627776 elif $q.u=="K" then 1000 elif $q.u=="M" then 1000000 elif $q.u=="G" then 1000000000 else 1000000000000 end);
    [.items[]|select(.metadata.labels["node-role.kubernetes.io/worker"]!=null)|{name:.metadata.name,ready:([.status.conditions[]|select(.type=="Ready")|.status][0]=="True"),cpu:(.status.allocatable.cpu|cpuMilli),memory:(.status.allocatable.memory|memBytes)}]
  ')"
  ready="$(jq '[.[]|select(.ready)]|length' <<<"$workers")"
  qualified="$(jq --argjson c "$((MIN_WORKER_CPU * 1000))" --argjson m "$((MIN_WORKER_MEMORY_GIB * 1073741824))" '[.[]|select(.ready and .cpu >= $c and .memory >= $m)]|length' <<<"$workers")"
  total_worker_cpu="$(jq '[.[]|select(.ready)|.cpu]|add // 0' <<<"$workers")"
  pending="$(oc get pods -A --field-selector=status.phase=Pending -o json | jq '.items|length')"
  degraded="$(oc get clusteroperators -o json | jq '[.items[]|select(any(.status.conditions[]?;.type=="Degraded" and .status=="True"))]|length')"
  unhealthy="$(oc get clusteroperators -o json | jq '[.items[]|select((any(.status.conditions[]?;.type=="Available" and .status=="True")|not) or any(.status.conditions[]?;.type=="Degraded" and .status=="True") or any(.status.conditions[]?;.type=="Progressing" and .status=="True"))]|length')"
  storage="$(oc get storageclass -o json | jq '[.items[]|select(.metadata.annotations["storageclass.kubernetes.io/is-default-class"]=="true" or .metadata.annotations["storageclass.beta.kubernetes.io/is-default-class"]=="true")]|length')"
  control_plane_taints="$(oc get nodes -o json | jq '[.items[]|select(.metadata.labels["node-role.kubernetes.io/control-plane"]!=null or .metadata.labels["node-role.kubernetes.io/master"]!=null)|(.spec.taints // [])|any(.[]?;.effect=="NoSchedule")]|all')"
  catalog="$(oc get catalogsources.operators.coreos.com -n openshift-marketplace -o json)"
  catalog_total="$(jq '.items|length' <<<"$catalog")"
  catalog_ready="$(jq '[.items[]|select(.status.connectionState.lastObservedState=="READY" or any(.status.conditions[]?;.type=="Ready" and .status=="True"))]|length' <<<"$catalog")"
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE
    disposition=waived
  else
    baseline=REQUIRED
    disposition=required
  fi
  jq -n --argjson workers "$workers" --argjson ready "$ready" --argjson qualified "$qualified" --argjson pending "$pending" \
    --argjson degraded "$degraded" --argjson unhealthy "$unhealthy" --argjson storage "$storage" --argjson cv "$cv" \
    --argjson catalogTotal "$catalog_total" --argjson catalogReady "$catalog_ready" \
    --arg baseline "$baseline" --arg disposition "$disposition" \
    --arg resourceProfile "$RESOURCE_PROFILE" --arg prometheusCpuRequest "$PROMETHEUS_CPU_REQUEST" --arg thanosCpuRequest "$THANOS_CPU_REQUEST" \
    --argjson minReady "$MIN_READY_WORKERS" --argjson minFull "$MIN_FULL_SIZE_WORKERS" \
    --argjson minCpu "$MIN_WORKER_CPU" --argjson minMem "$MIN_WORKER_MEMORY_GIB" \
    --argjson minTotalCpu "$MIN_TOTAL_WORKER_CPU" --argjson totalWorkerCpu "$total_worker_cpu" \
    --argjson controlPlaneTaints "$control_plane_taints" \
    '{resourceProfile:$resourceProfile,monitoringCpuRequests:{prometheus:$prometheusCpuRequest,thanos:$thanosCpuRequest},baselineDisposition:$baseline,underBaselineMode:($disposition=="waived"),requirements:{readyWorkers:$minReady,workersMeetingPerNodeBaseline:$minFull,cpuCoresPerWorker:$minCpu,memoryGiBPerWorker:$minMem,totalAllocatableWorkerCpuCores:$minTotalCpu},observed:{workers:$workers,readyWorkers:$ready,workersMeetingPerNodeBaseline:$qualified,totalAllocatableWorkerCpuMilli:$totalWorkerCpu,pendingPods:$pending,degradedClusterOperators:$degraded,unhealthyClusterOperators:$unhealthy,readyCatalogSources:$catalogReady,totalCatalogSources:$catalogTotal,defaultStorageClasses:$storage,clusterVersionReady:$cv,controlPlaneNoScheduleTaints:$controlPlaneTaints}}' >"$EVIDENCE_DIR/capacity-gate.json"
  if [[ "$cv" != true || "$ready" -lt "$MIN_READY_WORKERS" || "$pending" -gt 0 || "$degraded" -gt 0 || "$unhealthy" -gt 0 || "$storage" -lt 1 || "$catalog_total" -lt 1 || "$catalog_ready" -ne "$catalog_total" || "$control_plane_taints" != true ]] || \
     [[ "$RESOURCE_PROFILE" != reduced-dev && ( "$qualified" -lt "$MIN_FULL_SIZE_WORKERS" || "$total_worker_cpu" -lt "$((MIN_TOTAL_WORKER_CPU * 1000))" ) ]]; then
    log "Capacity preflight BLOCKED: capacity-baseline=$baseline, readyWorkers=$ready/$MIN_READY_WORKERS, fullSizeWorkers=$qualified/$MIN_FULL_SIZE_WORKERS, totalAllocatableWorkerCpu=${total_worker_cpu}m/$((MIN_TOTAL_WORKER_CPU * 1000))m, pendingPods=$pending, degradedClusterOperators=$degraded, unhealthyClusterOperators=$unhealthy, readyCatalogSources=$catalog_ready/$catalog_total, defaultStorageClasses=$storage, controlPlaneNoScheduleTaints=$control_plane_taints, ClusterVersionReady=$cv"
    return 1
  fi
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    log "DEV-SMOKE / REDUCED-CAPACITY; capacity-baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE; full-size workers=$qualified/$MIN_FULL_SIZE_WORKERS, total allocatable worker CPU=${total_worker_cpu}m/$((MIN_TOTAL_WORKER_CPU * 1000))m; readiness gates pass with readyWorkers=$ready, pendingPods=$pending, degradedClusterOperators=$degraded, unhealthyClusterOperators=$unhealthy, readyCatalogSources=$catalog_ready/$catalog_total, defaultStorageClasses=$storage, controlPlaneNoScheduleTaints=$control_plane_taints, ClusterVersionReady=$cv"
  else
    log "Capacity preflight PASS: $qualified workers meet the per-node baseline and total allocatable worker CPU is ${total_worker_cpu}m"
  fi
}

install_operator() {
  local namespace="$1" group="$2" subscription="$3" channel="$4" csv="$5" package="$6" catalog names count plan approved deployments
  CURRENT_PHASE="operator-install:$namespace/$csv"
  plan=''
  catalog="$(oc -n openshift-marketplace get packagemanifest "$package" -o json)" || return 1
  jq -e --arg channel "$channel" --arg csv "$csv" 'any(.status.channels[]?;.name==$channel and (.currentCSV==$csv or any(.entries[]?;.name==$csv)))' <<<"$catalog" >/dev/null || {
    printf 'catalog lacks exact channel/CSV %s/%s\n' "$channel" "$csv" >&2
    return 1
  }
  if ! oc get namespace "$namespace" >/dev/null 2>&1; then
    oc create namespace "$namespace" >/dev/null
  fi
  if oc -n "$namespace" get operatorgroup "$group" -o json >/dev/null 2>&1; then
    oc -n "$namespace" get operatorgroup "$group" -o json | jq -e '(.spec.targetNamespaces // [] | length)==0 and (.spec.targetNamespace // "")==""' >/dev/null || {
      printf 'existing OperatorGroup %s/%s has a target scope different from the cold-deploy target\n' "$namespace" "$group" >&2
      return 1
    }
  else
    oc apply -f - <<YAML
apiVersion: operators.coreos.com/v1
kind: OperatorGroup
metadata:
  name: $group
  namespace: $namespace
YAML
  fi
  if oc -n "$namespace" get subscription "$subscription" -o json >/dev/null 2>&1; then
    oc -n "$namespace" get subscription "$subscription" -o json | jq -e --arg package "$package" --arg channel "$channel" --arg csv "$csv" '
      .spec.name==$package and .spec.channel==$channel and .spec.startingCSV==$csv and
      .spec.source=="redhat-operators" and .spec.sourceNamespace=="openshift-marketplace" and
      .spec.installPlanApproval=="Manual"
    ' >/dev/null || {
      printf 'existing Subscription %s/%s differs from the pinned target\n' "$namespace" "$subscription" >&2
      return 1
    }
  else
    oc apply -f - <<YAML
apiVersion: operators.coreos.com/v1alpha1
kind: Subscription
metadata:
  name: $subscription
  namespace: $namespace
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
spec:
  name: $package
  channel: $channel
  source: redhat-operators
  sourceNamespace: openshift-marketplace
  startingCSV: $csv
  installPlanApproval: Manual
YAML
  fi
  [[ "$(oc -n "$namespace" get subscription "$subscription" -o jsonpath='{.spec.startingCSV}')" == "$csv" ]] || return 1
  if [[ "$(oc -n "$namespace" get csv "$csv" --ignore-not-found -o jsonpath='{.status.phase}')" != Succeeded ]]; then
    for _ in $(seq 1 $((INSTALL_TIMEOUT_SECONDS / 10))); do
      names="$(oc -n "$namespace" get installplans -o json | jq -r --arg csv "$csv" '[.items[]|select((.spec.clusterServiceVersionNames|length)==1 and .spec.clusterServiceVersionNames[0]==$csv)|.metadata.name]|.[]')"
      count="$(printf '%s\n' "$names" | sed '/^$/d' | wc -l | tr -d ' ')"
      if [[ "$count" == 1 ]]; then plan="$names"; break; fi
      [[ "$count" -le 1 ]] || { printf 'multiple InstallPlans match only %s\n' "$csv" >&2; return 1; }
      sleep 10
    done
    [[ -n "$plan" ]] || { printf 'timed out waiting for exact InstallPlan %s\n' "$csv" >&2; return 1; }
    approved="$(oc -n "$namespace" get installplan "$plan" -o jsonpath='{.spec.approved}')"
    if [[ "$approved" != true ]]; then
      log "Approving $namespace/$plan only after confirming its sole CSV is $csv"
      oc -n "$namespace" patch installplan "$plan" --type=merge -p '{"spec":{"approved":true}}' >/dev/null
    fi
  fi
  local csv_deadline=$((SECONDS + INSTALL_TIMEOUT_SECONDS))
  while ! oc -n "$namespace" get csv "$csv" --ignore-not-found -o name 2>/dev/null | grep -qx "clusterserviceversion.operators.coreos.com/$csv"; do
    ((SECONDS < csv_deadline)) || { printf 'timed out waiting for OLM to create exact CSV %s\n' "$csv" >&2; return 1; }
    sleep 5
  done
  oc -n "$namespace" wait --for=jsonpath='{.status.phase}'=Succeeded "csv/$csv" --timeout="$INSTALL_TIMEOUT_SECONDS"s
  deployments="$(oc -n "$namespace" get csv "$csv" -o json | jq -r '.spec.install.spec.deployments[].name')"
  while IFS= read -r deployment; do
    [[ -n "$deployment" ]] || continue
    oc -n "$namespace" rollout status "deployment/$deployment" --timeout="$INSTALL_TIMEOUT_SECONDS"s
  done <<<"$deployments"
  log "CSV $csv and its declared Deployments are ready"
}

validate_gitops_source() {
  [[ -e "$ODH_GITOPS_DIR/.git" ]] || { printf 'ODH_GITOPS_DIR must identify the existing GitOps checkout or clean worktree\n' >&2; return 1; }
  [[ "$ODH_GITOPS_REVISION" =~ ^[0-9a-f]{40}$ ]] || { printf 'ODH_GITOPS_REVISION must be a full immutable SHA\n' >&2; return 1; }
  [[ "$(git -C "$ODH_GITOPS_DIR" rev-parse HEAD)" == "$ODH_GITOPS_REVISION" ]] || { printf 'GitOps checkout HEAD differs from the pinned Argo revision\n' >&2; return 1; }
  git -C "$ODH_GITOPS_DIR" diff --quiet "$ODH_GITOPS_REVISION" -- "$APP_CHART_PATH" || {
    printf 'GitOps chart has local changes not present in the pinned Argo revision; refusing installation before profile artifact is published\n' >&2
    return 1
  }
  [[ -z "$(git -C "$ODH_GITOPS_DIR" ls-files --others --exclude-standard -- "$APP_CHART_PATH")" ]] || {
    printf 'GitOps chart has untracked files not present in the pinned Argo revision; refusing installation\n' >&2
    return 1
  }
}

render_platform_resource_profile() {
  local chart_dir="$ODH_GITOPS_DIR/$APP_CHART_PATH" strict_overlay reduced_overlay
  [[ -f "$PLATFORM_VALUES_FILE" ]] || { printf 'platform values file is missing\n' >&2; return 1; }
  command -v helm >/dev/null || { printf 'helm is required\n' >&2; return 1; }
  helm template "$APPLICATION_NAME" "$chart_dir" --namespace "$APPLICATION_NAMESPACE" \
    --values "$PLATFORM_VALUES_FILE" --set skipCrdCheck=true --set profile=maas --include-crds \
    >"$EVIDENCE_DIR/strict-resource-profile-render.yaml"
  cp "$EVIDENCE_DIR/strict-resource-profile-render.yaml" "$EVIDENCE_DIR/reduced-dev-resource-profile-render.yaml"
  strict_overlay="$EVIDENCE_DIR/monitoringstack-strict-render.yaml"
  reduced_overlay="$EVIDENCE_DIR/monitoringstack-reduced-dev-render.yaml"
  bash "$SCRIPT_DIR/render-monitoringstack-profile.sh" --profile strict --output "$strict_overlay"
  bash "$SCRIPT_DIR/render-monitoringstack-profile.sh" --profile reduced-dev --output "$reduced_overlay"
  if ! grep -Fq 'cpu: "100m"' "$strict_overlay" ||
    ! grep -Fq 'cpu: "50m"' "$strict_overlay" ||
    ! grep -Fq 'cpu: "50m"' "$reduced_overlay" ||
    ! grep -Fq 'cpu: "10m"' "$reduced_overlay"; then
    printf 'task-owned strict/reduced MonitoringStack overlays do not contain 100m/50m and 50m/10m CPU values\n' >&2
    return 1
  fi
  diff -u "$strict_overlay" "$reduced_overlay" \
    >"$EVIDENCE_DIR/resource-profile-render.diff" || [[ "$?" == 1 ]]
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    cp "$EVIDENCE_DIR/reduced-dev-resource-profile-render.yaml" "$EVIDENCE_DIR/platform-render.yaml"
  else
    cp "$EVIDENCE_DIR/strict-resource-profile-render.yaml" "$EVIDENCE_DIR/platform-render.yaml"
  fi
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    cat "$reduced_overlay" >>"$EVIDENCE_DIR/platform-render.yaml"
  fi
  jq -n --arg selected "$RESOURCE_PROFILE" --arg chartProfile "$CHART_PROFILE" \
    --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
    --arg gridOperator "$(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 25m; else printf 50m; fi)" \
    --arg rollbackPrometheus 100m --arg rollbackThanos 50m \
    '{resourceProfile:$selected,chartProfile:$chartProfile,resultLabel:(if $selected=="reduced-dev" then "DEV-SMOKE / REDUCED-CAPACITY" else "STRICT / FULL-CAPACITY" end),overrides:{prometheusCpuRequest:$prometheus,thanosCpuRequest:$thanos,gridOperatorCpuRequest:$gridOperator},rollback:{profile:"strict",prometheusCpuRequest:$rollbackPrometheus,thanosCpuRequest:$rollbackThanos},preserved:["memory requests and limits","config-reloader requests","one Prometheus replica","5Gi storage","24h retention"]}' \
    >"$EVIDENCE_DIR/resource-profile.json"
  sha256sum "$PLATFORM_VALUES_FILE" "$SCRIPT_DIR/render-monitoringstack-profile.sh" \
    "$EVIDENCE_DIR/strict-resource-profile-render.yaml" \
    "$EVIDENCE_DIR/reduced-dev-resource-profile-render.yaml" "$EVIDENCE_DIR/monitoringstack-strict-render.yaml" \
    "$EVIDENCE_DIR/monitoringstack-reduced-dev-render.yaml" "$EVIDENCE_DIR/resource-profile-render.diff" \
    >"$EVIDENCE_DIR/platform-render-sha256.txt"
  log "Rendered resource profiles before capacity evaluation: selected=$RESOURCE_PROFILE chartProfile=$CHART_PROFILE; Prometheus=$PROMETHEUS_CPU_REQUEST Thanos=$THANOS_CPU_REQUEST; Grid operator=$(jq -r '.overrides.gridOperatorCpuRequest' "$EVIDENCE_DIR/resource-profile.json")"
}

validate_monitoringstack_server_dry_run() {
  local manifest="$EVIDENCE_DIR/monitoringstack-profile-server-dry-run.yaml" patch_file="$EVIDENCE_DIR/monitoringstack-profile-merge-patch.json"
  cp "$EVIDENCE_DIR/monitoringstack-$RESOURCE_PROFILE-render.yaml" "$manifest"
  if [[ ! -s "$manifest" ]]; then
    printf 'NO_MONITORINGSTACK_PROFILE_RENDERED\n' >"$EVIDENCE_DIR/monitoringstack-server-dry-run.status"
    return 0
  fi
  if ! oc get crd monitoringstacks.monitoring.rhobs >/dev/null 2>&1; then
    printf 'SKIPPED_CRD_NOT_ESTABLISHED\n' >"$EVIDENCE_DIR/monitoringstack-server-dry-run.status"
    log 'MonitoringStack server-side dry-run skipped because its CRD is not established yet'
    return 0
  fi
  if ! oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack >/dev/null 2>&1; then
    printf 'SKIPPED_RESOURCE_NOT_CREATED\n' >"$EVIDENCE_DIR/monitoringstack-server-dry-run.status"
    log 'MonitoringStack profile dry-run deferred because the operator-owned resource is not created yet'
    return 0
  fi
  write_monitoringstack_profile_patch "$patch_file"
  oc -n "$RHOAI_MONITORING_NAMESPACE" patch monitoringstack data-science-monitoringstack \
    --type=merge --dry-run=server --patch-file "$patch_file" \
    >"$EVIDENCE_DIR/monitoringstack-server-dry-run.txt" 2>&1
  printf 'PASS\n' >"$EVIDENCE_DIR/monitoringstack-server-dry-run.status"
  log 'MonitoringStack CPU merge patch passed OpenShift server-side dry-run; no resource was changed'
}

write_monitoringstack_profile_patch() {
  local destination="$1"
  jq -n --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
    '{spec:{resources:{requests:{cpu:$prometheus}},prometheusConfig:{thanosResources:{requests:{cpu:$thanos}}}}}' \
    >"$destination"
  chmod 0600 "$destination"
}

apply_monitoringstack_resource_profile() {
  local profile="$1" manifest="$EVIDENCE_DIR/monitoringstack-$1-render.yaml" patch_file="$EVIDENCE_DIR/monitoringstack-profile-merge-patch.json" deadline current owner crd_established
  [[ "$profile" == strict || "$profile" == reduced-dev ]] || return 2
  [[ -s "$manifest" ]] || { printf 'rendered MonitoringStack overlay is missing for %s\n' "$profile" >&2; return 1; }
  deadline=$((SECONDS + PLATFORM_TIMEOUT_SECONDS))
  while :; do
    crd_established="$(oc get crd monitoringstacks.monitoring.rhobs -o json 2>/dev/null \
      | jq -r '[.status.conditions[]?|select(.type=="Established" and .status=="True")]|length' || true)"
    if [[ "$crd_established" == 1 ]]; then
      break
    fi
    if ((SECONDS >= deadline)); then
      oc get crd monitoringstacks.monitoring.rhobs -o yaml \
        >"$EVIDENCE_DIR/monitoringstack-crd-before-profile-timeout.yaml" 2>&1 || true
      printf 'timed out waiting for the MonitoringStack CRD to become Established\n' >&2
      return 1
    fi
    sleep 5
  done
  log 'MonitoringStack CRD is Established; waiting for the operator-owned resource before applying the profile overlay'
  while ! oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack -o json \
      >"$EVIDENCE_DIR/monitoringstack-before-profile-apply.json" 2>/dev/null; do
    if ((SECONDS >= deadline)); then
      oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack -o yaml \
        >"$EVIDENCE_DIR/monitoringstack-before-profile-timeout.yaml" 2>&1 || true
      printf 'timed out waiting for the operator-owned MonitoringStack\n' >&2
      return 1
    fi
    sleep 5
  done
  owner="$(jq -r '[.metadata.ownerReferences[]?|select(.kind=="Monitoring" and .name=="default-monitoring" and (.apiVersion|startswith("services.platform.opendatahub.io/"))) ]|length' "$EVIDENCE_DIR/monitoringstack-before-profile-apply.json")"
  [[ "$owner" == 1 ]] || { printf 'MonitoringStack owner is not Monitoring/default-monitoring; refusing profile apply\n' >&2; return 1; }
  write_monitoringstack_profile_patch "$patch_file"
  oc -n "$RHOAI_MONITORING_NAMESPACE" patch monitoringstack data-science-monitoringstack \
    --type=merge --dry-run=server --patch-file "$patch_file" \
    >"$EVIDENCE_DIR/monitoringstack-profile-server-dry-run.txt" 2>&1
  oc -n "$RHOAI_MONITORING_NAMESPACE" patch monitoringstack data-science-monitoringstack \
    --type=merge --patch-file "$patch_file" \
    >"$EVIDENCE_DIR/monitoringstack-profile-apply.txt"
  oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack -o json \
    | jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,managedFields:[.metadata.managedFields[]?|{manager,operation,fieldsType}],ownerReferences:[.metadata.ownerReferences[]?|{kind,name,uid}],prometheusCpuRequest:.spec.resources.requests.cpu,thanosCpuRequest:.spec.prometheusConfig.thanosResources.requests.cpu}' \
    >"$EVIDENCE_DIR/monitoringstack-profile-apply-observed.json"
  jq -e --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
    '.prometheusCpuRequest==$prometheus and .thanosCpuRequest==$thanos' \
    "$EVIDENCE_DIR/monitoringstack-profile-apply-observed.json" >/dev/null || {
      printf 'MonitoringStack did not retain both task-owned CPU request values after apply\n' >&2
      return 1
    }
  log "Applied the task-owned MonitoringStack CPU request patch for $profile using the API-supported resource fields"
}

apply_application() {
  CURRENT_PHASE=gitops-application
  validate_gitops_source
  [[ -f "$PLATFORM_VALUES_FILE" ]] || { printf 'platform values file is missing\n' >&2; return 1; }
  command -v helm >/dev/null || { printf 'helm is required\n' >&2; return 1; }
  if oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o json >/dev/null 2>&1; then
    oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o json \
      | jq -e '.metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"' >/dev/null || {
        printf 'existing Argo Application is not task-owned; refusing to adopt it\n' >&2
        return 1
      }
  fi
  render_platform_resource_profile
  validate_monitoringstack_server_dry_run
  helm template "$APPLICATION_NAME" "$ODH_GITOPS_DIR/$APP_CHART_PATH" --namespace "$APPLICATION_NAMESPACE" --values "$PLATFORM_VALUES_FILE" --set "profile=$CHART_PROFILE" --set dependencies.rhcl.config.authorinoSpec.listener.tls.enabled=false --include-crds >"$EVIDENCE_DIR/platform-render-authorino-bootstrap.yaml"
  sha256sum "$PLATFORM_VALUES_FILE" "$EVIDENCE_DIR/platform-render.yaml" \
    "$EVIDENCE_DIR/strict-resource-profile-render.yaml" "$EVIDENCE_DIR/reduced-dev-resource-profile-render.yaml" \
    "$EVIDENCE_DIR/monitoringstack-strict-render.yaml" "$EVIDENCE_DIR/monitoringstack-reduced-dev-render.yaml" \
    "$EVIDENCE_DIR/resource-profile-render.diff" >"$EVIDENCE_DIR/platform-render-sha256.txt"
  sha256sum "$EVIDENCE_DIR/platform-render-authorino-bootstrap.yaml" >"$EVIDENCE_DIR/platform-render-authorino-bootstrap-sha256.txt"
  oc apply -f "$SCRIPT_DIR/manifests/argocd-platform-sync-rbac.yaml" >"$EVIDENCE_DIR/platform-rbac-apply.txt"
  local values
  values="$(cat "$PLATFORM_VALUES_FILE")"
  jq -n --arg name "$APPLICATION_NAME" --arg namespace "$APPLICATION_NAMESPACE" --arg repo "$APP_REPO_URL" \
    --arg revision "$ODH_GITOPS_REVISION" --arg path "$APP_CHART_PATH" --arg values "$values" --arg profile "$CHART_PROFILE" \
    '{apiVersion:"argoproj.io/v1alpha1",kind:"Application",metadata:{name:$name,namespace:$namespace,labels:{"app.kubernetes.io/part-of":"grid-llmd-e2e"}},spec:{project:"default",source:{repoURL:$repo,targetRevision:$revision,path:$path,helm:{values:$values,parameters:[{name:"profile",value:$profile},{name:"dependencies.rhcl.config.authorinoSpec.listener.tls.enabled",value:"false"}]}},destination:{server:"https://kubernetes.default.svc",namespace:$namespace},syncPolicy:{automated:{prune:false,selfHeal:true,allowEmpty:false},syncOptions:["SkipDryRunOnMissingResource=true"],retry:{limit:8,backoff:{duration:"10s",factor:2,maxDuration:"3m"}}}}}' \
    | oc apply -f - >"$EVIDENCE_DIR/application-apply.txt"
  log "Pinned Argo Application applied at $ODH_GITOPS_REVISION with prune=false"
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    apply_monitoringstack_resource_profile reduced-dev
  fi
  local deadline=$((SECONDS + PLATFORM_TIMEOUT_SECONDS)) sync health
  while ((SECONDS < deadline)); do
    sync="$(oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o jsonpath='{.status.sync.status}' 2>/dev/null || true)"
    health="$(oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o jsonpath='{.status.health.status}' 2>/dev/null || true)"
    if [[ "$sync" == Synced && "$health" == Healthy ]]; then
      log "Argo Application reached Synced/Healthy at revision $ODH_GITOPS_REVISION"
      return 0
    fi
    sleep 15
  done
  printf 'timed out waiting for pinned Argo Application to reach Synced/Healthy\n' >&2
  return 1
}

wait_for_gitops_instance() {
  local deadline phase
  CURRENT_PHASE=gitops-default-instance
  deadline=$((SECONDS + INSTALL_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    if oc -n openshift-gitops get argocd openshift-gitops -o name >/dev/null 2>&1; then
      phase="$(oc -n openshift-gitops get argocd openshift-gitops -o jsonpath='{.status.phase}' 2>/dev/null || true)"
      if [[ "$phase" == Available ]]; then
        log 'Default OpenShift GitOps ArgoCD instance is Available'
        return 0
      fi
    fi
    sleep 5
  done
  oc -n openshift-gitops get argocd openshift-gitops -o yaml >"$EVIDENCE_DIR/argocd-default-instance-timeout.yaml" 2>&1 || true
  printf 'timed out waiting for the GitOps operator to create and make openshift-gitops/openshift-gitops Available\n' >&2
  return 1
}

verify_monitoring_resource_profile() {
  local stack_file owner schema_ok schema_file
  CURRENT_PHASE=monitoring-dev-resource-override
  stack_file="$EVIDENCE_DIR/monitoringstack-current.json"
  oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack -o json >"$stack_file"
  owner="$(jq -r '[.metadata.ownerReferences[]?|select(.kind=="Monitoring" and .name=="default-monitoring" and (.apiVersion|startswith("services.platform.opendatahub.io/"))) ]|length' "$stack_file")"
  [[ "$owner" == 1 ]] || {
    printf 'MonitoringStack owner is not Monitoring/default-monitoring; profile verification failed\n' >&2
    return 1
  }
  schema_file="$EVIDENCE_DIR/monitoringstack-cpu-request-schema.json"
  oc get crd monitoringstacks.monitoring.rhobs -o json | jq '[
    .spec.versions[]|select(.served==true)|
    .schema.openAPIV3Schema.properties.spec.properties as $spec|
    {version:.name,
     prometheusRequests:$spec.resources.properties.requests.additionalProperties,
     thanosRequests:$spec.prometheusConfig.properties.thanosResources.properties.requests.additionalProperties}
  ]' >"$schema_file"
  schema_ok="$(jq -r '
    def is_quantity: .["x-kubernetes-int-or-string"]==true and
      ([.anyOf[]?.type]|index("integer")!=null) and ([.anyOf[]?.type]|index("string")!=null);
    length>0 and all(.[]; (.prometheusRequests|is_quantity) and (.thanosRequests|is_quantity))
  ' "$schema_file")"
  [[ "$schema_ok" == true ]] || {
    printf 'MonitoringStack CRD does not declare both CPU request fields; profile verification failed\n' >&2
    return 1
  }
  jq -e --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
    '.spec.resources.requests.cpu==$prometheus and .spec.prometheusConfig.thanosResources.requests.cpu==$thanos' "$stack_file" >/dev/null || {
    printf 'Argo-managed MonitoringStack does not match the selected CPU request profile\n' >&2
    return 1
  }
  jq --arg profile "$RESOURCE_PROFILE" --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
    '{resourceProfile:$profile,expectedPrometheusCpuRequest:$prometheus,expectedThanosCpuRequest:$thanos,name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,ownerReferences:[.metadata.ownerReferences[]?|{apiVersion,kind,name,uid}],prometheusCpuRequest:.spec.resources.requests.cpu,thanosCpuRequest:.spec.prometheusConfig.thanosResources.requests.cpu,prometheusMemoryRequest:.spec.resources.requests.memory,prometheusLimits:.spec.resources.limits,thanosMemoryRequest:.spec.prometheusConfig.thanosResources.requests.memory,thanosLimits:.spec.prometheusConfig.thanosResources.limits,replicas:(.spec.prometheusConfig.replicas//.spec.replicas//null),storage:.spec.prometheusConfig.persistentVolumeClaim.resources.requests.storage,retention:.spec.retention}' "$stack_file" \
    >"$EVIDENCE_DIR/monitoringstack-resource-profile-observed.json"
  if [[ -s "$EVIDENCE_DIR/monitoringstack-spec-before-profile.json" ]]; then
    jq -S '.spec|del(.resources.requests.cpu,.prometheusConfig.thanosResources.requests.cpu)' "$stack_file" \
      >"$TEMP_HELPER_DIR/monitoringstack-spec-after-profile.json"
    jq -S 'del(.resources.requests.cpu,.prometheusConfig.thanosResources.requests.cpu)' \
      "$EVIDENCE_DIR/monitoringstack-spec-before-profile.json" >"$TEMP_HELPER_DIR/monitoringstack-spec-before-profile-normalized.json"
    cmp -s "$TEMP_HELPER_DIR/monitoringstack-spec-before-profile-normalized.json" \
      "$TEMP_HELPER_DIR/monitoringstack-spec-after-profile.json" || {
      printf 'MonitoringStack fields other than the two declared CPU requests changed during profile reconciliation\n' >&2
      return 1
    }
    printf 'pass: only Prometheus/Thanos CPU requests differ from the initial MonitoringStack spec\n' \
      >"$EVIDENCE_DIR/monitoringstack-profile-preservation.status"
  fi
  verify_prometheus_statefulset_resources
  log "MonitoringStack profile verified: resourceProfile=$RESOURCE_PROFILE Prometheus=$PROMETHEUS_CPU_REQUEST Thanos=$THANOS_CPU_REQUEST; only the task-owned CR overlay fields were managed"
}

capture_prometheus_statefulset_resources() {
  local destination="$1" raw_file
  raw_file="$TEMP_HELPER_DIR/prometheus-statefulset-raw.json"
  oc -n "$RHOAI_MONITORING_NAMESPACE" get statefulset "$PROMETHEUS_STATEFULSET_NAME" -o json >"$raw_file" 2>/dev/null || return 1
  jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,replicas:(.spec.replicas//1),
    containers:[.spec.template.spec.containers[]?|{name,requests:(.resources.requests//{}),limits:(.resources.limits//{})}],
    initContainers:[.spec.template.spec.initContainers[]?|{name,requests:(.resources.requests//{}),limits:(.resources.limits//{})}]}' \
    "$raw_file" >"$destination"
}

verify_prometheus_statefulset_resources() {
  local deadline snapshot
  CURRENT_PHASE=monitoring-dev-resource-override
  deadline=$((SECONDS + MONITORING_OVERRIDE_TIMEOUT_SECONDS))
  snapshot="$EVIDENCE_DIR/prometheus-statefulset-resources-after-profile.json"
  while ((SECONDS < deadline)); do
    if capture_prometheus_statefulset_resources "$snapshot" && jq -e --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" '
      .replicas==1 and
      any(.containers[];.name=="prometheus" and .requests.cpu==$prometheus) and
      any(.containers[];.name=="thanos-sidecar" and .requests.cpu==$thanos)
    ' "$snapshot" >/dev/null; then
      if [[ -s "$EVIDENCE_DIR/prometheus-statefulset-resources-before-profile.json" ]]; then
        jq -n --arg prometheus "$PROMETHEUS_CPU_REQUEST" --arg thanos "$THANOS_CPU_REQUEST" \
          --slurpfile before "$EVIDENCE_DIR/prometheus-statefulset-resources-before-profile.json" \
          --slurpfile after "$snapshot" '
          def rows($x): ((($x.containers//[])|map({name,requests,limits})) + (($x.initContainers//[])|map({name:("init/"+.name),requests,limits})))|sort_by(.name);
          $before[0] as $b|$after[0] as $a|
          (rows($b)|map(.name))==(rows($a)|map(.name)) and
          $a.replicas==1 and
          all(rows($a)[]; . as $new | (rows($b)|map(select(.name==$new.name))|.[0]) as $old |
            if $new.name=="prometheus" then
              $new.requests.cpu==$prometheus and $new.requests.memory==$old.requests.memory and $new.limits==$old.limits
            elif $new.name=="thanos-sidecar" then
              $new.requests.cpu==$thanos and $new.requests.memory==$old.requests.memory and $new.limits==$old.limits
            else $new.requests==$old.requests and $new.limits==$old.limits end)
        ' >/dev/null || {
          printf 'Prometheus StatefulSet resource comparison found a non-CPU or unexpected container change\n' >&2
          return 1
        }
        printf 'pass\n' >"$EVIDENCE_DIR/prometheus-statefulset-resource-comparison.status"
      else
        printf 'No pre-profile StatefulSet existed; post-profile resources captured and checked against the selected CPU requests.\n' \
          >"$EVIDENCE_DIR/prometheus-statefulset-resource-comparison.status"
      fi
      log "Prometheus StatefulSet has one replica and profile requests Prometheus=$PROMETHEUS_CPU_REQUEST/Thanos=$THANOS_CPU_REQUEST; memory, limits, and other container requests were compared unchanged"
      return 0
    fi
    sleep 10
  done
  printf 'Prometheus StatefulSet did not reconcile to the selected single-replica resource requests within the configured timeout\n' >&2
  return 1
}

verify_monitoring_profile_survives_reconciliation() {
  local deadline stable_since current reapplied
  CURRENT_PHASE=monitoring-dev-resource-override
  deadline=$((SECONDS + MONITORING_OVERRIDE_TIMEOUT_SECONDS))
  stable_since=-1
  reapplied=false
  while ((SECONDS < deadline)); do
    if oc -n "$RHOAI_MONITORING_NAMESPACE" get monitoringstack data-science-monitoringstack -o json \
      >"$EVIDENCE_DIR/monitoringstack-reconciliation-sample.json" 2>/dev/null; then
      current="$(jq -r '[.spec.resources.requests.cpu // "unset",.spec.prometheusConfig.thanosResources.requests.cpu // "unset"]|join("/")' \
        "$EVIDENCE_DIR/monitoringstack-reconciliation-sample.json")"
      if [[ "$current" == "$PROMETHEUS_CPU_REQUEST/$THANOS_CPU_REQUEST" ]]; then
        if ((stable_since < 0)); then stable_since=$SECONDS; fi
        if ((SECONDS - stable_since >= 30)); then
          jq '{observedAt:now|todateiso8601,ownerReferences:[.metadata.ownerReferences[]?|{kind,name,uid}],prometheusCPURequest:.spec.resources.requests.cpu,thanosCPURequest:.spec.prometheusConfig.thanosResources.requests.cpu,prometheusReplicas:(.spec.prometheusConfig.replicas//.spec.replicas//null),storage:.spec.prometheusConfig.persistentVolumeClaim.resources.requests.storage,retention:.spec.retention}' \
            "$EVIDENCE_DIR/monitoringstack-reconciliation-sample.json" >"$EVIDENCE_DIR/monitoringstack-reconciliation-stability.json"
          log "Task-owned MonitoringStack overlay remained $PROMETHEUS_CPU_REQUEST/$THANOS_CPU_REQUEST through the bounded reconciliation observation window"
          verify_monitoring_resource_profile
          verify_prometheus_statefulset_resources
          return 0
        fi
      else
        stable_since=-1
        if [[ "$RESOURCE_PROFILE" == reduced-dev && "$reapplied" == false ]]; then
          jq '{observedAt:now|todateiso8601,prometheusCPURequest:.spec.resources.requests.cpu,thanosCPURequest:.spec.prometheusConfig.thanosResources.requests.cpu}' \
            "$EVIDENCE_DIR/monitoringstack-reconciliation-sample.json" >"$EVIDENCE_DIR/monitoringstack-profile-reconciliation-drift.json"
          log 'Monitoring reconciliation changed a selected CPU request; reapplying the documented server-side MonitoringStack CR overlay once'
          apply_monitoringstack_resource_profile reduced-dev
          reapplied=true
        fi
      fi
    fi
    sleep 15
  done
  printf 'MonitoringStack CPU requests did not remain stable for 30 seconds within the configured timeout\n' >&2
  return 1
}

set_platform_helm_parameters() {
  local authorino_tls_enabled="$1" patch_file
  [[ "$authorino_tls_enabled" == true || "$authorino_tls_enabled" == false ]] || return 2
  oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o json \
    | jq -e 'all(.spec.source.helm.parameters[]?;
        .name=="profile" or
        .name=="dependencies.rhcl.config.authorinoSpec.listener.tls.enabled" or
        .name=="services.monitoring.dsci.metrics.replicas" or
        .name=="services.monitoring.dsci.metrics.storage.size" or
        .name=="services.monitoring.dsci.metrics.storage.retention")' >/dev/null || {
      printf 'Argo Application has unexpected Helm parameters; refusing to replace them\n' >&2
      return 1
    }
  patch_file="$EVIDENCE_DIR/argo-helm-parameters-${authorino_tls_enabled}.json"
  jq -n --arg tls "$authorino_tls_enabled" --arg profile "$CHART_PROFILE" --arg replicas "$DSCI_METRICS_REPLICAS" \
    --arg size "$DSCI_METRICS_STORAGE_SIZE" --arg retention "$DSCI_METRICS_RETENTION" \
    '{spec:{source:{helm:{parameters:[
      {name:"profile",value:$profile},
      {name:"services.monitoring.dsci.metrics.replicas",value:$replicas},
      {name:"services.monitoring.dsci.metrics.storage.size",value:$size},
      {name:"services.monitoring.dsci.metrics.storage.retention",value:$retention},
      {name:"dependencies.rhcl.config.authorinoSpec.listener.tls.enabled",value:$tls}
    ]}}}}' >"$patch_file"
  oc -n "$APPLICATION_NAMESPACE" patch application "$APPLICATION_NAME" --type=merge --patch-file="$patch_file" \
    >"$EVIDENCE_DIR/argo-helm-parameters-${authorino_tls_enabled}.txt"
  log "Argo desired state retained chart profile=$CHART_PROFILE and DSCI metrics replicas=$DSCI_METRICS_REPLICAS storage=$DSCI_METRICS_STORAGE_SIZE retention=$DSCI_METRICS_RETENTION; Authorino TLS enabled=$authorino_tls_enabled"
}

wait_for_application_sync() {
  local authorino_tls_enabled="$1" deadline
  deadline=$((SECONDS + PLATFORM_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    if oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o json \
      | jq -e --arg revision "$ODH_GITOPS_REVISION" --arg tls "$authorino_tls_enabled" --arg profile "$CHART_PROFILE" '
          .spec.source.targetRevision==$revision and .status.sync.status=="Synced" and
          .status.health.status=="Healthy" and
          any(.spec.source.helm.parameters[]?;.name=="profile" and .value==$profile) and
          any(.spec.source.helm.parameters[]?;
            .name=="dependencies.rhcl.config.authorinoSpec.listener.tls.enabled" and .value==$tls)
        ' >/dev/null; then
      return 0
    fi
    sleep 10
  done
  printf 'timed out waiting for pinned Argo Application with Authorino TLS enabled=%s\n' "$authorino_tls_enabled" >&2
  return 1
}

bootstrap_authorino_tls() {
  local annotation secret_ready elapsed
  CURRENT_PHASE=authorino-tls-bootstrap
  oc -n "$APPLICATION_NAMESPACE" get application "$APPLICATION_NAME" -o json \
    | jq -e '.metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e"' >/dev/null || {
      printf 'Argo Application is not task-owned; refusing Authorino TLS bootstrap\n' >&2
      return 1
    }
  set_platform_helm_parameters false
  wait_for_application_sync false
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then apply_monitoringstack_resource_profile reduced-dev; fi

  for ((elapsed=0; elapsed<PLATFORM_TIMEOUT_SECONDS; elapsed+=5)); do
    if oc -n "$AUTHORINO_NAMESPACE" get service "$AUTHORINO_SERVICE_NAME" -o json 2>/dev/null \
      | jq -e --arg authorino "$AUTHORINO_NAME" '
          any(.metadata.ownerReferences[]?;.kind=="Authorino" and .name==$authorino) and
          .spec.selector["authorino-resource"]==$authorino
        ' >/dev/null 2>&1; then
      break
    fi
    sleep 5
  done
  ((elapsed < PLATFORM_TIMEOUT_SECONDS)) || {
    printf 'timed out waiting for the task-owned Authorino authorization Service\n' >&2
    return 1
  }

  annotation="$(oc -n "$AUTHORINO_NAMESPACE" get service "$AUTHORINO_SERVICE_NAME" -o jsonpath='{.metadata.annotations.service\.beta\.openshift\.io/serving-cert-secret-name}' 2>/dev/null || true)"
  if [[ -n "$annotation" && "$annotation" != "$AUTHORINO_TLS_SECRET" ]]; then
    printf 'Authorino Service already requests a different serving certificate Secret\n' >&2
    return 1
  fi
  if [[ -z "$annotation" ]]; then
    oc -n "$AUTHORINO_NAMESPACE" annotate service "$AUTHORINO_SERVICE_NAME" \
      "service.beta.openshift.io/serving-cert-secret-name=$AUTHORINO_TLS_SECRET" --overwrite \
      >"$EVIDENCE_DIR/authorino-service-annotation.txt"
  fi
  secret_ready=false
  for ((elapsed=0; elapsed<AUTHORINO_CERT_TIMEOUT_SECONDS; elapsed+=5)); do
    if oc -n "$AUTHORINO_NAMESPACE" get secret "$AUTHORINO_TLS_SECRET" -o json 2>/dev/null \
      | jq '{name:.metadata.name,namespace:.metadata.namespace,created:.metadata.creationTimestamp,type:.type,dataKeys:(.data|keys)}' \
      >"$EVIDENCE_DIR/authorino-cert-metadata.json" 2>/dev/null; then
      jq -e '.type=="kubernetes.io/tls" and (.dataKeys|index("tls.crt")!=null) and (.dataKeys|index("tls.key")!=null)' \
        "$EVIDENCE_DIR/authorino-cert-metadata.json" >/dev/null && secret_ready=true && break
    fi
    sleep 5
  done
  [[ "$secret_ready" == true ]] || {
    printf 'timed out waiting for an OpenShift service-ca TLS Secret with certificate/key keys\n' >&2
    return 1
  }
  set_platform_helm_parameters true
  wait_for_application_sync true
  if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
    apply_monitoringstack_resource_profile reduced-dev
    verify_monitoring_profile_survives_reconciliation
  fi
  oc -n "$AUTHORINO_NAMESPACE" wait --for=condition=Ready "authorino/$AUTHORINO_NAME" --timeout="${PLATFORM_TIMEOUT_SECONDS}s"
  oc -n "$AUTHORINO_NAMESPACE" rollout status "deployment/$AUTHORINO_NAME" --timeout="${PLATFORM_TIMEOUT_SECONDS}s"
  oc -n "$AUTHORINO_NAMESPACE" get endpointslice -l "kubernetes.io/service-name=$AUTHORINO_SERVICE_NAME" -o json \
    | jq -e 'any(.items[]?;any(.endpoints[]?;.conditions.ready==true))' >/dev/null || {
      printf 'Authorino authorization Service has no ready EndpointSlice target\n' >&2
      return 1
    }
  oc -n "$AUTHORINO_NAMESPACE" get authorino "$AUTHORINO_NAME" -o json \
    | jq '{name:.metadata.name,namespace:.metadata.namespace,tlsEnabled:.spec.listener.tls.enabled,certSecretRef:.spec.listener.tls.certSecretRef.name,conditions:[.status.conditions[]?|{type,status,reason}]}' \
    >"$EVIDENCE_DIR/authorino-readiness.json"
  log "Authorino TLS bootstrap passed for $AUTHORINO_NAMESPACE/$AUTHORINO_NAME; Secret contents excluded"
}

record_database_metadata() {
  oc -n redhat-ai-gateway-infra get deployment postgres -o json \
    | jq '{kind:.kind,name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,created:.metadata.creationTimestamp,labels:.metadata.labels,ownerReferences:[.metadata.ownerReferences[]?|{kind,name,uid}],images:[.spec.template.spec.containers[]|{name,image}],replicas:{desired:(.spec.replicas//1),ready:(.status.readyReplicas//0),available:(.status.availableReplicas//0)}}' \
    >"$EVIDENCE_DIR/postgres-deployment-metadata.json"
  oc -n redhat-ai-gateway-infra get service postgres -o json \
    | jq '{kind:.kind,name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,created:.metadata.creationTimestamp,labels:.metadata.labels,ownerReferences:[.metadata.ownerReferences[]?|{kind,name,uid}],selector:.spec.selector,ports:[.spec.ports[]?|{name,port,targetPort,protocol}]}' \
    >"$EVIDENCE_DIR/postgres-service-metadata.json"
  oc -n redhat-ai-gateway-infra get secret maas-db-config postgres-creds -o json \
    | jq '[.items[]|{kind:.kind,name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,created:.metadata.creationTimestamp,labels:.metadata.labels,type:.type,dataKeys:(.data|keys)}]' \
    >"$EVIDENCE_DIR/postgres-secret-metadata.json"
}

verify_task_owned_database() {
  local expected_image="$1" owner actual_image resource
  for resource in deployment/postgres service/postgres secret/postgres-creds secret/maas-db-config; do
    owner="$(oc -n redhat-ai-gateway-infra get "$resource" -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)"
    [[ "$owner" == cold-grid-llmd-e2e ]] || {
      printf 'database resource %s is not owned by this E2E task; refusing to reuse or alter it\n' "$resource" >&2
      return 1
    }
  done
  actual_image="$(oc -n redhat-ai-gateway-infra get deployment postgres -o json \
    | jq -er '[.spec.template.spec.containers[].image]|if length==1 then .[0] else error("expected one PostgreSQL container") end')"
  [[ "$actual_image" == "$expected_image" ]] || {
    printf 'task-owned PostgreSQL image differs from the exact installed RHOAI CSV digest\n' >&2
    return 1
  }
  oc -n redhat-ai-gateway-infra get deployment postgres -o json \
    | jq -e '(.spec.replicas//1)>0 and (.status.readyReplicas//0)==(.spec.replicas//1) and (.status.availableReplicas//0)==(.spec.replicas//1)' >/dev/null || {
      printf 'task-owned PostgreSQL Deployment is not Ready\n' >&2
      return 1
    }
  oc -n redhat-ai-gateway-infra get endpointslice -l kubernetes.io/service-name=postgres -o json \
    | jq -e 'any(.items[]?;any(.endpoints[]?;.conditions.ready==true))' >/dev/null || {
      printf 'task-owned PostgreSQL Service has no ready EndpointSlice target\n' >&2
      return 1
    }
  oc -n redhat-ai-gateway-infra get secret maas-db-config -o json \
    | jq -e '(.data|keys|index("DB_CONNECTION_URL"))!=null' >/dev/null || {
      printf 'task-owned maas-db-config lacks DB_CONNECTION_URL\n' >&2
      return 1
    }
  record_database_metadata
}

configure_maas_database() {
  local image script helper helper_status resource_namespace owner resource db_count kubectl_server KUBECTL_BIN source_head source_status
  CURRENT_PHASE=database-prerequisite
  [[ "$ALLOW_POC_DATABASE" == true ]] || {
    printf 'set ALLOW_POC_DATABASE=true to enable the documented ephemeral emptyDir database\n' >&2
    return 1
  }
  if [[ ! -d "$MAAS_SOURCE_DIR" ]] || ! git -C "$MAAS_SOURCE_DIR" rev-parse --is-inside-work-tree 2>/dev/null | grep -qx true; then
    printf 'MAAS_SOURCE_DIR must identify an existing Git worktree\n' >&2
    return 1
  fi
  [[ "$MAAS_SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || { printf 'MAAS_SOURCE_REVISION must be a full immutable SHA\n' >&2; return 1; }
  git -C "$MAAS_SOURCE_DIR" cat-file -e "$MAAS_SOURCE_REVISION^{commit}" || return 1
  source_head="$(git -C "$MAAS_SOURCE_DIR" rev-parse HEAD)"
  [[ "$source_head" == "$MAAS_SOURCE_REVISION" ]] || {
    printf 'MAAS_SOURCE_REVISION does not match the clean source worktree HEAD\n' >&2
    return 1
  }
  if git -C "$MAAS_SOURCE_DIR" symbolic-ref -q HEAD >/dev/null 2>&1; then
    printf 'MaaS source worktree must be detached at the recorded SHA\n' >&2
    return 1
  fi
  source_status="$(git -C "$MAAS_SOURCE_DIR" status --porcelain)"
  [[ -z "$source_status" ]] || {
    printf 'MaaS source worktree must be clean before running the pinned database helper\n' >&2
    return 1
  }
  command -v python3 >/dev/null || { printf 'python3 is required to pin the helper input\n' >&2; return 1; }
  command -v kubectl >/dev/null || { printf 'kubectl is required by the supported MaaS helper\n' >&2; return 1; }
  [[ -n "$KUBE_CONTEXT" ]] || { printf 'KUBE_CONTEXT is required for MaaS database bootstrap\n' >&2; return 1; }
  kubectl_server="$(command kubectl --context="$KUBE_CONTEXT" config view --raw --minify -o jsonpath='{.clusters[0].cluster.server}')"
  [[ "$kubectl_server" == "$(oc whoami --show-server)" ]] || {
    printf 'kubectl selected API server differs from pinned oc API server; refusing database bootstrap\n' >&2
    return 1
  }
  printf 'selected_context=%s\nkubectl_api_server=%s\n' "$KUBE_CONTEXT" "$kubectl_server" >"$EVIDENCE_DIR/maas-kubectl-context.txt"
  # The MaaS helper has an upgrade path that can copy an existing database URL
  # from legacy namespaces. Refuse those locations and reuse only the exact
  # task-owned set in redhat-ai-gateway-infra.
  for resource_namespace in redhat-ai-gateway-infra redhat-ods-applications opendatahub; do
    if oc -n "$resource_namespace" get deployment postgres >/dev/null 2>&1; then
      [[ "$resource_namespace" == redhat-ai-gateway-infra ]] || {
        printf 'legacy PostgreSQL Deployment exists at %s; refusing cross-namespace helper reuse\n' "$resource_namespace" >&2
        return 1
      }
      owner="$(oc -n "$resource_namespace" get deployment postgres -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)"
      [[ "$owner" == cold-grid-llmd-e2e ]] || {
        printf 'unowned PostgreSQL deployment exists at %s/postgres; refusing helper upgrade path\n' "$resource_namespace" >&2
        return 1
      }
    fi
    if oc -n "$resource_namespace" get service postgres >/dev/null 2>&1; then
      [[ "$resource_namespace" == redhat-ai-gateway-infra ]] || {
        printf 'legacy PostgreSQL Service exists at %s; refusing cross-namespace helper reuse\n' "$resource_namespace" >&2
        return 1
      }
      owner="$(oc -n "$resource_namespace" get service postgres -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)"
      [[ "$owner" == cold-grid-llmd-e2e ]] || {
        printf 'unowned PostgreSQL Service exists at %s/postgres; refusing to modify it\n' "$resource_namespace" >&2
        return 1
      }
    fi
    if oc -n "$resource_namespace" get secret maas-db-config >/dev/null 2>&1; then
      [[ "$resource_namespace" == redhat-ai-gateway-infra ]] || {
        printf 'legacy maas-db-config exists at %s; refusing cross-namespace helper reuse\n' "$resource_namespace" >&2
        return 1
      }
      owner="$(oc -n "$resource_namespace" get secret maas-db-config -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)"
      [[ "$owner" == cold-grid-llmd-e2e ]] || {
        printf 'unowned maas-db-config Secret exists at %s; refusing to read or replace it\n' "$resource_namespace" >&2
        return 1
      }
    fi
  done
  if oc -n redhat-ai-gateway-infra get secret postgres-creds >/dev/null 2>&1; then
    owner="$(oc -n redhat-ai-gateway-infra get secret postgres-creds -o jsonpath='{.metadata.labels.app\.kubernetes\.io/managed-by}' 2>/dev/null || true)"
    [[ "$owner" == cold-grid-llmd-e2e ]] || {
      printf 'unowned postgres-creds Secret exists in redhat-ai-gateway-infra; refusing to reuse it\n' >&2
      return 1
    }
  fi
  [[ -n "$RHOAI_CSV" ]] || { printf 'RHOAI_CSV must name the installed RHOAI CSV\n' >&2; return 1; }
  if [[ -z "$TEMP_HELPER_DIR" ]]; then
    TEMP_HELPER_DIR="$(mktemp -d "${TMPDIR:-/tmp}/grid-llmd-maas-db.XXXXXX")"
    chmod 0700 "$TEMP_HELPER_DIR"
  fi
  oc -n "$RHOAI_NAMESPACE" get csv "$RHOAI_CSV" -o json >"$TEMP_HELPER_DIR/rhoai-csv.json" 2>/dev/null || {
    printf 'configured RHOAI CSV %s is missing from namespace %s\n' "$RHOAI_CSV" "$RHOAI_NAMESPACE" >&2
    return 1
  }
  [[ "$(jq -r '.status.phase // ""' "$TEMP_HELPER_DIR/rhoai-csv.json")" == Succeeded ]] || {
    printf 'configured RHOAI CSV %s is not Succeeded\n' "$RHOAI_CSV" >&2
    return 1
  }
  image="$(jq -er '[.spec.relatedImages[]|select(.name=="postgresql_16_image")|.image] | if length==1 then .[0] else error("expected exactly one PostgreSQL image") end' "$TEMP_HELPER_DIR/rhoai-csv.json")"
  [[ "$image" =~ @sha256:[0-9a-f]{64}$ ]] || { printf 'RHOAI CSV PostgreSQL image is not digest pinned\n' >&2; return 1; }
  [[ "$image" != *:latest* ]] || { printf 'mutable :latest PostgreSQL image reference is forbidden\n' >&2; return 1; }
  {
    printf '\n## MaaS development database input lock\n\n'
    printf -- '- MaaS source revision: `%s`\n' "$MAAS_SOURCE_REVISION"
    printf -- '- RHOAI CSV: `%s/%s`\n' "$RHOAI_NAMESPACE" "$RHOAI_CSV"
    printf -- '- PostgreSQL image from `postgresql_16_image`: `%s`\n' "$image"
    printf -- '- Secret values: excluded; only names, keys, and metadata are recorded\n'
  } >>"$EVIDENCE_DIR/source-lock.md"
  {
    printf 'MaaS source revision: %s\n' "$MAAS_SOURCE_REVISION"
    printf 'RHOAI CSV: %s\n' "$RHOAI_CSV"
    printf 'PostgreSQL image from RHOAI CSV postgresql_16_image: %s\n' "$image"
    printf 'Storage: development emptyDir (ephemeral; not for production)\n'
    printf 'Ownership: task bootstrap resources labeled app.kubernetes.io/managed-by=cold-grid-llmd-e2e\n'
    printf 'Database Secret values: excluded\n'
  } >"$EVIDENCE_DIR/maas-database-helper-inputs.md"

  db_count=0
  for resource in deployment/postgres service/postgres secret/postgres-creds secret/maas-db-config; do
    if oc -n redhat-ai-gateway-infra get "$resource" >/dev/null 2>&1; then
      ((db_count+=1))
    fi
  done
  if ((db_count > 0)); then
    ((db_count == 4)) || {
      printf 'partial PostgreSQL bootstrap state exists; refusing to overwrite or complete it automatically\n' >&2
      return 1
    }
    verify_task_owned_database "$image"
    log "Reused all four task-owned database resources after matching CSV=$RHOAI_CSV and exact PostgreSQL digest=$image; no Secret values read"
    return 0
  fi
  script="$TEMP_HELPER_DIR/setup-database.sh"
  helper="$TEMP_HELPER_DIR/deployment-helpers.sh"
  mkdir -p "$TEMP_HELPER_DIR/bin"
  KUBECTL_BIN="$(command -v kubectl)"
  cat >"$TEMP_HELPER_DIR/bin/kubectl" <<EOF
#!/usr/bin/env bash
exec "$KUBECTL_BIN" --context="$KUBE_CONTEXT" "\$@"
EOF
  chmod 0700 "$TEMP_HELPER_DIR/bin/kubectl"
  git -C "$MAAS_SOURCE_DIR" show "$MAAS_SOURCE_REVISION:scripts/setup-database.sh" >"$script"
  git -C "$MAAS_SOURCE_DIR" show "$MAAS_SOURCE_REVISION:scripts/deployment-helpers.sh" >"$helper"
  python3 - "$script" "$helper" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
helper = Path(sys.argv[2])
s = p.read_text()
replacements = {
    'DEFAULT_POSTGRES_IMAGE="registry.redhat.io/rhel9/postgresql-16:latest"':
        'DEFAULT_POSTGRES_IMAGE="$MAAS_POSTGRES_IMAGE"',
    'POSTGRES_IMAGE="$(resolve_postgres_image)"':
        'POSTGRES_IMAGE="$MAAS_POSTGRES_IMAGE"',
    'kubectl apply -n "$INFRA_NAMESPACE" -f -':
        'kubectl create -n "$INFRA_NAMESPACE" -f -',
    '    purpose: poc':
        '    purpose: poc\n    app.kubernetes.io/managed-by: cold-grid-llmd-e2e\n    app.kubernetes.io/part-of: grid-llmd-e2e',
    '''if [[ "${POSTGRES_IMAGE}" == "${DEFAULT_POSTGRES_IMAGE}" ]]; then
  echo "  Using default PostgreSQL image (operator CSV not available)"
else
  echo "  Resolved PostgreSQL image from operator CSV"
fi''':
        'echo "  Using exact digest-pinned PostgreSQL image from the RHOAI CSV"',
}
for old, new in replacements.items():
    expected = 3 if old == '    purpose: poc' else 1
    if s.count(old) != expected:
        raise SystemExit("MaaS helper shape changed; refusing to run an unreviewed helper")
    s = s.replace(old, new)
p.write_text(s)

# The pinned upstream helper creates the POC resources but has no run ownership
# labels. Add labels only to those helper-owned objects in the temporary copy.
h = helper.read_text()
old = '''  # Pass the connection URL via stdin to avoid exposing credentials in process arguments
  printf '%s' "$connection_url" | \\
    kubectl create secret generic maas-db-config \\
      --from-file=DB_CONNECTION_URL=/dev/stdin \\
      --dry-run=client -o yaml | \\
    kubectl label --local -f - app=maas-api --dry-run=client -o yaml | \\
    kubectl apply -n "$namespace" -f -'''
new = '''  # Reuse only this run's existing Secret. Never read or print its data.
  local existing_owner
  existing_owner="$(kubectl -n "$namespace" get secret maas-db-config -o jsonpath='{.metadata.labels.app\\.kubernetes\\.io/managed-by}' 2>/dev/null || true)"
  if [[ -n "$existing_owner" ]]; then
    [[ "$existing_owner" == cold-grid-llmd-e2e ]] || {
      log_error "maas-db-config exists without this run's ownership label"
      return 1
    }
    return 0
  fi

  # Pipe the value through stdin and create without an apply annotation.
  printf '%s' "$connection_url" | \\
    kubectl create secret generic maas-db-config \\
      --from-file=DB_CONNECTION_URL=/dev/stdin \\
      --dry-run=client -o yaml | \\
    kubectl label --local -f - app=maas-api app.kubernetes.io/managed-by=cold-grid-llmd-e2e app.kubernetes.io/part-of=grid-llmd-e2e --overwrite --dry-run=client -o yaml | \\
    kubectl create -n "$namespace" -f -'''
if h.count(old) != 1:
    raise SystemExit("MaaS Secret helper shape changed; refusing an unreviewed secret write")
h = h.replace(old, new)
helper.write_text(h)
PY
  sha256sum "$script" "$helper" >"$EVIDENCE_DIR/maas-database-helper-sha256.txt"
  log "Applying the pinned MaaS development database helper; CSV=$RHOAI_CSV source=$MAAS_SOURCE_REVISION image=$image; Secret values excluded"
  if PATH="$TEMP_HELPER_DIR/bin:$PATH" NAMESPACE=redhat-ods-applications INFRA_NAMESPACE=redhat-ai-gateway-infra MAAS_POSTGRES_IMAGE="$image" \
      bash "$script" >"$TEMP_HELPER_DIR/helper-output.log" 2>&1; then
    helper_status=0
  else
    helper_status=$?
  fi
  {
    printf 'helper_exit_status=%s\n' "$helper_status"
    printf 'helper_stdout_stderr=omitted to prevent credential or connection-string leakage\n'
    printf 'helper_source_revision=%s\n' "$MAAS_SOURCE_REVISION"
    printf 'postgres_image=%s\n' "$image"
  } >"$EVIDENCE_DIR/maas-database-helper.log"
  if ((helper_status != 0)); then
    log 'MaaS database helper failed; raw output remained in a mode-0700 temporary directory and was removed on exit'
    return 1
  fi
  oc -n redhat-ai-gateway-infra rollout status deployment/postgres --timeout=300s
  verify_task_owned_database "$image"
  oc -n redhat-ai-gateway-infra get secret maas-db-config postgres-creds \
    -o custom-columns='NAME:.metadata.name,TYPE:.type,CREATED:.metadata.creationTimestamp' \
    >"$EVIDENCE_DIR/maas-database-secret-metadata.txt"
  oc -n redhat-ai-gateway-infra get deployment/postgres service/postgres -o wide \
    >"$EVIDENCE_DIR/postgres-resources.txt"
  oc -n redhat-ai-gateway-infra get endpoints postgres -o wide \
    >"$EVIDENCE_DIR/postgres-endpoints.txt"
  oc -n redhat-ai-gateway-infra get secret maas-db-config -o json \
    | jq '{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,dataKeys:(.data|keys)}' \
    >"$EVIDENCE_DIR/maas-db-config-metadata.json"
  log 'MaaS database Deployment is ready; only Secret metadata was captured'
}

verify_platform() {
  local gate_count=0 failed_gates=0 gate_file="$EVIDENCE_DIR/.platform-gates.ndjson"
  CURRENT_PHASE=platform-readiness
  oc -n "$APPLICATION_NAMESPACE" get application.argoproj.io "$APPLICATION_NAME" -o json \
    | jq '{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,source:.spec.source.repoURL,path:.spec.source.path,targetRevision:.spec.source.targetRevision,pruneEnabled:(.spec.syncPolicy.automated.prune//false),syncStatus:.status.sync.status,syncedRevision:.status.sync.revision,health:.status.health.status,lastOperationPhase:.status.operationState.phase,lastOperationFinishedAt:.status.operationState.finishedAt}' \
    >"$EVIDENCE_DIR/argo-application-state.json"
  oc -n "$APPLICATION_NAMESPACE" get application.argoproj.io "$APPLICATION_NAME" -o json \
    | jq '[.status.resources[]?|select(.status!="Synced")|{group,kind,namespace,name,status,health}]' \
    >"$EVIDENCE_DIR/argo-out-of-sync-resources.json"
  oc get datasciencecluster default-dsc -o json | jq '[.status.conditions[]?|select(.type=="Ready" or .type=="AIGatewayReady" or .type=="KserveReady" or .type=="KserveLLMInferenceServiceDependencies" or .type=="KserveLLMInferenceServiceWideEPDependencies" or .type=="ModelsAsAServiceReady")|{type,status,reason,message}]' >"$EVIDENCE_DIR/rhoai-readiness.json"
  oc get dsci default-dsci -o json | jq '[.status.conditions[]?|select(.type=="Ready" or .type=="MonitoringStackAvailable" or .type=="OpenTelemetryCollectorAvailable" or .type=="ThanosQuerierAvailable" or .type=="PersesAvailable")|{type,status,reason,message}]' >"$EVIDENCE_DIR/dsci-monitoring-readiness.json"
  oc -n "$RHOAI_MONITORING_NAMESPACE" get statefulset "$PROMETHEUS_STATEFULSET_NAME" -o json \
    | jq '{name:.metadata.name,namespace:.metadata.namespace,desired:(.spec.replicas//0),ready:(.status.readyReplicas//0),updated:(.status.updatedReplicas//0),current:(.status.currentReplicas//0)}' \
    >"$EVIDENCE_DIR/prometheus-statefulset-readiness.json"
  oc -n "$RHOAI_MONITORING_NAMESPACE" get endpointslice -l "kubernetes.io/service-name=$PROMETHEUS_SERVICE_NAME" -o json \
    | jq '[.items[]|{name:.metadata.name,ready:[.endpoints[]?|select(.conditions.ready==true)]|length,total:(.endpoints|length)}]' \
    >"$EVIDENCE_DIR/prometheus-endpoints.json"
  oc get maastenantconfig -A -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,conditions:[.status.conditions[]?|{type,status,reason,message,observedGeneration}]}]' >"$EVIDENCE_DIR/maas-readiness.json"
  oc get aitenant -A -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,conditions:[.status.conditions[]?|{type,status,reason,message,observedGeneration}]}]' >"$EVIDENCE_DIR/aitenant-readiness.json"
  oc -n redhat-ai-gateway-infra get deployment maas-api -o json | jq '{name:.metadata.name,replicas:(.spec.replicas//1),ready:(.status.readyReplicas//0),available:(.status.availableReplicas//0),conditions:[.status.conditions[]?|{type,status,reason}]}' >"$EVIDENCE_DIR/maas-api-readiness.json"
  oc -n redhat-ai-gateway-infra get endpointslice -l kubernetes.io/service-name=maas-api -o json | jq '[.items[]|{name:.metadata.name,ready:[.endpoints[]?|select(.conditions.ready==true)]|length,total:(.endpoints|length)}]' >"$EVIDENCE_DIR/maas-api-endpoints.json"
  oc -n redhat-ai-gateway-infra get deployment/postgres service/postgres -o json | jq '[.items[]|{kind:.kind,name:.metadata.name,owner:.metadata.labels["app.kubernetes.io/managed-by"],image:(.spec.template.spec.containers[0].image//null),ready:(.status.readyReplicas//0),available:(.status.availableReplicas//0)}]' >"$EVIDENCE_DIR/postgres-ownership.json"
  oc -n redhat-ai-gateway-infra get endpointslice -l kubernetes.io/service-name=postgres -o json | jq '[.items[]|{name:.metadata.name,ready:[.endpoints[]?|select(.conditions.ready==true)]|length,total:(.endpoints|length)}]' >"$EVIDENCE_DIR/postgres-endpoints.json"
  oc -n redhat-ai-gateway-infra get secret maas-db-config postgres-creds -o json | jq '[.items[]|{name:.metadata.name,owner:.metadata.labels["app.kubernetes.io/managed-by"],dataKeys:(.data|keys)}]' >"$EVIDENCE_DIR/postgres-secret-metadata.json"
  secret_created="$(oc -n redhat-ai-gateway-infra get secret maas-db-config -o jsonpath='{.metadata.creationTimestamp}')"
  oc -n redhat-ai-gateway-infra logs deployment/maas-api --since-time="$secret_created" --tail=500 2>/dev/null \
    | python3 -c 'import json,re,sys; t=sys.stdin.read(); patterns={"missingDatabaseSecretMessagePresent":r"(?i)(?:(?:maas-db-config|DB_CONNECTION_URL).{0,100}(?:not found|missing|not set|unset|empty)|(?:not found|missing|not set|unset|empty).{0,100}(?:maas-db-config|DB_CONNECTION_URL))", "databaseConnectionFailurePresent":r"(?i)(?:(?:database|postgres(?:ql)?|DB_CONNECTION_URL).{0,120}(?:connection refused|could not connect|failed to connect|connection error|dial tcp|no route to host|timed out|timeout|authentication failed|server closed the connection)|(?:connection refused|could not connect|failed to connect|connection error|dial tcp|no route to host|timed out|timeout|authentication failed|server closed the connection).{0,120}(?:database|postgres(?:ql)?|DB_CONNECTION_URL))", "databaseConfigurationFailurePresent":r"(?i)(?:(?:maas-db-config|DB_CONNECTION_URL|database config(?:uration)?|postgres(?:ql)? config(?:uration)?).{0,120}(?:invalid|missing|not set|unset|empty|failed|error|parse|cannot|could not|unavailable)|(?:invalid|missing|not set|unset|empty|failed|error|parse|cannot|could not|unavailable).{0,120}(?:maas-db-config|DB_CONNECTION_URL|database config(?:uration)?|postgres(?:ql)? config(?:uration)?))"}; print(json.dumps({"sinceSecretCreated":True,"logRead":bool(t),**{k:bool(re.search(v,t)) for k,v in patterns.items()},"rawLogsPersisted":False}))' \
    >"$EVIDENCE_DIR/maas-api-log-gate.json" || true
  oc -n kuadrant-system get authorino authorino -o json | jq '[.status.conditions[]?|{type,status,reason,message}]' >"$EVIDENCE_DIR/authorino-readiness.json"
  oc -n kuadrant-system get pods -l authorino-resource=authorino -o json | jq '[.items[]|{name:.metadata.name,ready:([.status.conditions[]?|select(.type=="Ready")|.status][0]=="True"),phase:.status.phase}]' >"$EVIDENCE_DIR/authorino-pods.json"
  oc -n kuadrant-system get secret authorino-server-cert -o json | jq '{name:.metadata.name,namespace:.metadata.namespace,type:.type,dataKeys:(.data|keys)}' >"$EVIDENCE_DIR/authorino-secret-metadata.json"
  oc -n kuadrant-system get endpointslice -l kubernetes.io/service-name=authorino-authorino-authorization -o json | jq '[.items[]|{name:.metadata.name,ready:[.endpoints[]?|select(.conditions.ready==true)]|length,total:(.endpoints|length)}]' >"$EVIDENCE_DIR/authorino-endpoints.json"
  oc -n openshift-ingress get gateway maas-default-gateway -o json | jq '[.status.conditions[]?|select(.type=="Accepted" or .type=="Programmed")|{type,status,reason,observedGeneration}]' >"$EVIDENCE_DIR/maas-gateway-readiness.json"
  oc -n redhat-ods-applications get endpointslice -o json | jq '
    [.items[]|
      select(.metadata.labels["kubernetes.io/service-name"]=="kserve-webhook-server-service" or
             .metadata.labels["kubernetes.io/service-name"]=="llmisvc-webhook-server-service" or
             .metadata.labels["kubernetes.io/service-name"]=="odh-model-controller-webhook-service")|
      {service:.metadata.labels["kubernetes.io/service-name"],ready:[.endpoints[]?|select(.conditions.ready==true)]|length,total:(.endpoints|length)}]
    | group_by(.service)
    | map({service:.[0].service,endpointSlices:length,ready:(map(.ready)|add),total:(map(.total)|add)})
  ' >"$EVIDENCE_DIR/webhook-endpoints.json"

  : >"$gate_file"
  gate_check() {
    local name="$1" description="$2" file="$3" expression="$4" status
    if jq -e "$expression" "$file" >/dev/null 2>&1; then
      status=PASS
    else
      status=BLOCKED
      failed_gates=$((failed_gates + 1))
    fi
    gate_count=$((gate_count + 1))
    jq -cn --arg name "$name" --arg description "$description" --arg status "$status" \
      '{name:$name,description:$description,status:$status}' >>"$gate_file"
    log "$description $status"
  }

  gate_check rhoai 'RHOAI DataScienceCluster/KServe/MaaS readiness' "$EVIDENCE_DIR/rhoai-readiness.json" \
    '([.[]|select(.type=="Ready" or .type=="AIGatewayReady" or .type=="KserveReady" or .type=="KserveLLMInferenceServiceDependencies" or .type=="ModelsAsAServiceReady")]|length==5) and all(.[]|select(.type!="KserveLLMInferenceServiceWideEPDependencies");.status=="True")'
  gate_check dsci-monitoring 'DSCI MonitoringStackAvailable condition' "$EVIDENCE_DIR/dsci-monitoring-readiness.json" \
    'any(.[];.type=="MonitoringStackAvailable" and .status=="True")'
  gate_check monitoring-resource-profile 'Argo-selected MonitoringStack request profile' "$EVIDENCE_DIR/monitoringstack-resource-profile-observed.json" \
    '.prometheusCpuRequest==.expectedPrometheusCpuRequest and .thanosCpuRequest==.expectedThanosCpuRequest and .replicas==1 and .storage=="5Gi" and .retention=="24h"'
  gate_check prometheus-statefulset 'Prometheus StatefulSet replicas Ready' "$EVIDENCE_DIR/prometheus-statefulset-readiness.json" \
    '.desired>0 and .ready==.desired and .updated==.desired'
  gate_check prometheus-endpoints 'Prometheus Service has ready endpoints' "$EVIDENCE_DIR/prometheus-endpoints.json" \
    'length>0 and any(.[];.ready>0)'
  if ! jq -e 'any(.[];.type=="OpenTelemetryCollectorAvailable" and .status=="True")' "$EVIDENCE_DIR/dsci-monitoring-readiness.json" >/dev/null; then
    cat >"$EVIDENCE_DIR/otel-optional-for-direct-smoke.md" <<'NOTE'
# DSCI OpenTelemetry status for this smoke

OpenTelemetryCollectorAvailable is recorded as optional for the direct Grid + llm-d EPP signal-routing smoke. The required DSCI gate is
MonitoringStackAvailable=True. The routing qualification samples EPP metrics directly and does not use OTel traces for its routing assertion.
This waiver does not claim OTel readiness or MaaS tracing/showback readiness.
NOTE
  fi
  gate_check maas-tenant 'MaaSTenantConfig/default-tenant Ready' "$EVIDENCE_DIR/maas-readiness.json" \
    'any(.[];.namespace=="models-as-a-service" and .name=="default-tenant" and any(.conditions[]?;.type=="Ready" and .status=="True"))'
  gate_check ai-tenant 'AITenant/models-as-a-service Ready' "$EVIDENCE_DIR/aitenant-readiness.json" \
    'any(.[];.namespace=="ai-tenants" and .name=="models-as-a-service" and any(.conditions[]?;.type=="Ready" and .status=="True"))'
  gate_check maas-api 'MaaS API Deployment Available' "$EVIDENCE_DIR/maas-api-readiness.json" \
    '.replicas>0 and .ready==.replicas and .available==.replicas'
  gate_check maas-api-endpoints 'MaaS API Service endpoints ready' "$EVIDENCE_DIR/maas-api-endpoints.json" \
    'length>0 and any(.[];.ready>0)'
  gate_check maas-database-logs 'Sanitized MaaS logs have no database/configuration failure' "$EVIDENCE_DIR/maas-api-log-gate.json" \
    '.logRead==true and .missingDatabaseSecretMessagePresent==false and .databaseConnectionFailurePresent==false and .databaseConfigurationFailurePresent==false'
  gate_check postgres-ownership 'PostgreSQL Deployment/Service task-owned and digest-pinned' "$EVIDENCE_DIR/postgres-ownership.json" \
    'length==2 and all(.[];.owner=="cold-grid-llmd-e2e") and any(.[];.kind=="Deployment" and .ready>0 and .available>0 and (.image|test("@sha256:")))'
  gate_check postgres-endpoints 'PostgreSQL Service endpoints ready' "$EVIDENCE_DIR/postgres-endpoints.json" \
    'length>0 and any(.[];.ready>0)'
  gate_check postgres-secrets 'maas-db-config key and run-owned Secret metadata' "$EVIDENCE_DIR/postgres-secret-metadata.json" \
    'length==2 and all(.[];.owner=="cold-grid-llmd-e2e" and (.dataKeys|length>0)) and any(.[];.name=="maas-db-config" and (.dataKeys|index("DB_CONNECTION_URL")!=null))'
  gate_check authorino 'Authorino Ready' "$EVIDENCE_DIR/authorino-readiness.json" \
    'any(.[];.type=="Ready" and .status=="True")'
  gate_check authorino-tls-secret 'Authorino TLS Secret has expected key names' "$EVIDENCE_DIR/authorino-secret-metadata.json" \
    '.dataKeys|index("tls.crt")!=null and index("tls.key")!=null'
  gate_check authorino-pods 'Authorino Pods Ready' "$EVIDENCE_DIR/authorino-pods.json" \
    'length>0 and all(.[];.ready==true)'
  gate_check authorino-endpoints 'Authorino TLS Service endpoint ready' "$EVIDENCE_DIR/authorino-endpoints.json" \
    'length>0 and any(.[];.ready>0)'
  gate_check maas-gateway 'MaaS Gateway Accepted and Programmed' "$EVIDENCE_DIR/maas-gateway-readiness.json" \
    'any(.[];.type=="Accepted" and .status=="True") and any(.[];.type=="Programmed" and .status=="True")'
  gate_check serving-webhooks 'KServe/LLMInferenceService/model-controller webhook endpoints ready' "$EVIDENCE_DIR/webhook-endpoints.json" \
    'length==3 and ([.[].service]|sort)==["kserve-webhook-server-service","llmisvc-webhook-server-service","odh-model-controller-webhook-service"] and all(.[];.endpointSlices>0 and .ready>0 and .total>=.ready)'
  oc get pods -A --field-selector=status.phase=Pending -o json \
    | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,node:.spec.nodeName,reason:.status.reason,message:.status.message}]' \
    >"$EVIDENCE_DIR/post-reconciliation-pending-pods.json"
  gate_check no-pending-pods 'No Pods remain Pending after platform reconciliation' "$EVIDENCE_DIR/post-reconciliation-pending-pods.json" \
    'length==0'

  jq -s --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    --argjson gateCount "$gate_count" --argjson failedGates "$failed_gates" \
    '{observedAt:$observedAt,gateCount:$gateCount,failedGates:$failedGates,status:(if $failedGates==0 then "PASS" else "BLOCKED" end),gates:.}' \
    "$gate_file" >"$EVIDENCE_DIR/platform-gate-summary.json"
  rm -f "$gate_file"
  if [[ "$(jq -r '.syncStatus' "$EVIDENCE_DIR/argo-application-state.json")" != Synced ]]; then
    log "Argo Application is not fully Synced; health and last operation are recorded separately from live readiness"
  fi
  if ((failed_gates > 0)); then
    log "RHOAI/MaaS platform readiness BLOCKED: $failed_gates of $gate_count required gates failed; no inference traffic was sent"
    return 1
  fi
  log 'RHOAI, MaaS, Authorino, Gateway, webhooks, and required DSCI monitoring readiness passed; WideEP condition is not a gate'
}

write_resource_profile_record
verify_access
capture_inventory
case "$MODE" in
  preflight) capacity_gate ;;
  render-profile)
    validate_gitops_source
    render_platform_resource_profile
    validate_monitoringstack_server_dry_run
    ;;
  apply-platform)
    render_platform_resource_profile
    validate_monitoringstack_server_dry_run
    validate_gitops_source
    capacity_gate
    install_operator "$RHOAI_NAMESPACE" rhods-operator rhods-operator stable-3.5 "$RHOAI_CSV" rhods-operator
    install_operator "$GITOPS_OPERATOR_NAMESPACE" openshift-gitops-operator openshift-gitops-operator gitops-1.20 openshift-gitops-operator.v1.20.8 openshift-gitops-operator
    wait_for_gitops_instance
    apply_application
    bootstrap_authorino_tls
    configure_maas_database
    ;;
  reconcile-platform-config)
    apply_application
    bootstrap_authorino_tls
    ;;
  rollback-resource-profile)
    [[ "$RESOURCE_PROFILE" == strict ]] || { printf 'rollback-resource-profile requires RESOURCE_PROFILE=strict\n' >&2; exit 2; }
    validate_gitops_source
    apply_application
    apply_monitoringstack_resource_profile strict
    verify_monitoring_profile_survives_reconciliation
    log 'Reduced-dev profile rolled back to strict MaaS requests; shared platform resources were retained'
    ;;
  configure-maas-db)
    pending="$(oc get pods -A --field-selector=status.phase=Pending -o json | jq '.items|length')"
    log "Applying only the MaaS database prerequisite with $pending cluster Pods currently Pending; this does not authorize Grid deployment"
    configure_maas_database
    ;;
  verify-platform)
    if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then verify_monitoring_profile_survives_reconciliation; fi
    verify_platform
    ;;
  collect)
    if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then
      if ! verify_monitoring_resource_profile; then
        log 'Read-only reduced-dev MonitoringStack verification failed; no profile override was applied during collection'
      fi
    fi
    verify_platform || true
    log 'Read-only collection complete; no inference traffic was sent'
    ;;
  *) printf 'unsupported mode: %s\n' "$MODE" >&2; exit 2 ;;
esac
log "Mode $MODE completed"
printf 'Evidence: %s\n' "$EVIDENCE_DIR"
