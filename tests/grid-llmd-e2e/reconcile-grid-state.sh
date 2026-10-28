#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Reconcile task-owned Grid CR drift and residue from a failed Helm revision.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${GRID_STATE_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
GRID_RELEASE_NAME="${GRID_RELEASE_NAME:-grid-operator}"
GRID_RELEASE_NAMESPACE="${GRID_RELEASE_NAMESPACE:-praxis-grid-operator-system}"
GRID_DEPLOYMENT_NAME="${GRID_DEPLOYMENT_NAME:-grid-operator}"
GRID_NETWORK_NAME="${GRID_NETWORK_NAME:-grid-llmd-e2e}"
GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"
GRID_GATEWAY_NAME="${GRID_GATEWAY_NAME:-grid-gateway}"
GRID_RESOURCE_NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
GRID_RUN_LABEL="${GRID_RUN_LABEL:-$(date -u +%Y%m%dT%H%M%SZ)}"
GRID_OPERATOR_SOURCE_REVISION="${GRID_OPERATOR_SOURCE_REVISION:-}"
GRID_OPERATOR_CHART_NAME="${GRID_OPERATOR_CHART_NAME:-praxis-ai-grid-operator-chart}"
GRID_OPERATOR_CHART_VERSION="${GRID_OPERATOR_CHART_VERSION:-}"
GRID_OPERATOR_CHART_SHA256="${GRID_OPERATOR_CHART_SHA256:-}"
GRID_OPERATOR_IMAGE="${GRID_OPERATOR_IMAGE:-}"
GRID_WAIT_TIMEOUT="${GRID_WAIT_TIMEOUT:-180s}"
GRID_EVIDENCE_DIR="${GRID_EVIDENCE_DIR:-}"

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

helm() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command helm --kube-context="$KUBE_CONTEXT" "$@"
}

while (($#)); do
  case "$1" in
    --mode) MODE="$2"; shift 2 ;;
    --evidence-dir) GRID_EVIDENCE_DIR="$2"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="$2"; shift 2 ;;
    *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
  esac
done

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Grid state reconciliation plan (no cluster calls):
  1. Verify the current oc identity, server, /readyz, and explicit API TLS mode.
  2. Run the existing read-only RHOAI/MaaS platform gate.
  3. Read only GridNetwork/GridSite documents from the active Helm release manifest.
  4. Verify the ODH operator Deployment image/digest and Grid CRDs.
  5. Correct GridNetwork consumerGateways[0].siteName only when the rest of its spec matches release intent.
  6. Delete only run-labelled, Helm-owned GridSites absent from the active release and unreferenced by every GridNetwork.
  7. Verify one desired GridSite and Distributed routingMapStatus with matching revisions.

Modes: plan, reconcile, verify, collect.
Required environment for reconcile/verify:
  GRID_OPERATOR_SOURCE_REVISION, GRID_OPERATOR_CHART_VERSION, GRID_OPERATOR_CHART_SHA256, GRID_OPERATOR_IMAGE.
When reconciling an existing release, pass its recorded GRID_RUN_LABEL; the generated UTC default is for a fresh run identity.
API TLS bypass requires CLUSTER_ENV=ephemeral-dev and ALLOW_INSECURE_DEV_API=true.
PLAN
  printf 'Fresh-run label default: %s\n' "$GRID_RUN_LABEL"
  printf 'For an existing release, pass its recorded GRID_RUN_LABEL explicitly.\n'
  exit 0
fi

for bin in oc helm jq awk; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
type -P helm >/dev/null || { printf 'helm executable is required\n' >&2; exit 2; }
[[ "$MODE" == reconcile || "$MODE" == verify || "$MODE" == collect ]] || {
  printf 'unsupported mode: %s\n' "$MODE" >&2
  exit 2
}
[[ -n "$GRID_OPERATOR_SOURCE_REVISION" && -n "$GRID_OPERATOR_CHART_VERSION" && -n "$GRID_OPERATOR_CHART_SHA256" && -n "$GRID_OPERATOR_IMAGE" ]] || {
  printf 'source revision, chart version, chart SHA-256, and immutable operator image are required\n' >&2
  exit 2
}
[[ "$GRID_OPERATOR_CHART_SHA256" =~ ^[0-9a-fA-F]{64}$ ]] || {
  printf 'GRID_OPERATOR_CHART_SHA256 must contain exactly 64 hexadecimal characters\n' >&2
  exit 2
}
[[ "$GRID_OPERATOR_IMAGE" == *@sha256:* ]] || {
  printf 'GRID_OPERATOR_IMAGE must be pinned by sha256 digest\n' >&2
  exit 2
}

if [[ -z "$GRID_EVIDENCE_DIR" ]]; then
  GRID_EVIDENCE_DIR="$REPO_ROOT/evidence/grid-state-$(date -u +%Y%m%dT%H%M%SZ)"
fi
[[ ! -e "$GRID_EVIDENCE_DIR" ]] || {
  printf 'evidence path already exists; choose a new UTC timestamp\n' >&2
  exit 2
}
mkdir -p "$GRID_EVIDENCE_DIR"
chmod 0700 "$GRID_EVIDENCE_DIR"
TEMP_DIR="$(mktemp -d)"
chmod 0700 "$TEMP_DIR"
trap 'rm -rf "$TEMP_DIR"' EXIT
on_error() {
  local exit_code="$1"
  if ((BASH_SUBSHELL > 0)); then return "$exit_code"; fi
  trap - ERR
  log "Grid state mode $MODE stopped with exit=$exit_code; see this evidence directory for the first failing gate"
  jq -n --arg mode "$MODE" --arg status blocked --argjson exitCode "$exit_code" \
    --arg evidenceDirectory "$GRID_EVIDENCE_DIR" \
    '{mode:$mode,status:$status,exitCode:$exitCode,evidenceDirectory:$evidenceDirectory,inferenceTrafficSent:false}' \
    >"$GRID_EVIDENCE_DIR/RESULT.json" 2>/dev/null || true
  exit "$exit_code"
}
trap 'on_error $?' ERR

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  printf '%s\n' "$line" | tee -a "$GRID_EVIDENCE_DIR/RUN-LOG.md" "$REPO_ROOT/RUN-LOG.md"
}

api_access() {
  local context user server ready insecure api_tls_mode
  context="$KUBE_CONTEXT"
  user="$(oc whoami)"
  server="$(oc whoami --show-server)"
  ready="$(oc get --raw=/readyz)"
  [[ "$ready" == *ok* ]] || { log "API access blocked: /readyz did not return ok"; return 1; }
  if [[ -n "$EXPECTED_API_SERVER" && "$server" != "$EXPECTED_API_SERVER" ]]; then
    log "API access blocked: server mismatch (observed $server)"
    return 1
  fi
  insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
  if [[ "$insecure" == true ]]; then
    [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
      log 'API access blocked: kubeconfig bypasses TLS without explicit ephemeral-dev opt-in'
      return 1
    }
    api_tls_mode=explicit-ephemeral-dev-unverified
  else
    api_tls_mode=kubeconfig-verified
  fi
  {
    printf 'capturedAt=%s\nuser=%s\nserver=%s\ncontext=%s\nreadyz=%s\napiTlsMode=%s\n' \
      "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$user" "$server" "$context" "$ready" "$api_tls_mode"
  } >"$GRID_EVIDENCE_DIR/api-access-summary.txt"
  chmod 0600 "$GRID_EVIDENCE_DIR/api-access-summary.txt"
  log "API access verified for $server; TLS mode recorded as $api_tls_mode"
}

extract_release_doc() {
  local template="$1" out="$2"
  helm get manifest -n "$GRID_RELEASE_NAMESPACE" "$GRID_RELEASE_NAME" |
    awk -v template="$template" '
      $0 ~ "^# Source: .*/templates/" template "$" {capture=1}
      capture && /^---$/ {exit}
      capture {print}
    ' >"$out"
  [[ -s "$out" ]] || { log "active Helm release lacks expected template $template"; return 1; }
}

capture_state() {
  local manifest="$TEMP_DIR/release-manifest.yaml"
  helm status -n "$GRID_RELEASE_NAMESPACE" "$GRID_RELEASE_NAME" -o json |
    jq '{name:.name,namespace:.namespace,revision:.version,status:.info.status,chart:.chart,description:.info.description,updated:.info.last_deployed}' \
    >"$GRID_EVIDENCE_DIR/helm-status.json"
  [[ "$(jq -r '.status' "$GRID_EVIDENCE_DIR/helm-status.json")" == deployed ]] || {
    log 'Grid Helm release is not deployed'
    return 1
  }
  helm list -n "$GRID_RELEASE_NAMESPACE" -o json | jq -r --arg release "$GRID_RELEASE_NAME" \
    '.[]|select(.name==$release)|.chart' >"$GRID_EVIDENCE_DIR/helm-chart-version.txt"
  [[ "$(cat "$GRID_EVIDENCE_DIR/helm-chart-version.txt")" == "$GRID_OPERATOR_CHART_NAME-$GRID_OPERATOR_CHART_VERSION" ]] || {
    log "Grid Helm release chart identity does not match $GRID_OPERATOR_CHART_NAME-$GRID_OPERATOR_CHART_VERSION"
    return 1
  }
  helm get manifest -n "$GRID_RELEASE_NAMESPACE" "$GRID_RELEASE_NAME" >"$manifest"
  chmod 0600 "$manifest"
  awk '/^# Source: .*\/templates\/gridnetwork.yaml$/{capture=1} capture{print} capture && /^---$/{exit}' \
    "$manifest" >"$GRID_EVIDENCE_DIR/release-gridnetwork.yaml"
  awk '/^# Source: .*\/templates\/gridsite.yaml$/{capture=1} capture{print} capture && /^---$/{exit}' \
    "$manifest" >"$GRID_EVIDENCE_DIR/release-gridsite.yaml"
  [[ -s "$GRID_EVIDENCE_DIR/release-gridnetwork.yaml" && -s "$GRID_EVIDENCE_DIR/release-gridsite.yaml" ]] || {
    log 'Active release did not render exactly the expected GridNetwork/GridSite templates'
    return 1
  }
  oc create --dry-run=client -f "$GRID_EVIDENCE_DIR/release-gridnetwork.yaml" -o json >"$GRID_EVIDENCE_DIR/release-gridnetwork.json"
  oc create --dry-run=client -f "$GRID_EVIDENCE_DIR/release-gridsite.yaml" -o json >"$GRID_EVIDENCE_DIR/release-gridsite.json"
  jq -e --arg network "$GRID_NETWORK_NAME" --arg namespace "$GRID_RESOURCE_NAMESPACE" \
    --arg release "$GRID_RELEASE_NAME" --arg run "$GRID_RUN_LABEL" \
    '.kind=="GridNetwork" and .metadata.name==$network and
     .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e" and
     .metadata.labels["app.kubernetes.io/instance"]==$release and
     .metadata.labels["praxis.fast/qualification-run"]==$run and
     (.spec.consumerGateways|length)==1 and
     .spec.consumerGateways[0].namespace==$namespace' \
    "$GRID_EVIDENCE_DIR/release-gridnetwork.json" >/dev/null || {
      log 'GridNetwork in active release does not match the configured task identity'
      return 1
    }
  jq -e --arg site "$GRID_SITE_NAME" --arg network "$GRID_NETWORK_NAME" \
    --arg release "$GRID_RELEASE_NAME" --arg run "$GRID_RUN_LABEL" \
    '.kind=="GridSite" and .metadata.name==$site and .spec.gridNetworkRef==$network and
     .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e" and
     .metadata.labels["app.kubernetes.io/instance"]==$release and
     .metadata.labels["praxis.fast/qualification-run"]==$run' \
    "$GRID_EVIDENCE_DIR/release-gridsite.json" >/dev/null || {
      log 'GridSite in active release does not match the configured task identity'
      return 1
    }
  jq -r '[(.metadata.name),(.metadata.labels["helm.sh/chart"] // "unknown"),(.metadata.labels["app.kubernetes.io/version"] // "unknown")] | @tsv' \
    "$GRID_EVIDENCE_DIR/release-gridnetwork.json" >"$GRID_EVIDENCE_DIR/grid-resource-chart-identity.tsv"
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,generation:.metadata.generation,resourceVersion:.metadata.resourceVersion,labels:.metadata.labels,annotations:.metadata.annotations},spec:.spec,status:{phase:.status.phase,conditions:.status.conditions,routingMapStatus:.status.routingMapStatus,praxisConfigStatus:.status.praxisConfigStatus}}' \
    >"$GRID_EVIDENCE_DIR/live-gridnetwork.json"
  oc get gridsites -o json | jq '[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,generation:.metadata.generation,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences,finalizers:.metadata.finalizers,deletionTimestamp:.metadata.deletionTimestamp},spec:.spec,status:{phase:.status.phase,reason:.status.reason,message:.status.message,observedGeneration:.status.observedGeneration,lastTransitionTime:.status.lastTransitionTime,capabilities:.status.capabilities}}]' \
    >"$GRID_EVIDENCE_DIR/live-gridsites.json"
  oc get deployment "$GRID_DEPLOYMENT_NAME" -n "$GRID_RELEASE_NAMESPACE" -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation},spec:{replicas:.spec.replicas,containers:[.spec.template.spec.containers[]|{name,image,resources}]},status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
    >"$GRID_EVIDENCE_DIR/operator-deployment.json"
  oc get pods -n "$GRID_RELEASE_NAMESPACE" -o json | jq --arg deployment "$GRID_DEPLOYMENT_NAME" \
    '[.items[]|select(.metadata.labels["app.kubernetes.io/name"]=="praxis-ai-grid-operator-chart")|{name:.metadata.name,phase:.status.phase,conditions:.status.conditions,containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]' \
    >"$GRID_EVIDENCE_DIR/operator-pods.json"
  oc get crd -o json | jq '[.items[]|select(.metadata.name|test("^(agenttoolproviders|gridnetworks|gridsites|inferenceproviders|siteinvites)\\.grid\\.praxis\\.fast$"))|{name:.metadata.name,established:any(.status.conditions[]?;.type=="Established" and .status=="True")}]' \
    >"$GRID_EVIDENCE_DIR/grid-crd-readiness.json"
  oc get svc -A -o json | jq --arg namespace "$GRID_RESOURCE_NAMESPACE" --arg run "$GRID_RUN_LABEL" \
    '[.items[]|select(.metadata.namespace==$namespace and .metadata.name=="grid-gateway" and .metadata.labels["praxis.fast/qualification-run"]==$run)|{namespace:.metadata.namespace,name:.metadata.name,labels:.metadata.labels,ports:.spec.ports}]' \
    >"$GRID_EVIDENCE_DIR/grid-gateway-services.json"
  oc get pods -A --field-selector=status.phase=Pending -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,reason:.status.reason,message:.status.message}]' \
    >"$GRID_EVIDENCE_DIR/pending-pods.json"
  cat >"$GRID_EVIDENCE_DIR/source-lock.md" <<LOCK
# Grid state reconciliation source lock

- ODH operator source revision: $GRID_OPERATOR_SOURCE_REVISION
- ODH operator chart name: $GRID_OPERATOR_CHART_NAME
- ODH operator chart version: $GRID_OPERATOR_CHART_VERSION
- ODH operator chart SHA-256: $GRID_OPERATOR_CHART_SHA256
- Expected operator image: $GRID_OPERATOR_IMAGE
- Active Helm release: $GRID_RELEASE_NAMESPACE/$GRID_RELEASE_NAME
- Run label: $GRID_RUN_LABEL
- GridNetwork/GridSite desired names: $GRID_NETWORK_NAME / $GRID_SITE_NAME
LOCK
  find "$GRID_EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
}

verify_operator_and_crds() {
  local configured runtime configured_digest runtime_digest
  oc wait deployment/"$GRID_DEPLOYMENT_NAME" -n "$GRID_RELEASE_NAMESPACE" \
    --for=condition=Available --timeout="$GRID_WAIT_TIMEOUT" >/dev/null
  oc get deployment "$GRID_DEPLOYMENT_NAME" -n "$GRID_RELEASE_NAMESPACE" -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation},spec:{replicas:.spec.replicas,containers:[.spec.template.spec.containers[]|{name,image,resources}]},status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
    >"$GRID_EVIDENCE_DIR/operator-deployment.json"
  oc get pods -n "$GRID_RELEASE_NAMESPACE" -o json | jq \
    '[.items[]|select(.metadata.labels["app.kubernetes.io/name"]=="praxis-ai-grid-operator-chart")|{name:.metadata.name,phase:.status.phase,conditions:.status.conditions,containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]' \
    >"$GRID_EVIDENCE_DIR/operator-pods.json"
  configured="$(jq -r '.spec.containers[]|select(.name=="operator")|.image' "$GRID_EVIDENCE_DIR/operator-deployment.json")"
  runtime="$(jq -r '[.[]|.containers[]?|select(.name=="operator" and .ready)|.imageID][0] // ""' "$GRID_EVIDENCE_DIR/operator-pods.json")"
  configured_digest="${configured##*@}"
  runtime_digest="${runtime##*@}"
  [[ "$configured" == "$GRID_OPERATOR_IMAGE" && "$runtime_digest" == "${GRID_OPERATOR_IMAGE##*@}" ]] || {
    log "operator image gate failed: configured/runtime digest do not match the expected immutable image"
    return 1
  }
  jq -e 'length>=5 and all(.[];.established==true)' "$GRID_EVIDENCE_DIR/grid-crd-readiness.json" >/dev/null || {
    log 'required Grid CRDs are not all Established'
    return 1
  }
  log "ODH operator ready with configured/runtime digest $configured_digest"
}

reconcile_network_site() {
  local desired live expected_site current_site uid generation patch
  desired="$GRID_EVIDENCE_DIR/release-gridnetwork.json"
  live="$GRID_EVIDENCE_DIR/live-gridnetwork.json"
  jq -e --arg release "$GRID_RELEASE_NAME" --arg release_ns "$GRID_RELEASE_NAMESPACE" \
    --arg run "$GRID_RUN_LABEL" --arg network "$GRID_NETWORK_NAME" \
    '.metadata.labels["app.kubernetes.io/managed-by"]=="Helm" and
     .metadata.labels["app.kubernetes.io/instance"]==$release and
     .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e" and
     .metadata.labels["praxis.fast/qualification-run"]==$run and
     .metadata.annotations["meta.helm.sh/release-name"]==$release and
     .metadata.annotations["meta.helm.sh/release-namespace"]==$release_ns and
     .metadata.name==$network' "$live" >/dev/null || {
      log 'live GridNetwork is not positively owned by the configured task release/run; refusing mutation'
      return 1
    }
  jq -e --argjson desired "$(cat "$desired")" '
    (.spec.consumerGateways|length)==1 and ($desired.spec.consumerGateways|length)==1 and
    (.spec.gridId==$desired.spec.gridId) and (.spec.tls==$desired.spec.tls) and
    ((.spec.seeds // [])==($desired.spec.seeds // [])) and
    ((.spec.swim // {gossipNodes:3,probeInterval:"5s",suspicionTimeout:"10s"}) ==
     ($desired.spec.swim // {gossipNodes:3,probeInterval:"5s",suspicionTimeout:"10s"})) and
    ((.spec.consumerGateways[0]|del(.siteName))==($desired.spec.consumerGateways[0]|del(.siteName)))
  ' "$live" >/dev/null || {
    log 'GridNetwork spec drift includes fields beyond the single expected task siteName correction; refusing mutation'
    return 1
  }
  expected_site="$(jq -r '.spec.consumerGateways[0].siteName' "$desired")"
  current_site="$(jq -r '.spec.consumerGateways[0].siteName' "$live")"
  if [[ "$current_site" != "$expected_site" ]]; then
    uid="$(jq -r '.metadata.uid' "$live")"
    generation="$(jq -r '.metadata.generation' "$live")"
    patch="$(jq -cn --arg uid "$uid" --argjson generation "$generation" \
      --arg gateway "$(jq -r '.spec.consumerGateways[0].name' "$desired")" \
      --arg namespace "$(jq -r '.spec.consumerGateways[0].namespace' "$desired")" \
      --arg old "$current_site" --arg new "$expected_site" \
      '[{op:"test",path:"/metadata/uid",value:$uid},
        {op:"test",path:"/metadata/generation",value:$generation},
        {op:"test",path:"/spec/consumerGateways/0/name",value:$gateway},
        {op:"test",path:"/spec/consumerGateways/0/namespace",value:$namespace},
        {op:"test",path:"/spec/consumerGateways/0/siteName",value:$old},
        {op:"replace",path:"/spec/consumerGateways/0/siteName",value:$new}]')"
    printf '%s\n' "$patch" >"$GRID_EVIDENCE_DIR/gridnetwork-site-name-patch.json"
    oc patch gridnetwork "$GRID_NETWORK_NAME" --type=json --patch "$patch" \
      >"$GRID_EVIDENCE_DIR/gridnetwork-site-name-patch-result.txt"
    log "reconciled only GridNetwork $GRID_NETWORK_NAME consumer siteName from $current_site to $expected_site"
  else
    log "GridNetwork $GRID_NETWORK_NAME consumer siteName already matches active release: $expected_site"
  fi
}

remove_failed_revision_orphans() {
  local active_site_names sites_json name owned refs finalizers
  active_site_names="$(jq -r '.metadata.name' "$GRID_EVIDENCE_DIR/release-gridsite.json")"
  [[ "$active_site_names" == "$GRID_SITE_NAME" ]] || {
    log 'active release declares a different GridSite set than this single-site reconciliation expects'
    return 1
  }
  sites_json="$(oc get gridsites -o json)"
  jq -e --arg run "$GRID_RUN_LABEL" --arg site "$GRID_SITE_NAME" \
    'all(.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.name!=$site);
      .metadata.labels["app.kubernetes.io/managed-by"]=="Helm" and
      .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e")' \
    <<<"$sites_json" >/dev/null || {
      log 'a non-current GridSite has the task run label but not the expected Helm/task ownership labels; refusing cleanup'
      return 1
    }
  while IFS= read -r name; do
    [[ -n "$name" ]] || continue
    owned="$(jq -r --arg name "$name" --arg release "$GRID_RELEASE_NAME" \
      --arg release_ns "$GRID_RELEASE_NAMESPACE" --arg run "$GRID_RUN_LABEL" \
      --arg network "$GRID_NETWORK_NAME" \
      '[.items[]|select(.metadata.name==$name)][0] |
       (.metadata.labels["app.kubernetes.io/instance"]==$release and
        .metadata.labels["app.kubernetes.io/part-of"]=="grid-llmd-e2e" and
        .metadata.labels["app.kubernetes.io/managed-by"]=="Helm" and
        .metadata.labels["praxis.fast/qualification-run"]==$run and
        .metadata.annotations["meta.helm.sh/release-name"]==$release and
        .metadata.annotations["meta.helm.sh/release-namespace"]==$release_ns and
        .spec.gridNetworkRef==$network)' <<<"$sites_json")"
    [[ "$owned" == true ]] || { log "stale GridSite $name failed exact release/run ownership check"; return 1; }
    finalizers="$(jq -r --arg name "$name" '[.items[]|select(.metadata.name==$name)][0].metadata.finalizers // [] | length' <<<"$sites_json")"
    [[ "$finalizers" == 0 ]] || { log "stale GridSite $name has finalizers; refusing automatic deletion"; return 1; }
    refs="$(oc get gridnetworks -o json | jq --arg site "$name" '[.items[]|select(any(.spec.consumerGateways[]?;.siteName==$site) or any(.spec.gatewayRefs[]?;.siteName==$site))|.metadata.name] | length')"
    [[ "$refs" == 0 ]] || { log "stale GridSite $name is still referenced by a GridNetwork; refusing deletion"; return 1; }
    printf 'name=%s\nownership=task-run-and-helm-release\nactiveReleaseDeclares=false\nnetworkReferences=0\n' "$name" \
      >>"$GRID_EVIDENCE_DIR/deleted-grid-sites.txt"
    oc delete gridsite "$name" --wait=true --timeout="$GRID_WAIT_TIMEOUT" \
      >>"$GRID_EVIDENCE_DIR/deleted-grid-sites.txt"
    log "removed obsolete task-owned GridSite $name left by a failed prior Helm revision"
    sites_json="$(oc get gridsites -o json)"
  done < <(jq -r --arg run "$GRID_RUN_LABEL" --arg site "$GRID_SITE_NAME" \
    '.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.name!=$site)|.metadata.name' <<<"$sites_json")
  jq '[.items[]|{name:.metadata.name,labels:.metadata.labels,annotations:.metadata.annotations,spec:.spec,status:{phase:.status.phase,reason:.status.reason,message:.status.message,observedGeneration:.status.observedGeneration,lastTransitionTime:.status.lastTransitionTime,capabilities:.status.capabilities}}]' \
    <<<"$(oc get gridsites -o json)" >"$GRID_EVIDENCE_DIR/gridsites-after.json"
}

verify_grid_state() {
  local timeout_seconds start status service_count site_phase site_reason
  timeout_seconds="${GRID_WAIT_TIMEOUT%s}"
  start="$(date +%s)"
  while true; do
    status="$(oc get gridnetwork "$GRID_NETWORK_NAME" -o json | jq -r --arg site "$GRID_SITE_NAME" '
      .metadata.generation as $generation |
      if (.spec.consumerGateways|length)==1 and .spec.consumerGateways[0].siteName==$site and
         .status.phase=="Active" and
         any(.status.routingMapStatus[]?;.phase=="Distributed" and .renderedRevision==.distributedRevision and .observedGeneration==$generation)
      then "ready" else "pending" end' 2>/dev/null || true)"
    [[ "$status" == ready ]] && break
    (( $(date +%s) - start < timeout_seconds )) || {
      log "GridNetwork did not reach current-generation Distributed status within ${GRID_WAIT_TIMEOUT}"
      return 1
    }
    sleep 5
  done
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json | jq '{name:.metadata.name,generation:.metadata.generation,phase:.status.phase,consumerGateways:.spec.consumerGateways,routingMapStatus:.status.routingMapStatus}' \
    >"$GRID_EVIDENCE_DIR/gridnetwork-after.json"
  oc get gridsites -o json | jq '[.items[]|{name:.metadata.name,labels:.metadata.labels,annotations:.metadata.annotations,spec:.spec,status:{phase:.status.phase,reason:.status.reason,message:.status.message,observedGeneration:.status.observedGeneration,lastTransitionTime:.status.lastTransitionTime,capabilities:.status.capabilities}}]' \
    >"$GRID_EVIDENCE_DIR/gridsites-after.json"
  jq -e --arg site "$GRID_SITE_NAME" --arg run "$GRID_RUN_LABEL" \
    '[.[]|select(.labels["praxis.fast/qualification-run"]==$run)] as $task | ($task|length)==1 and $task[0].name==$site' \
    "$GRID_EVIDENCE_DIR/gridsites-after.json" >/dev/null || {
    log 'task run does not have exactly one GridSite matching the active release'
    return 1
  }
  site_phase="$(jq -r --arg site "$GRID_SITE_NAME" --arg run "$GRID_RUN_LABEL" \
    '[.[]|select(.labels["praxis.fast/qualification-run"]==$run and .name==$site)][0].status.phase // "unknown"' \
    "$GRID_EVIDENCE_DIR/gridsites-after.json")"
  site_reason="$(jq -r --arg site "$GRID_SITE_NAME" --arg run "$GRID_RUN_LABEL" \
    '[.[]|select(.labels["praxis.fast/qualification-run"]==$run and .name==$site)][0].status.reason // "unknown"' \
    "$GRID_EVIDENCE_DIR/gridsites-after.json")"
  service_count="$(jq 'length' "$GRID_EVIDENCE_DIR/grid-gateway-services.json")"
  jq -n --arg network "$GRID_NETWORK_NAME" --arg site "$GRID_SITE_NAME" \
    --arg sitePhase "$site_phase" --arg siteReason "$site_reason" \
    --argjson gatewayServicePresent "$([[ "$service_count" -gt 0 ]] && echo true || echo false)" \
    '{gridNetwork:$network,gridSite:$site,gridSiteStatus:{phase:$sitePhase,reason:$siteReason},routingMap:"Distributed",gatewayServicePresent:$gatewayServicePresent,inferenceTrafficSent:false,qualification:"Grid bootstrap only; Gateway readiness not implied"}' \
    >"$GRID_EVIDENCE_DIR/RESULT.json"
  log "Grid bootstrap state verified: one task GridSite ($site_phase/$site_reason) and current Distributed routingMapStatus; no inference traffic sent"
  if [[ "$service_count" == 0 ]]; then
    log 'task Grid Gateway Service is still absent; GridSite address discovery and inference qualification remain blocked on Gateway deployment'
  fi
}

api_access
invocation_server="$(oc whoami --show-server)"
invocation_user="$(oc whoami)"
invocation_context="$KUBE_CONTEXT"
{
  printf '# Grid state reconciliation invocation\n\n'
  printf -- '- Mode: %s\n' "$MODE"
  printf -- '- Cluster API: %s\n' "$invocation_server"
  printf -- '- User/context: %s / %s\n' "$invocation_user" "$invocation_context"
  printf -- '- API TLS mode: see api-access-summary.txt\n'
  printf -- '- ODH source revision: %s\n' "$GRID_OPERATOR_SOURCE_REVISION"
  printf -- '- ODH chart: %s (SHA-256 %s)\n' "$GRID_OPERATOR_CHART_VERSION" "$GRID_OPERATOR_CHART_SHA256"
  printf -- '- Expected runtime image: %s\n' "$GRID_OPERATOR_IMAGE"
  printf -- '- Task GridNetwork/GridSite: %s / %s\n' "$GRID_NETWORK_NAME" "$GRID_SITE_NAME"
  printf -- '- Task run label: %s\n\n' "$GRID_RUN_LABEL"
  printf 'Sanitized replay command:\n\n'
  # Emit a literal trailing backslash followed by a newline. Keep the
  # backslash in a %s argument so printf does not serialize it as literal \\n.
  printf '%s\n' \
    "CLUSTER_ENV=ephemeral-dev ALLOW_INSECURE_DEV_API=true \\" \
    "GRID_OPERATOR_SOURCE_REVISION='$GRID_OPERATOR_SOURCE_REVISION' \\" \
    "GRID_OPERATOR_CHART_NAME='$GRID_OPERATOR_CHART_NAME' \\" \
    "GRID_OPERATOR_CHART_VERSION='$GRID_OPERATOR_CHART_VERSION' \\" \
    "GRID_OPERATOR_CHART_SHA256='$GRID_OPERATOR_CHART_SHA256' \\" \
    "GRID_OPERATOR_IMAGE='$GRID_OPERATOR_IMAGE' \\" \
    "GRID_RUN_LABEL='$GRID_RUN_LABEL' GRID_NETWORK_NAME='$GRID_NETWORK_NAME' GRID_SITE_NAME='$GRID_SITE_NAME' \\" \
    "GRID_RESOURCE_NAMESPACE='$GRID_RESOURCE_NAMESPACE' \\" \
    "'$SCRIPT_DIR/reconcile-grid-state.sh' --mode $MODE --expected-api-server '$EXPECTED_API_SERVER'" \
    ''
  printf 'No credentials, Secret values, certificate data, or kubeconfig data are included.\n'
} >"$GRID_EVIDENCE_DIR/INVOCATION.md"
chmod 0600 "$GRID_EVIDENCE_DIR/INVOCATION.md"
if [[ "$MODE" == reconcile ]]; then
  PLATFORM_EVIDENCE_DIR="$GRID_EVIDENCE_DIR/platform-gate"
  platform_args=(--mode verify-platform --evidence-dir "$PLATFORM_EVIDENCE_DIR")
  if [[ -n "$EXPECTED_API_SERVER" ]]; then platform_args+=(--expected-api-server "$EXPECTED_API_SERVER"); fi
  "$SCRIPT_DIR/cold-deploy.sh" "${platform_args[@]}"
fi
capture_state
if [[ "$MODE" != collect ]]; then verify_operator_and_crds; fi

case "$MODE" in
  reconcile)
    reconcile_network_site
    remove_failed_revision_orphans
    verify_grid_state
    ;;
  verify)
    # Read-only verification does not repair drift or remove prior-release objects.
    desired_site="$(jq -r '.spec.consumerGateways[0].siteName' "$GRID_EVIDENCE_DIR/release-gridnetwork.json")"
    live_site="$(jq -r '.spec.consumerGateways[0].siteName' "$GRID_EVIDENCE_DIR/live-gridnetwork.json")"
    [[ "$live_site" == "$desired_site" ]] || { log "GridNetwork drift: desired site=$desired_site live site=$live_site"; exit 1; }
    [[ "$(jq -r '.metadata.name' "$GRID_EVIDENCE_DIR/release-gridsite.json")" == "$GRID_SITE_NAME" ]] || { log 'active release GridSite name mismatch'; exit 1; }
    remove_check="$(oc get gridsites -o json | jq --arg run "$GRID_RUN_LABEL" --arg site "$GRID_SITE_NAME" '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.name!=$site)]|length')"
    [[ "$remove_check" == 0 ]] || { log "stale task GridSites remain: $remove_check"; exit 1; }
    verify_grid_state
    ;;
  collect)
    log 'Read-only Grid state collection complete; no resources were changed and no inference traffic was sent'
    ;;
esac

log "Grid state mode $MODE completed"
printf 'Evidence: %s\n' "$GRID_EVIDENCE_DIR"
