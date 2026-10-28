#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Deploy the task-owned Praxis Grid Gateway Service and Gateway API route.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${GRID_GATEWAY_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
GRID_NETWORK_NAME="${GRID_NETWORK_NAME:-grid-llmd-e2e}"
GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"
RELEASE_NAME="${GRID_GATEWAY_RELEASE_NAME:-grid-gateway}"
DEPLOYMENT_NAME="${GRID_GATEWAY_DEPLOYMENT_NAME:-grid-gateway}"
SERVICE_NAME="${GRID_GATEWAY_SERVICE_NAME:-grid-gateway}"
SERVICE_PORT="${GRID_GATEWAY_SERVICE_PORT:-8080}"
GATEWAY_NAME="${GRID_API_GATEWAY_NAME:-grid-llmd-ingress}"
GATEWAY_CLASS="${GRID_API_GATEWAY_CLASS:-data-science-gateway-class}"
HTTPROUTE_NAME="${GRID_HTTPROUTE_NAME:-grid-llmd-inference}"
HOSTNAME="${GRID_GATEWAY_HOSTNAME:-${GRID_SITE_NAME}.grid.internal}"
TLS_SECRET_NAME="${GRID_GATEWAY_TLS_SECRET_NAME:-grid-site-identity}"
SOURCE_SECRET_NAMESPACE="${GRID_IDENTITY_NAMESPACE:-praxis-grid-operator-system}"
SOURCE_IDENTITY_SECRET="${GRID_IDENTITY_SECRET_NAME:-grid-site-identity}"
SOURCE_CA_SECRET="${GRID_CA_SECRET_NAME:-grid-ca}"
CHART_FILE="${PRAXIS_GATEWAY_CHART_FILE:-}"
CHART_SHA256="${PRAXIS_GATEWAY_CHART_SHA256:-}"
CHART_VERSION="${PRAXIS_GATEWAY_CHART_VERSION:-0.2.2}"
SOURCE_REVISION="${PRAXIS_GRID_SOURCE_REVISION:-}"
IMAGE="${PRAXIS_GATEWAY_IMAGE:-}"
PRAXIS_CONFIG_FILE="${GRID_GATEWAY_PRAXIS_CONFIG_FILE:-}"
PRAXIS_CONFIG_MAP_NAME="${GRID_GATEWAY_PRAXIS_CONFIG_MAP_NAME:-${SERVICE_NAME}-config}"
PRAXIS_CONFIG_MAP_EXTERNAL="${GRID_GATEWAY_PRAXIS_CONFIG_MAP_EXTERNAL:-false}"
EXPECTED_ENDPOINTS_FILE="${GRID_GATEWAY_EXPECTED_ENDPOINTS_FILE:-}"
OVERLAY_CONFIGMAP="${GRID_GATEWAY_OVERLAY_CONFIGMAP:-}"
ALLOW_RETAINED_OVERLAY="${GRID_GATEWAY_ALLOW_RETAINED_OVERLAY:-false}"
CONFIG_SOURCE="${GRID_GATEWAY_PRAXIS_CONFIG_SOURCE:-byo}"
BOOTSTRAP_MODE="${GRID_GATEWAY_BOOTSTRAP_MODE:-false}"
OPERATOR_CONFIG_MAP_NAME="${GRID_GATEWAY_OPERATOR_CONFIGMAP_NAME:-grid-llmd-praxis-config}"
MOUNT_RECONCILIATION_ENABLED="${GRID_GATEWAY_MOUNT_RECONCILIATION_ENABLED:-false}"
RELEASE_HELM_MOUNTS="${GRID_GATEWAY_RELEASE_HELM_MOUNTS:-$MOUNT_RECONCILIATION_ENABLED}"
GRID_SERVING_ENABLED="${GRID_GATEWAY_GRID_SERVING_ENABLED:-false}"
GRID_OPERATOR_SERVICE_ACCOUNT="${GRID_OPERATOR_SERVICE_ACCOUNT:-grid-operator}"
GRID_OPERATOR_NAMESPACE="${GRID_OPERATOR_NAMESPACE:-praxis-grid-operator-system}"
BOOTSTRAP_CONFIG_FILE="${GRID_GATEWAY_OPERATOR_BOOTSTRAP_CONFIG_FILE:-$SCRIPT_DIR/operator-bootstrap-praxis.yaml}"
CPU_REQUEST="${GRID_GATEWAY_CPU_REQUEST:-25m}"
MEMORY_REQUEST="${GRID_GATEWAY_MEMORY_REQUEST:-128Mi}"
CPU_LIMIT="${GRID_GATEWAY_CPU_LIMIT:-500m}"
MEMORY_LIMIT="${GRID_GATEWAY_MEMORY_LIMIT:-512Mi}"
TIMEOUT="${GRID_GATEWAY_TIMEOUT:-8m}"
EVIDENCE_DIR="${GRID_GATEWAY_EVIDENCE_DIR:-}"
tmpdir=""

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

helm() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command helm --kube-context="$KUBE_CONTEXT" "$@"
}

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Task-owned Gateway path plan (plan mode makes no cluster calls):
  1. Verify the immutable main-source chart archive and digest-pinned Gateway image.
  2. Verify the existing task namespace, GridNetwork secret references, GatewayClass, and ownership of any existing objects.
  3. Verify the operator-generated site certificate chains to the task Grid CA and matches the configured Gateway hostname; record metadata only.
  4. Render the Praxis Gateway Service/Deployment plus a task-owned HTTPS Gateway and HTTPRoute.
  5. Copy only the task-owned site identity and public CA certificate into the task namespace; never log Secret values.
  6. Install the chart, then apply the HTTPS Gateway and route to the task Grid Gateway Service.
  7. Verify Deployment, Service endpoints, Gateway Accepted/Programmed, and current HTTPRoute Accepted/ResolvedRefs.

The bootstrap responder is a temporary 503 configuration. The downstream Grid
operator owns generated Praxis configuration. The task-owned Gateway chart can
mount that Grid-generated ConfigMap as an external config source and mount the
current routing overlay directly. This phase creates no LLMInferenceService,
simulator, ExtProc filter, or inference traffic.
Modes: plan, render, install, verify, collect. Render performs API reads and server-side dry-runs only. API TLS bypass requires the explicit ephemeral-dev opt-in.
PLAN
  exit 0
fi

for bin in oc helm jq sha256sum awk openssl base64; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
type -P helm >/dev/null || { printf 'helm executable is required\n' >&2; exit 2; }
[[ "$MODE" == render || "$MODE" == install || "$MODE" == verify || "$MODE" == collect ]] || {
  printf 'unsupported GRID_GATEWAY_MODE: %s\n' "$MODE" >&2
  exit 2
}
[[ -n "$EXPECTED_API_SERVER" && -n "$RUN_LABEL" && -n "$SOURCE_REVISION" ]] || {
  printf 'EXPECTED_API_SERVER, GRID_RUN_LABEL, and PRAXIS_GRID_SOURCE_REVISION are required\n' >&2
  exit 2
}
[[ "$IMAGE" =~ ^(.+)@(sha256:[0-9a-fA-F]{64})$ ]] || {
  printf 'PRAXIS_GATEWAY_IMAGE must be pinned as repository@sha256:<64 hex>\n' >&2
  exit 2
}
IMAGE_DIGEST="${BASH_REMATCH[2]}"
[[ -f "$CHART_FILE" && "$CHART_SHA256" =~ ^[0-9a-fA-F]{64}$ ]] || {
  printf 'provide an existing Praxis Gateway chart archive and 64-character SHA-256\n' >&2
  exit 2
}
case "$CONFIG_SOURCE" in
  byo|operator) ;;
  *) printf 'GRID_GATEWAY_PRAXIS_CONFIG_SOURCE must be byo or operator\n' >&2; exit 2 ;;
esac
[[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true || "$PRAXIS_CONFIG_MAP_EXTERNAL" == false ]] || {
  printf 'GRID_GATEWAY_PRAXIS_CONFIG_MAP_EXTERNAL must be true or false\n' >&2
  exit 2
}
[[ "$MOUNT_RECONCILIATION_ENABLED" == true || "$MOUNT_RECONCILIATION_ENABLED" == false ]] || {
  printf 'GRID_GATEWAY_MOUNT_RECONCILIATION_ENABLED must be true or false\n' >&2
  exit 2
}
[[ "$RELEASE_HELM_MOUNTS" == true || "$RELEASE_HELM_MOUNTS" == false ]] || {
  printf 'GRID_GATEWAY_RELEASE_HELM_MOUNTS must be true or false\n' >&2
  exit 2
}
if [[ "$RELEASE_HELM_MOUNTS" == true && "$MOUNT_RECONCILIATION_ENABLED" != true ]]; then
  printf 'GRID_GATEWAY_RELEASE_HELM_MOUNTS=true requires mount reconciliation\n' >&2
  exit 2
fi
[[ "$GRID_SERVING_ENABLED" == true || "$GRID_SERVING_ENABLED" == false ]] || {
  printf 'GRID_GATEWAY_GRID_SERVING_ENABLED must be true or false\n' >&2
  exit 2
}
[[ "$ALLOW_RETAINED_OVERLAY" == true || "$ALLOW_RETAINED_OVERLAY" == false ]] || {
  printf 'GRID_GATEWAY_ALLOW_RETAINED_OVERLAY must be true or false\n' >&2
  exit 2
}
[[ "$BOOTSTRAP_MODE" == true || "$BOOTSTRAP_MODE" == false ]] || {
  printf 'GRID_GATEWAY_BOOTSTRAP_MODE must be true or false\n' >&2
  exit 2
}
if [[ "$BOOTSTRAP_MODE" == true ]]; then
  [[ "$CONFIG_SOURCE" == byo && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false && -n "$PRAXIS_CONFIG_FILE" && -f "$PRAXIS_CONFIG_FILE" && -n "$OVERLAY_CONFIGMAP" && "$GRID_SERVING_ENABLED" == false ]] || {
    printf 'bootstrap mode requires a local BYO config, task Grid overlay, and disabled Grid serving\n' >&2
    exit 2
  }
  if ! grep -Fq 'status: 503' "$PRAXIS_CONFIG_FILE" || ! grep -Fq 'the gateway is starting' "$PRAXIS_CONFIG_FILE"; then
    printf 'bootstrap mode accepts only the checked 503 startup responder config\n' >&2
    exit 2
  fi
fi
if [[ "$CONFIG_SOURCE" == operator ]]; then
  [[ "$MOUNT_RECONCILIATION_ENABLED" == true ]] || {
    printf 'operator Praxis source requires the explicit Grid mount-reconciliation handoff\n' >&2
    exit 2
  }
  [[ -z "$PRAXIS_CONFIG_FILE" && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false && -z "$OVERLAY_CONFIGMAP" ]] || {
    printf 'operator Praxis source owns the generated config and overlay mount; do not set BYO config or overlay inputs\n' >&2
    exit 2
  }
  [[ -n "$OPERATOR_CONFIG_MAP_NAME" && -n "$GRID_OPERATOR_SERVICE_ACCOUNT" && -n "$GRID_OPERATOR_NAMESPACE" && -f "$BOOTSTRAP_CONFIG_FILE" ]] || {
    printf 'operator Praxis source requires a config map name, Grid operator identity, and checked bootstrap config file\n' >&2
    exit 2
  }
  if [[ "$GRID_SERVING_ENABLED" == true ]]; then
    [[ "$MOUNT_RECONCILIATION_ENABLED" == true ]] || {
      printf 'operator grid serving requires GRID_GATEWAY_MOUNT_RECONCILIATION_ENABLED=true\n' >&2
      exit 2
    }
    [[ -n "$GRID_NETWORK_NAME" && -n "$SERVICE_NAME" ]] || {
      printf 'operator grid serving requires the task GridNetwork and Gateway reference names\n' >&2
      exit 2
    }
  fi
  PRAXIS_CONFIG_MAP_NAME="$OPERATOR_CONFIG_MAP_NAME"
elif [[ -n "$PRAXIS_CONFIG_FILE" ]]; then
  [[ -f "$PRAXIS_CONFIG_FILE" && -n "$PRAXIS_CONFIG_MAP_NAME" && -n "$OVERLAY_CONFIGMAP" ]] || {
    printf 'GRID_GATEWAY_PRAXIS_CONFIG_FILE requires a config map name and GRID_GATEWAY_OVERLAY_CONFIGMAP\n' >&2
    exit 2
  }
  if [[ "$BOOTSTRAP_MODE" != true ]]; then
    grep -Fq '/etc/praxis/routing/routing-overlay.json' "$PRAXIS_CONFIG_FILE" || {
      printf 'task Praxis config must consume /etc/praxis/routing/routing-overlay.json\n' >&2
      exit 2
    }
  fi
  if grep -Eiq '(^|[[:space:]])(authorization|api[_-]?key|access[_-]?token|password|bearer[_-]?token)[[:space:]]*:' "$PRAXIS_CONFIG_FILE"; then
    printf 'Praxis config contains a credential-like field; use Secret references and keep values out of the config\n' >&2
    exit 2
  fi
  [[ "$CONFIG_SOURCE" == byo ]] || {
    printf 'GRID_GATEWAY_PRAXIS_CONFIG_FILE is only supported with BYO config\n' >&2
    exit 2
  }
elif [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then
  [[ "$CONFIG_SOURCE" == byo && -n "$PRAXIS_CONFIG_MAP_NAME" && -n "$OVERLAY_CONFIGMAP" &&
     -n "$EXPECTED_ENDPOINTS_FILE" && -f "$EXPECTED_ENDPOINTS_FILE" ]] || {
    printf 'external Grid-generated Praxis config requires BYO source, config and overlay map names, and GRID_GATEWAY_EXPECTED_ENDPOINTS_FILE\n' >&2
    exit 2
  }
elif [[ -n "$OVERLAY_CONFIGMAP" ]]; then
  printf 'GRID_GATEWAY_OVERLAY_CONFIGMAP requires GRID_GATEWAY_PRAXIS_CONFIG_FILE\n' >&2
  exit 2
fi
if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" != true && -n "$EXPECTED_ENDPOINTS_FILE" ]]; then
  printf 'GRID_GATEWAY_EXPECTED_ENDPOINTS_FILE is only used for a Grid-generated external Praxis ConfigMap\n' >&2
  exit 2
fi
if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true && ( "$CONFIG_SOURCE" != byo || -z "$PRAXIS_CONFIG_MAP_NAME" ||
  -z "$OVERLAY_CONFIGMAP" || -z "$EXPECTED_ENDPOINTS_FILE" || ! -f "$EXPECTED_ENDPOINTS_FILE" ) ]]; then
  printf 'external Grid-generated Praxis config requires BYO source, config and overlay map names, and a readable expected endpoint inventory\n' >&2
  exit 2
fi
[[ "$HOSTNAME" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] || {
  printf 'GRID_GATEWAY_HOSTNAME must be a DNS hostname\n' >&2
  exit 2
}
[[ "$SERVICE_PORT" =~ ^[0-9]+$ && "$SERVICE_PORT" -ge 1 && "$SERVICE_PORT" -le 65535 ]] || {
  printf 'GRID_GATEWAY_SERVICE_PORT must be between 1 and 65535\n' >&2
  exit 2
}
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="$REPO_ROOT/evidence/grid-gateway-$(date -u +%Y%m%dT%H%M%SZ)"
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

log() {
  local line
  line="$(date -u +%Y-%m-%dT%H:%M:%SZ) $*"
  printf '%s\n' "$line" | tee -a "$EVIDENCE_DIR/RUN-LOG.md" "$REPO_ROOT/RUN-LOG.md"
}

capture_failure() {
  local code="$1"
  trap - ERR EXIT
  if [[ -n "${tmpdir:-}" && -d "$tmpdir" ]]; then
    rm -rf "$tmpdir"
    tmpdir=""
  fi
  {
    printf '# Grid Gateway attempt bug capture\n\n'
    printf -- '- First failed phase: `%s`\n' "${CURRENT_PHASE:-initialization}"
    printf -- '- Failure detail: `%s`\n' "${CURRENT_FAILURE:-see bounded phase evidence}"
    printf -- '- Exit code: `%s`\n' "$code"
    printf -- '- Time: `%s`\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf -- '- Context: `%s`\n' "${context:-not-read}"
    printf -- '- Identity: `%s`\n' "${user:-not-read}"
    printf -- '- Expected API server: `%s`\n' "$EXPECTED_API_SERVER"
    printf -- '- Observed API server: `%s`\n' "${server:-not-read}"
    printf -- '- Grid source revision: `%s`\n' "$SOURCE_REVISION"
    printf -- '- Gateway image: `%s`\n' "$IMAGE"
    printf -- '- Namespace/resources: `%s` / Gateway `%s`, HTTPRoute `%s`, Service `%s`\n' "$NAMESPACE" "$GATEWAY_NAME" "$HTTPROUTE_NAME" "$SERVICE_NAME"
    printf -- '- Evidence directory: `%s`\n' "$EVIDENCE_DIR"
    printf '\nNo bearer token, Secret value, private key, certificate bytes, or inference request was captured/sent.\n'
  } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  jq -n --arg mode "$MODE" --arg status blocked --arg phase "${CURRENT_PHASE:-initialization}" \
    --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    --arg context "${context:-}" --arg identity "${user:-}" \
    --arg expectedServer "$EXPECTED_API_SERVER" --arg observedServer "${server:-}" \
    '{mode:$mode,status:$status,phase:$phase,exitCode:$exitCode,evidenceDirectory:$evidence,api:{context:$context,identity:$identity,expectedServer:$expectedServer,observedServer:$observedServer},inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json" 2>/dev/null || true
  log "Grid Gateway $MODE stopped with exit=$code at phase=${CURRENT_PHASE:-initialization}"
  exit "$code"
}
capture_exit() {
  local code="$1"
  if [[ -n "${tmpdir:-}" && -d "$tmpdir" ]]; then
    rm -rf "$tmpdir"
    tmpdir=""
  fi
  if ((code != 0)); then capture_failure "$code"; fi
}
trap 'capture_failure $?' ERR
trap 'capture_exit $?' EXIT

CURRENT_PHASE=artifact-validation
observed_chart_sha="$(sha256sum "$CHART_FILE" | awk '{print $1}')"
[[ "$observed_chart_sha" == "$CHART_SHA256" ]] || {
  CURRENT_FAILURE='Praxis Gateway chart archive checksum mismatch'
  printf 'chart SHA mismatch\n' >&2
  exit 1
}
chart_metadata="$(helm show chart "$CHART_FILE")"
grep -Fq "version: $CHART_VERSION" <<<"$chart_metadata" || {
  CURRENT_FAILURE='Praxis Gateway chart version mismatch'
  printf 'chart version mismatch\n' >&2
  exit 1
}

CURRENT_PHASE=api-access
context="$KUBE_CONTEXT"
user="$(oc whoami)"
server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || {
  CURRENT_FAILURE='current oc context points at a different API server'
  printf 'API server mismatch: expected=%s observed=%s context=%s\n' \
    "$EXPECTED_API_SERVER" "$server" "$context" >&2
  capture_failure 1
}
ready="$(oc get --raw=/readyz)"
[[ "$ready" == *ok* ]] || {
  CURRENT_FAILURE='API /readyz did not return ok'
  printf 'API /readyz did not return ok\n' >&2
  exit 1
}
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
if [[ "$insecure" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
    CURRENT_FAILURE='insecure kubeconfig was used without the explicit ephemeral-dev opt-in'
    printf 'insecure kubeconfig rejected without the explicit ephemeral-dev opt-in\n' >&2
    exit 1
  }
  API_TLS_MODE=explicit-ephemeral-dev-unverified
else
  API_TLS_MODE=kubeconfig-verified
fi

CURRENT_PHASE=ownership-preflight
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
  "$EVIDENCE_DIR/namespace.json" >/dev/null || {
  CURRENT_FAILURE='Grid resource namespace is not owned by this run'
  printf 'refusing to use a namespace not owned by this run\n' >&2
  exit 1
}
oc get gatewayclass "$GATEWAY_CLASS" -o json >"$EVIDENCE_DIR/gatewayclass.json"
jq -e 'any(.status.conditions[]?; .type=="Accepted" and .status=="True")' \
  "$EVIDENCE_DIR/gatewayclass.json" >/dev/null || {
  CURRENT_FAILURE='requested GatewayClass is not Accepted'
  printf 'GatewayClass is not Accepted\n' >&2
  exit 1
}
oc get gridnetwork "$GRID_NETWORK_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gridnetwork.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
  "$EVIDENCE_DIR/gridnetwork.json" >/dev/null || {
  CURRENT_FAILURE='GridNetwork is not owned by this run'
  printf 'GridNetwork ownership does not match this task run\n' >&2
  exit 1
}
jq -e --arg sourceNs "$SOURCE_SECRET_NAMESPACE" --arg sourceSite "$SOURCE_IDENTITY_SECRET" --arg sourceCa "$SOURCE_CA_SECRET" \
  --arg gatewayNs "$NAMESPACE" --arg gatewaySite "$TLS_SECRET_NAME" --arg gatewayCa "$SOURCE_CA_SECRET" \
  '((.spec.tls.siteSecretRef.namespace==$sourceNs and .spec.tls.siteSecretRef.name==$sourceSite and
     .spec.tls.caSecretRef.namespace==$sourceNs and .spec.tls.caSecretRef.name==$sourceCa) or
    (.spec.tls.siteSecretRef.namespace==$gatewayNs and .spec.tls.siteSecretRef.name==$gatewaySite and
     .spec.tls.caSecretRef.namespace==$gatewayNs and .spec.tls.caSecretRef.name==$gatewayCa))' \
  "$EVIDENCE_DIR/gridnetwork.json" >/dev/null || {
  CURRENT_FAILURE='GridNetwork does not reference the expected task-owned CA/site identity Secrets'
    printf 'GridNetwork TLS references match neither the exact source Secrets nor the run-owned Gateway-namespace copies\n' >&2
  exit 1
}
if [[ -n "$OVERLAY_CONFIGMAP" ]]; then
  CURRENT_PHASE=grid-overlay-ownership-preflight
  jq -er --arg gateway "$SERVICE_NAME" --arg namespace "$NAMESPACE" \
    --argjson allowRetained "$ALLOW_RETAINED_OVERLAY" '
    .metadata.generation as $generation |
    [.status.routingMapStatus[]? |
      select(.gatewayName==$gateway and .namespace==$namespace and
        (.phase=="Distributed" or ($allowRetained and .phase=="Retained" and .reason=="GatewayMountsNotReady")) and
        .observedGeneration==$generation and .renderedRevision==.distributedRevision and .distributedRevision!="" and
        ((.phase!="Retained") or (.candidateCount // 0)>0))]
    | if length==1 then .[0] else error("Grid overlay is not current and Distributed") end
  ' "$EVIDENCE_DIR/gridnetwork.json" >"$EVIDENCE_DIR/grid-overlay-status-before.json" || {
    CURRENT_FAILURE='GridNetwork has no current Distributed overlay or explicitly allowed retained overlay for the task Gateway'
    printf 'GridNetwork has no current Distributed overlay (or explicitly allowed retained overlay) for the task Gateway\n' >&2
    exit 1
  }
  observed_overlay_map="$(jq -r '.configMapName' "$EVIDENCE_DIR/grid-overlay-status-before.json")"
  observed_overlay_revision="$(jq -r '.distributedRevision' "$EVIDENCE_DIR/grid-overlay-status-before.json")"
  [[ "$observed_overlay_map" == "$OVERLAY_CONFIGMAP" ]] || {
    CURRENT_FAILURE='configured overlay ConfigMap does not match the current GridNetwork routing status'
    printf 'configured overlay ConfigMap does not match the current GridNetwork routing status\n' >&2
    exit 1
  }
  oc get configmap "$OVERLAY_CONFIGMAP" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/grid-overlay-configmap-before.json"
  jq -e --arg network "$GRID_NETWORK_NAME" --arg gateway "$SERVICE_NAME" --arg revision "$observed_overlay_revision" '
    .metadata.labels["grid.praxis.fast/network"]==$network and
    .metadata.labels["grid.praxis.fast/gateway"]==$gateway and
    .metadata.annotations["grid.praxis.fast/overlay-revision"]==$revision and
    ((.data["routing-overlay.json"] // "")|length)>0
  ' "$EVIDENCE_DIR/grid-overlay-configmap-before.json" >/dev/null || {
    CURRENT_FAILURE='Grid routing ConfigMap is not the current operator-produced overlay for this network and Gateway'
    printf 'Grid routing ConfigMap is not the current operator-produced overlay for this network and Gateway\n' >&2
    exit 1
  }
  jq --arg revision "$observed_overlay_revision" --argjson candidates "$(jq '.candidateCount' "$EVIDENCE_DIR/grid-overlay-status-before.json")" \
    --arg phase "$(jq -r '.phase' "$EVIDENCE_DIR/grid-overlay-status-before.json")" \
    '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,annotations:.metadata.annotations,dataKeys:(.data|keys),observedPhase:$phase,distributedRevision:$revision,candidateCount:$candidates}' \
    "$EVIDENCE_DIR/grid-overlay-configmap-before.json" >"$EVIDENCE_DIR/grid-overlay-configmap-summary.json"
fi
oc get namespace "$SOURCE_SECRET_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
  "$EVIDENCE_DIR/operator-namespace.json" >/dev/null || {
  CURRENT_FAILURE='Grid operator namespace ownership does not match this run'
  printf 'Grid operator namespace is not owned by this run\n' >&2
  exit 1
}

for kind_name in "gateway/$GATEWAY_NAME" "httproute/$HTTPROUTE_NAME"; do
  kind="${kind_name%%/*}"
  name="${kind_name#*/}"
  if oc get "$kind" "$name" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/existing-$kind-$name.json" 2>/dev/null; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
      "$EVIDENCE_DIR/existing-$kind-$name.json" >/dev/null || {
      CURRENT_FAILURE="$kind/$name exists without matching task-run ownership"
      printf '%s/%s exists without this run ownership label\n' "$kind" "$name" >&2
      exit 1
    }
  fi
done

if helm status "$RELEASE_NAME" -n "$NAMESPACE" >/dev/null 2>&1; then
  helm get manifest "$RELEASE_NAME" -n "$NAMESPACE" >"$EVIDENCE_DIR/helm-manifest-before.yaml"
  grep -Fq "praxis.fast/qualification-run: $RUN_LABEL" "$EVIDENCE_DIR/helm-manifest-before.yaml" || {
    CURRENT_FAILURE='Helm release exists without matching task-run ownership'
    printf 'Helm release exists without this run ownership label\n' >&2
    exit 1
  }
elif oc get deployment "$DEPLOYMENT_NAME" -n "$NAMESPACE" >/dev/null 2>&1 ||
     oc get service "$SERVICE_NAME" -n "$NAMESPACE" >/dev/null 2>&1; then
  CURRENT_FAILURE='Gateway Deployment or Service exists without a task-owned Helm release'
  printf 'Gateway Deployment or Service already exists outside the expected Helm release\n' >&2
  exit 1
fi

CURRENT_PHASE=render-review
praxis_config_map_name=""
if [[ "$CONFIG_SOURCE" == byo && ( -n "$PRAXIS_CONFIG_FILE" || "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ) ]]; then praxis_config_map_name="$PRAXIS_CONFIG_MAP_NAME"; fi
BYO_MOUNT_RECONCILIATION_VALUES=""
if [[ "$CONFIG_SOURCE" == byo && "$MOUNT_RECONCILIATION_ENABLED" == true ]]; then
  BYO_MOUNT_RECONCILIATION_VALUES="$(cat <<YAML
mountReconciliation:
  enabled: true
  network: $GRID_NETWORK_NAME
  gatewayRef: $SERVICE_NAME
  releaseHelmMounts: $RELEASE_HELM_MOUNTS
  rbac:
    create: true
  operator:
    serviceAccount: $GRID_OPERATOR_SERVICE_ACCOUNT
    namespace: $GRID_OPERATOR_NAMESPACE
YAML
)"
fi
if [[ "$CONFIG_SOURCE" == operator ]]; then
cat >"$EVIDENCE_DIR/gateway-values.yaml" <<YAML
fullnameOverride: $SERVICE_NAME
commonLabels:
  app.kubernetes.io/part-of: grid-llmd-e2e
  praxis.fast/qualification-run: "$RUN_LABEL"
image:
  repository: ${IMAGE%@*}
  tag: $CHART_VERSION
  digest: $IMAGE_DIGEST
  pullPolicy: Always
  flavor: grid-gateway
grid:
  networkName: $GRID_NETWORK_NAME
  siteName: $GRID_SITE_NAME
praxisConfig:
  source: operator
  operator:
    configMapName: "$OPERATOR_CONFIG_MAP_NAME"
    gridServing:
      enabled: $GRID_SERVING_ENABLED
      networkName: "$GRID_NETWORK_NAME"
      gatewayRefName: "$SERVICE_NAME"
mountReconciliation:
  enabled: true
  network: $GRID_NETWORK_NAME
  gatewayRef: $SERVICE_NAME
  releaseHelmMounts: $RELEASE_HELM_MOUNTS
  rbac:
    create: true
  operator:
    serviceAccount: $GRID_OPERATOR_SERVICE_ACCOUNT
    namespace: $GRID_OPERATOR_NAMESPACE
gridIdentity:
  tlsSecretName: $TLS_SECRET_NAME
  caSecretName: $SOURCE_CA_SECRET
service:
  enabled: true
  type: ClusterIP
  port: $SERVICE_PORT
resources:
  requests:
    cpu: $CPU_REQUEST
    memory: $MEMORY_REQUEST
  limits:
    cpu: $CPU_LIMIT
    memory: $MEMORY_LIMIT
YAML
if [[ "$GRID_SERVING_ENABLED" == true ]]; then
  cat >>"$EVIDENCE_DIR/gateway-values.yaml" <<'YAML'
health:
  readiness:
    exec:
      command:
        - /bin/sh
        - -c
        - "exec wget -q -T 2 --header 'Host: 127.0.0.1' -O /dev/null http://127.0.0.1:9901/healthy"
    initialDelaySeconds: 3
    periodSeconds: 5
    timeoutSeconds: 3
  liveness:
    exec:
      command:
        - /bin/sh
        - -c
        - "exec wget -q -T 2 --header 'Host: 127.0.0.1' -O /dev/null http://127.0.0.1:9901/healthy"
    initialDelaySeconds: 5
    periodSeconds: 10
    timeoutSeconds: 3
YAML
fi
else
cat >"$EVIDENCE_DIR/gateway-values.yaml" <<YAML
fullnameOverride: $SERVICE_NAME
commonLabels:
  app.kubernetes.io/part-of: grid-llmd-e2e
  praxis.fast/qualification-run: "$RUN_LABEL"
image:
  repository: ${IMAGE%@*}
  tag: $CHART_VERSION
  digest: $IMAGE_DIGEST
  pullPolicy: Always
  flavor: grid-gateway
grid:
  networkName: $GRID_NETWORK_NAME
  siteName: $GRID_SITE_NAME
praxisConfig:
  source: byo
  byo:
    configMapName: "$praxis_config_map_name"
    key: praxis.yaml
overlay:
  configMapName: "$OVERLAY_CONFIGMAP"
  sidecar:
    enabled: false
$BYO_MOUNT_RECONCILIATION_VALUES
gridIdentity:
  tlsSecretName: $TLS_SECRET_NAME
  caSecretName: $SOURCE_CA_SECRET
service:
  enabled: true
  type: ClusterIP
  port: $SERVICE_PORT
resources:
  requests:
    cpu: $CPU_REQUEST
    memory: $MEMORY_REQUEST
  limits:
    cpu: $CPU_LIMIT
    memory: $MEMORY_LIMIT
YAML
fi

helm template "$RELEASE_NAME" "$CHART_FILE" --namespace "$NAMESPACE" \
  --values "$EVIDENCE_DIR/gateway-values.yaml" \
  --api-versions security.openshift.io/v1 >"$EVIDENCE_DIR/rendered-gateway.yaml"
grep -Fq "image: $IMAGE" "$EVIDENCE_DIR/rendered-gateway.yaml" || {
  CURRENT_FAILURE='Rendered Gateway Deployment does not use the immutable image pin'
  printf 'rendered Gateway image does not match the digest pin\n' >&2
  exit 1
}
if [[ "$GRID_SERVING_ENABLED" == true ]] &&
   ! grep -Fq 'http://127.0.0.1:9901/healthy' "$EVIDENCE_DIR/rendered-gateway.yaml"; then
  CURRENT_FAILURE='gridServing Gateway readiness/liveness probes do not check Praxis /healthy'
  printf 'gridServing Gateway probes must check the loopback Praxis admin /healthy endpoint\n' >&2
  exit 1
fi
grep -Fq "name: $SERVICE_NAME" "$EVIDENCE_DIR/rendered-gateway.yaml"
grep -Fq "port: $SERVICE_PORT" "$EVIDENCE_DIR/rendered-gateway.yaml"
if [[ -n "$OVERLAY_CONFIGMAP" ]]; then
  grep -Fq "name: \"$OVERLAY_CONFIGMAP\"" "$EVIDENCE_DIR/rendered-gateway.yaml" || {
    CURRENT_FAILURE='Rendered Gateway Deployment does not mount the configured task Grid overlay ConfigMap'
    printf 'rendered Gateway does not mount the configured Grid overlay ConfigMap\n' >&2
    exit 1
  }
  grep -Fq 'mountPath: "/etc/praxis/routing"' "$EVIDENCE_DIR/rendered-gateway.yaml" || {
    CURRENT_FAILURE='Rendered Gateway Deployment does not mount the Grid overlay at the Praxis routing path'
    printf 'rendered Gateway does not mount the Grid overlay at /etc/praxis/routing\n' >&2
    exit 1
  }
fi
if grep -Eq '^(kind: Gateway|kind: HTTPRoute|kind: Secret)$' "$EVIDENCE_DIR/rendered-gateway.yaml"; then
  CURRENT_FAILURE='Praxis Gateway chart unexpectedly owns a Gateway API or Secret resource'
  printf 'Praxis Gateway chart render includes a separately owned Gateway API or Secret resource\n' >&2
  exit 1
fi

if [[ -n "$PRAXIS_CONFIG_FILE" || "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then
  if [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then
    CURRENT_PHASE=configmap-ownership-preflight
    external_configmap_json="$(oc get configmap "$PRAXIS_CONFIG_MAP_NAME" -n "$NAMESPACE" -o json)"
    jq -e --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" --arg gateway "$SERVICE_NAME" '
      (.metadata.labels["praxis.fast/qualification-run"]==$run) or
      (.metadata.labels["app.kubernetes.io/managed-by"]=="grid-operator" and
       .metadata.labels["grid.praxis.fast/network"]==$network and
       .metadata.labels["grid.praxis.fast/gateway"]==$gateway)
    ' <<<"$external_configmap_json" >/dev/null || {
      CURRENT_FAILURE='external Praxis ConfigMap is not owned by this run or the matching task GridNetwork'
      printf 'external Praxis ConfigMap ownership is ambiguous; refusing to adopt it\n' >&2
      exit 1
    }
    jq -j -er '.data["praxis.yaml"]' <<<"$external_configmap_json" |
      python3 "$SCRIPT_DIR/validate-generated-praxis-config.py" "$EXPECTED_ENDPOINTS_FILE" \
        >"$EVIDENCE_DIR/external-praxis-config-validation.json" || {
      CURRENT_FAILURE='Grid-generated Praxis ConfigMap failed overlay, reload, endpoint, or credential-field validation'
      printf 'external Grid-generated Praxis ConfigMap failed structural validation; see sanitized validation result\n' >&2
      exit 1
    }
    live_config_sha="$(jq -er '.sha256' "$EVIDENCE_DIR/external-praxis-config-validation.json")"
    source_config_sha=''
    if [[ -n "$PRAXIS_CONFIG_FILE" ]]; then source_config_sha="$(sha256sum "$PRAXIS_CONFIG_FILE" | awk '{print $1}')"; fi
    endpoint_inventory_sha="$(sha256sum "$EXPECTED_ENDPOINTS_FILE" | awk '{print $1}')"
    jq --arg liveSha "$live_config_sha" --arg sourceSha "$source_config_sha" --arg endpointsSha "$endpoint_inventory_sha" \
      '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,resourceVersion:.metadata.resourceVersion,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels},dataKeys:(.data|keys),praxisYamlLiveSha256:$liveSha,priorSourceSnapshotSha256:(if $sourceSha=="" then null else $sourceSha end),expectedEndpointInventorySha256:$endpointsSha,gridGenerated:true}' \
      <<<"$external_configmap_json" >"$EVIDENCE_DIR/external-praxis-configmap-metadata.json"
    unset external_configmap_json
  else
  cat >"$EVIDENCE_DIR/praxis-config.yaml" <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: $PRAXIS_CONFIG_MAP_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
data:
  praxis.yaml: |
$(sed 's/^/    /' "$PRAXIS_CONFIG_FILE")
YAML
  chmod 0600 "$EVIDENCE_DIR/praxis-config.yaml"
  fi
  if [[ -n "$PRAXIS_CONFIG_FILE" ]]; then
  if [[ -n "$PRAXIS_CONFIG_FILE" ]]; then
    sha256sum "$PRAXIS_CONFIG_FILE" >"$EVIDENCE_DIR/praxis-config-source-sha256.txt"
  fi
  fi
fi

operator_bootstrap_required=false
if [[ "$CONFIG_SOURCE" == operator ]]; then
  cat >"$EVIDENCE_DIR/operator-bootstrap-configmap.yaml" <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: $OPERATOR_CONFIG_MAP_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
data:
  praxis.yaml: |
$(sed 's/^/    /' "$BOOTSTRAP_CONFIG_FILE")
YAML
  chmod 0600 "$EVIDENCE_DIR/operator-bootstrap-configmap.yaml"
  if operator_config_json="$(oc get configmap "$OPERATOR_CONFIG_MAP_NAME" -n "$NAMESPACE" -o json 2>/dev/null)"; then
    jq -e --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" --arg gateway "$SERVICE_NAME" '
      (.metadata.labels["praxis.fast/qualification-run"]==$run) or
      (.metadata.labels["app.kubernetes.io/managed-by"]=="grid-operator" and .metadata.labels["grid.praxis.fast/network"]==$network and .metadata.labels["grid.praxis.fast/gateway"]==$gateway)
    ' <<<"$operator_config_json" >/dev/null || {
      CURRENT_FAILURE='operator Praxis ConfigMap exists without this run label or matching task Grid ownership labels'
      printf 'operator Praxis ConfigMap exists outside this run ownership; refusing adoption\n' >&2
      exit 1
    }
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels},dataKeys:(.data|keys)}' \
      <<<"$operator_config_json" >"$EVIDENCE_DIR/operator-configmap-before-metadata.json"
    jq -e '.data["praxis.yaml"]|type=="string" and length>0' <<<"$operator_config_json" >/dev/null || {
      CURRENT_FAILURE='operator Praxis ConfigMap has no nonempty bootstrap or generated config'
      printf 'operator Praxis ConfigMap lacks a nonempty praxis.yaml key\n' >&2
      exit 1
    }
    unset operator_config_json
  else
    operator_bootstrap_required=true
    oc apply --dry-run=server -f "$EVIDENCE_DIR/operator-bootstrap-configmap.yaml" >"$EVIDENCE_DIR/operator-bootstrap-configmap-dry-run.txt"
  fi
fi

cat >"$EVIDENCE_DIR/gateway-route.yaml" <<YAML
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: $GATEWAY_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  gatewayClassName: $GATEWAY_CLASS
  listeners:
    - name: https
      hostname: $HOSTNAME
      port: 443
      protocol: HTTPS
      tls:
        mode: Terminate
        certificateRefs:
          - group: ""
            kind: Secret
            name: $TLS_SECRET_NAME
      allowedRoutes:
        namespaces:
          from: Same
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: $HTTPROUTE_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: $GATEWAY_NAME
      sectionName: https
  hostnames:
    - $HOSTNAME
  rules:
    - matches:
        - path:
            type: PathPrefix
            value: /
      backendRefs:
        - group: ""
          kind: Service
          name: $SERVICE_NAME
          port: $SERVICE_PORT
          weight: 1
YAML
chmod 0600 "$EVIDENCE_DIR/gateway-values.yaml" "$EVIDENCE_DIR/rendered-gateway.yaml" "$EVIDENCE_DIR/gateway-route.yaml"
cp "$CHART_FILE" "$EVIDENCE_DIR/praxis-gateway-$CHART_VERSION.tgz"
chmod 0600 "$EVIDENCE_DIR/praxis-gateway-$CHART_VERSION.tgz"
sha256sum "$EVIDENCE_DIR/gateway-values.yaml" "$EVIDENCE_DIR/rendered-gateway.yaml" \
  "$EVIDENCE_DIR/gateway-route.yaml" >"$EVIDENCE_DIR/render-sha256.txt"

cat >"$EVIDENCE_DIR/INVOCATION.md" <<EOF
# Task-owned Grid Gateway invocation

- Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- Context: $context
- Helm Kubernetes context: $KUBE_CONTEXT
- User: $user
- API server: $server
- API readiness: $ready
- API TLS mode: $API_TLS_MODE
- Run label: $RUN_LABEL
- Grid source revision: $SOURCE_REVISION
- Chart version/archive SHA-256: $CHART_VERSION / $observed_chart_sha
- Gateway image: $IMAGE
- Gateway release/namespace: $RELEASE_NAME / $NAMESPACE
- Gateway Service: $SERVICE_NAME:$SERVICE_PORT
- Gateway API object: $GATEWAY_NAME using class $GATEWAY_CLASS
- HTTPRoute: $HTTPROUTE_NAME
- Listener hostname: $HOSTNAME
- Backend: $NAMESPACE/$SERVICE_NAME:$SERVICE_PORT
- GridNetwork/site: $GRID_NETWORK_NAME / $GRID_SITE_NAME
- Resource requests/limits: CPU $CPU_REQUEST/$CPU_LIMIT; memory $MEMORY_REQUEST/$MEMORY_LIMIT
- Praxis config: $(if [[ "$CONFIG_SOURCE" == operator ]]; then printf 'Grid operator-generated consumer config; bootstrap 503 ConfigMap is temporary and mount-reconciliation readiness is a separate gate'; elif [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then printf 'Grid-operator-generated task ConfigMap %s, structurally validated for overlay reload and both expected pool endpoints; live and prior source hashes are recorded separately' "$PRAXIS_CONFIG_MAP_NAME"; elif [[ -n "$PRAXIS_CONFIG_FILE" ]]; then printf 'task-owned BYO ConfigMap %s; source SHA-256 is recorded separately' "$PRAXIS_CONFIG_MAP_NAME"; else printf 'bootstrap responder only; signal routing is not configured'; fi)
- Grid overlay ConfigMap: ${OVERLAY_CONFIGMAP:-not mounted}

This phase creates only the task-owned Gateway path. It does not deploy inference workloads, ExtProc, or send inference traffic.
No bearer token, Secret values, private key, certificate bytes, or kubeconfig data is recorded.
EOF

printf 'context=%s\nuser=%s\nserver=%s\nreadyz=%s\napi_tls_mode=%s\n' \
  "$context" "$user" "$server" "$ready" "$API_TLS_MODE" >"$EVIDENCE_DIR/api-access-summary.txt"

if [[ "$MODE" == verify || "$MODE" == collect ]]; then
  CURRENT_PHASE=read-only-collection
else
  CURRENT_PHASE=certificate-validation
  tmpdir="$(mktemp -d)"
  identity_json="$(oc get secret "$SOURCE_IDENTITY_SECRET" -n "$SOURCE_SECRET_NAMESPACE" -o json)"
  ca_json="$(oc get secret "$SOURCE_CA_SECRET" -n "$SOURCE_SECRET_NAMESPACE" -o json)"
  jq -e '(.data|keys|sort)==["tls.crt","tls.key"]' <<<"$identity_json" >/dev/null || {
    CURRENT_FAILURE='task site-identity Secret does not have exactly tls.crt/tls.key'
    printf 'task site-identity Secret key names are not the expected TLS pair\n' >&2
    exit 1
  }
  jq -e '.data|has("ca.crt")' <<<"$ca_json" >/dev/null || {
    CURRENT_FAILURE='task Grid CA Secret lacks ca.crt'
    printf 'task Grid CA Secret has no ca.crt key\n' >&2
    exit 1
  }
  printf '%s' "$(jq -r '.data["tls.crt"]' <<<"$identity_json")" | base64 -d >"$tmpdir/site.crt"
  printf '%s' "$(jq -r '.data["ca.crt"]' <<<"$ca_json")" | base64 -d >"$tmpdir/grid-ca.crt"
  openssl verify -CAfile "$tmpdir/grid-ca.crt" "$tmpdir/site.crt" >"$EVIDENCE_DIR/grid-identity-chain.txt"
  openssl x509 -in "$tmpdir/site.crt" -noout -subject -issuer -dates -fingerprint -sha256 -ext subjectAltName \
    >"$EVIDENCE_DIR/grid-identity-certificate-metadata.txt"
  grep -Fq "DNS:$HOSTNAME" "$EVIDENCE_DIR/grid-identity-certificate-metadata.txt" || {
    CURRENT_FAILURE='task site identity certificate SAN does not match Gateway hostname'
    printf 'task site certificate SAN does not match configured Gateway hostname\n' >&2
    exit 1
  }
  cert_pub_sha="$(openssl x509 -in "$tmpdir/site.crt" -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum | awk '{print $1}')"
  key_pub_sha="$(printf '%s' "$(jq -r '.data["tls.key"]' <<<"$identity_json")" | base64 -d | openssl pkey -pubout -outform DER | sha256sum | awk '{print $1}')"
  [[ "$cert_pub_sha" == "$key_pub_sha" ]] || {
    CURRENT_FAILURE='task site identity certificate and private key do not form a pair'
    printf 'task site identity certificate/key pair validation failed\n' >&2
    exit 1
  }
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},type,keyNames:(.data|keys)}' \
    <<<"$identity_json" >"$EVIDENCE_DIR/source-site-identity-metadata.json"
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},type,keyNames:(.data|keys)}' \
    <<<"$ca_json" >"$EVIDENCE_DIR/source-grid-ca-metadata.json"

  CURRENT_PHASE='server-dry-run'
  if [[ -n "$PRAXIS_CONFIG_FILE" && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false ]]; then
    if existing_config_json="$(oc get configmap "$PRAXIS_CONFIG_MAP_NAME" -n "$NAMESPACE" -o json 2>/dev/null)"; then
      jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
        <<<"$existing_config_json" >/dev/null || {
        CURRENT_FAILURE='same-name Praxis ConfigMap exists without matching task-run ownership'
        printf 'Praxis ConfigMap exists outside this run ownership\n' >&2
        exit 1
      }
      existing_config_sha="$(jq -r '.data["praxis.yaml"] // empty' <<<"$existing_config_json" | sha256sum | awk '{print $1}')"
      jq --arg sha "$existing_config_sha" \
        '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels},dataKeys:(.data|keys),praxisYamlSha256:$sha}' \
        <<<"$existing_config_json" >"$EVIDENCE_DIR/praxis-config-before-metadata.json"
      unset existing_config_json
    fi
    oc apply --dry-run=server -f "$EVIDENCE_DIR/praxis-config.yaml" >"$EVIDENCE_DIR/praxis-config-dry-run.txt"
  fi
  oc apply --dry-run=server -f "$EVIDENCE_DIR/gateway-route.yaml" >"$EVIDENCE_DIR/gateway-route-dry-run.txt"

  if [[ "$MODE" == render ]]; then
    jq -n --arg status rendered --arg evidence "$EVIDENCE_DIR" --arg image "$IMAGE" \
      --arg sourceRevision "$SOURCE_REVISION" --arg chartSha256 "$observed_chart_sha" \
      '{mode:"render",status:$status,evidenceDirectory:$evidence,image:$image,sourceRevision:$sourceRevision,chartSha256:$chartSha256,clusterMutated:false,inferenceTrafficSent:false}' \
      >"$EVIDENCE_DIR/RESULT.json"
    log "Grid Gateway render and server-side dry-runs passed; no cluster resources changed"
    exit 0
  fi

  CURRENT_PHASE=task-secret-bootstrap
  if destination_identity_json="$(oc get secret "$TLS_SECRET_NAME" -n "$NAMESPACE" -o json 2>/dev/null)"; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and (.data|keys|sort)==["tls.crt","tls.key"]' \
      <<<"$destination_identity_json" >/dev/null || {
      CURRENT_FAILURE='same-name destination TLS Secret exists without matching task ownership or expected key names'
      printf 'destination TLS Secret exists outside this run ownership\n' >&2
      exit 1
    }
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},type,keyNames:(.data|keys)}' \
      <<<"$destination_identity_json" >"$EVIDENCE_DIR/destination-identity-before-metadata.json"
  fi
  if destination_ca_json="$(oc get secret "$SOURCE_CA_SECRET" -n "$NAMESPACE" -o json 2>/dev/null)"; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and (.data|keys)==["ca.crt"]' \
      <<<"$destination_ca_json" >/dev/null || {
      CURRENT_FAILURE='same-name destination CA Secret exists without matching task ownership or public-ca-only contents'
      printf 'destination CA Secret exists outside this run ownership\n' >&2
      exit 1
    }
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},type,keyNames:(.data|keys)}' \
      <<<"$destination_ca_json" >"$EVIDENCE_DIR/destination-ca-before-metadata.json"
  fi
  jq --arg ns "$NAMESPACE" --arg name "$TLS_SECRET_NAME" --arg run "$RUN_LABEL" \
    '{apiVersion:"v1",kind:"Secret",metadata:{name:$name,namespace:$ns,labels:{"app.kubernetes.io/part-of":"grid-llmd-e2e","app.kubernetes.io/managed-by":"grid-llmd-e2e-run","praxis.fast/qualification-run":$run}},type:"kubernetes.io/tls",data:{"tls.crt":.data["tls.crt"],"tls.key":.data["tls.key"]}}' \
    <<<"$identity_json" | oc apply -f - >/dev/null
  jq --arg ns "$NAMESPACE" --arg name "$SOURCE_CA_SECRET" --arg run "$RUN_LABEL" \
    '{apiVersion:"v1",kind:"Secret",metadata:{name:$name,namespace:$ns,labels:{"app.kubernetes.io/part-of":"grid-llmd-e2e","app.kubernetes.io/managed-by":"grid-llmd-e2e-run","praxis.fast/qualification-run":$run}},type:"Opaque",data:{"ca.crt":.data["ca.crt"]}}' \
    <<<"$ca_json" | oc apply -f - >/dev/null
  unset identity_json ca_json
  unset destination_identity_json destination_ca_json

  if [[ -n "$PRAXIS_CONFIG_FILE" && "$PRAXIS_CONFIG_MAP_EXTERNAL" == false ]]; then
    CURRENT_PHASE=praxis-config-apply
    oc apply -f "$EVIDENCE_DIR/praxis-config.yaml" >"$EVIDENCE_DIR/praxis-config-apply.txt"
  fi
  if [[ "$CONFIG_SOURCE" == operator && "$operator_bootstrap_required" == true ]]; then
    CURRENT_PHASE=operator-bootstrap-configmap-apply
    oc apply -f "$EVIDENCE_DIR/operator-bootstrap-configmap.yaml" >"$EVIDENCE_DIR/operator-bootstrap-configmap-apply.txt"
  fi

  CURRENT_PHASE=helm-install
  printf 'helm --kube-context=%q upgrade --install %q %q --namespace %q --values %q --wait --timeout %q\n' \
    "$KUBE_CONTEXT" "$RELEASE_NAME" "$CHART_FILE" "$NAMESPACE" "$EVIDENCE_DIR/gateway-values.yaml" "$TIMEOUT" \
    >"$EVIDENCE_DIR/replay-command.txt"
  helm upgrade --install "$RELEASE_NAME" "$CHART_FILE" --namespace "$NAMESPACE" \
    --values "$EVIDENCE_DIR/gateway-values.yaml" --wait --timeout "$TIMEOUT" \
    >"$EVIDENCE_DIR/helm-install.log" 2>&1
  oc wait deployment/"$DEPLOYMENT_NAME" -n "$NAMESPACE" --for=condition=Available --timeout="$TIMEOUT" \
    >"$EVIDENCE_DIR/deployment-wait.txt"
  CURRENT_PHASE=gateway-route-apply
  oc apply -f "$EVIDENCE_DIR/gateway-route.yaml" >"$EVIDENCE_DIR/gateway-route-apply.txt"
fi

CURRENT_PHASE=readiness-verification
deadline=$((SECONDS + ${GRID_GATEWAY_READINESS_TIMEOUT_SECONDS:-480}))
while ((SECONDS < deadline)); do
  if oc get gateway "$GATEWAY_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gateway.json" 2>/dev/null &&
     jq -e --arg class "$GATEWAY_CLASS" --arg host "$HOSTNAME" --arg tls "$TLS_SECRET_NAME" \
       '.metadata.generation as $g |
        [.status.conditions[]?|select(.type=="Accepted" or .type=="Programmed")] as $cs |
        ($cs|length)==2 and all($cs[]; .status=="True" and .observedGeneration==$g) and
        .spec.gatewayClassName==$class and
        any(.spec.listeners[]?; .name=="https" and .hostname==$host and .port==443 and .protocol=="HTTPS" and .tls.mode=="Terminate" and any(.tls.certificateRefs[]?;.name==$tls)) and
        any(.status.listeners[]?; .name=="https" and (.attachedRoutes // 0)>=1 and all(.conditions[]?|select(.type=="Accepted" or .type=="Programmed" or .type=="ResolvedRefs"); .status=="True" and .observedGeneration==$g))' \
       "$EVIDENCE_DIR/gateway.json" >/dev/null; then
    if oc get httproute "$HTTPROUTE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/httproute.json" 2>/dev/null &&
       jq -e --arg ns "$NAMESPACE" --arg parent "$GATEWAY_NAME" --arg host "$HOSTNAME" \
         --arg svc "$SERVICE_NAME" --argjson port "$SERVICE_PORT" \
         '.metadata.generation as $g |
          any(.status.parents[]?;
            .controllerName=="openshift.io/gateway-controller/v1" and
            .parentRef.name==$parent and
            ((.parentRef.namespace // $ns)==$ns) and
            all(.conditions[]?|select(.type=="Accepted" or .type=="ResolvedRefs"); .status=="True" and .observedGeneration==$g) and
            ([.conditions[]?|select(.type=="Accepted" or .type=="ResolvedRefs")]|length)==2
          ) and
          .spec.hostnames==[$host] and
          .spec.parentRefs[0].name==$parent and
          .spec.parentRefs[0].sectionName=="https" and
          ([.spec.rules[].matches[]?|select(.path.type=="PathPrefix" and .path.value=="/")]|length)==1 and
          ([.spec.rules[].backendRefs[]?]|length)==1 and
          ([.spec.rules[].backendRefs[]?|select(.name==$svc and .port==$port)]|length)==1' \
         "$EVIDENCE_DIR/httproute.json" >/dev/null; then
      break
    fi
  fi
  sleep 5
done

CURRENT_PHASE=rollout-stability
oc rollout status deployment/"$DEPLOYMENT_NAME" -n "$NAMESPACE" --timeout="$TIMEOUT" \
  >"$EVIDENCE_DIR/rollout-status.txt"

oc get deployment "$DEPLOYMENT_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/deployment.json"
deployment_revision="$(jq -er '.metadata.annotations["deployment.kubernetes.io/revision"]' "$EVIDENCE_DIR/deployment.json")"
oc get replicasets -n "$NAMESPACE" -l app.kubernetes.io/instance="$RELEASE_NAME" -o json |
  jq '[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},spec:{replicas:.spec.replicas},status:{readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas}}]' \
    >"$EVIDENCE_DIR/replicasets.json"
current_replicaset="$(jq -er --arg revision "$deployment_revision" '
  [.[]|select(.metadata.annotations["deployment.kubernetes.io/revision"]==$revision and (.spec.replicas // 0)>0)]
  | if length==1 then .[0].metadata.name else error("current Deployment revision does not identify exactly one active ReplicaSet") end
' "$EVIDENCE_DIR/replicasets.json")"
printf '%s\n' "$current_replicaset" >"$EVIDENCE_DIR/current-replicaset.txt"
oc get pods -n "$NAMESPACE" -l app.kubernetes.io/instance="$RELEASE_NAME" -o json |
  jq --arg rs "$current_replicaset" '[.items[]|select(any(.metadata.ownerReferences[]?;.kind=="ReplicaSet" and .name==$rs))|{name:.metadata.name,uid:.metadata.uid,phase:.status.phase,node:.spec.nodeName,conditions:.status.conditions,containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]' \
  >"$EVIDENCE_DIR/pods.json"
oc get service "$SERVICE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/service.json"
oc get endpoints "$SERVICE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/endpoints.json"
oc get endpointslices -n "$NAMESPACE" -l kubernetes.io/service-name="$SERVICE_NAME" -o json \
  | jq '[.items[]|{name:.metadata.name,ports:.ports,endpoints:[.endpoints[]?|{addresses,conditions,targetRef:{kind:.targetRef.kind,name:.targetRef.name,uid:.targetRef.uid}}]}]' \
  >"$EVIDENCE_DIR/endpointslices.json"
jq -e --arg image "$IMAGE" --arg digest "$IMAGE_DIGEST" \
  '.status.observedGeneration==.metadata.generation and .status.readyReplicas==1 and .status.availableReplicas==1 and all(.spec.template.spec.containers[];.image==$image)' \
  "$EVIDENCE_DIR/deployment.json" >/dev/null || {
  CURRENT_FAILURE='Praxis Gateway Deployment is not current, available 1/1, and configured with the locked image'
  printf 'Praxis Gateway Deployment did not reach current 1/1 availability\n' >&2
  exit 1
}
jq -e --arg image "$IMAGE" --arg digest "$IMAGE_DIGEST" \
  'length==1 and .[0].phase=="Running" and all(.[0].containers[];.ready==true and .image==$image and (.imageID|endswith($digest)))' \
  "$EVIDENCE_DIR/pods.json" >/dev/null || {
  CURRENT_FAILURE='Praxis Gateway runtime Pod imageID does not match the immutable image pin'
  printf 'Praxis Gateway runtime Pod is not Ready at the locked image digest\n' >&2
  exit 1
}
jq -e '.subsets|any(.[];.addresses|length>0)' "$EVIDENCE_DIR/endpoints.json" >/dev/null || {
  CURRENT_FAILURE='Praxis Grid Gateway Service has no ready endpoint'
  printf 'Praxis Grid Gateway Service has no ready endpoint\n' >&2
  exit 1
}
jq -e --arg class "$GATEWAY_CLASS" --arg host "$HOSTNAME" --arg tls "$TLS_SECRET_NAME" \
  '.metadata.generation as $g |
   [.status.conditions[]?|select(.type=="Accepted" or .type=="Programmed")] as $cs |
   ($cs|length)==2 and all($cs[]; .status=="True" and .observedGeneration==$g) and
   .spec.gatewayClassName==$class and
   any(.spec.listeners[]?; .name=="https" and .hostname==$host and .port==443 and .protocol=="HTTPS" and .tls.mode=="Terminate" and any(.tls.certificateRefs[]?;.name==$tls)) and
   any(.status.listeners[]?; .name=="https" and (.attachedRoutes // 0)>=1 and all(.conditions[]?|select(.type=="Accepted" or .type=="Programmed" or .type=="ResolvedRefs"); .status=="True" and .observedGeneration==$g))' \
  "$EVIDENCE_DIR/gateway.json" >/dev/null || {
  CURRENT_FAILURE='task Gateway is not current, Accepted, and Programmed'
  printf 'task Gateway did not reach current Accepted/Programmed conditions\n' >&2
  exit 1
}
jq -e --arg ns "$NAMESPACE" --arg parent "$GATEWAY_NAME" --arg host "$HOSTNAME" \
  --arg svc "$SERVICE_NAME" --argjson port "$SERVICE_PORT" \
  '.metadata.generation as $g |
   any(.status.parents[]?;
     .controllerName=="openshift.io/gateway-controller/v1" and
     .parentRef.name==$parent and ((.parentRef.namespace // $ns)==$ns) and
     all(.conditions[]?|select(.type=="Accepted" or .type=="ResolvedRefs"); .status=="True" and .observedGeneration==$g) and
     ([.conditions[]?|select(.type=="Accepted" or .type=="ResolvedRefs")]|length)==2
   ) and .spec.hostnames==[$host] and .spec.parentRefs[0].name==$parent and
   .spec.parentRefs[0].sectionName=="https" and
   ([.spec.rules[].matches[]?|select(.path.type=="PathPrefix" and .path.value=="/")]|length)==1 and
   ([.spec.rules[].backendRefs[]?]|length)==1 and
   ([.spec.rules[].backendRefs[]?|select(.name==$svc and .port==$port)]|length)==1' \
  "$EVIDENCE_DIR/httproute.json" >/dev/null || {
  CURRENT_FAILURE='task HTTPRoute is not current/Accepted/ResolvedRefs or does not target the named Grid Gateway Service and port'
  printf 'task HTTPRoute gate failed\n' >&2
  exit 1
}

jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,containers:[.spec.template.spec.containers[]|{name,image,resources}]},status:{observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
  "$EVIDENCE_DIR/deployment.json" >"$EVIDENCE_DIR/deployment-summary.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:{type:.spec.type,ports:.spec.ports,selector:.spec.selector},clusterIP:.spec.clusterIP}' \
  "$EVIDENCE_DIR/service.json" >"$EVIDENCE_DIR/service-summary.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:{gatewayClassName:.spec.gatewayClassName,listeners:[.spec.listeners[]|{name,hostname,port,protocol,tls,allowedRoutes}]},status:{addresses:.status.addresses,conditions:.status.conditions,listeners:.status.listeners}}' \
  "$EVIDENCE_DIR/gateway.json" >"$EVIDENCE_DIR/gateway-summary.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:{parentRefs:.spec.parentRefs,hostnames:.spec.hostnames,rules:.spec.rules},status:{parents:.status.parents}}' \
  "$EVIDENCE_DIR/httproute.json" >"$EVIDENCE_DIR/httproute-summary.json"
oc get secret "$TLS_SECRET_NAME" -n "$NAMESPACE" -o json |
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},type,keyNames:(.data|keys)}' \
  >"$EVIDENCE_DIR/task-site-identity-metadata.json"
oc get secret "$SOURCE_CA_SECRET" -n "$NAMESPACE" -o json |
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,annotations:.metadata.annotations,ownerReferences:.metadata.ownerReferences},type,keyNames:(.data|keys)}' \
  >"$EVIDENCE_DIR/task-grid-ca-metadata.json"
oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp >"$EVIDENCE_DIR/events.txt" 2>&1 || true
oc logs deployment/"$DEPLOYMENT_NAME" -n "$NAMESPACE" --all-containers=true --tail=200 \
  >"$EVIDENCE_DIR/gateway-logs.txt" 2>&1 || true

gateway_address="$(jq -r '[.status.addresses[]?.value // empty] | first // ""' "$EVIDENCE_DIR/gateway.json")"
cat >"$EVIDENCE_DIR/source-lock.md" <<EOF
# Praxis Grid Gateway source and artifact lock

- Praxis Grid source revision: $SOURCE_REVISION
- Chart: praxis-gateway $CHART_VERSION
- Chart archive SHA-256: $observed_chart_sha
- Chart archive in this evidence directory: $(basename "$EVIDENCE_DIR/praxis-gateway-$CHART_VERSION.tgz")
- Runtime image: $IMAGE
- Runtime imageID: $(jq -r --arg image "$IMAGE" '.[0].containers[]|select(.image==$image)|.imageID' "$EVIDENCE_DIR/pods.json")
- Run label: $RUN_LABEL
- Deployment namespace: $NAMESPACE
- Gateway API object/class: $GATEWAY_NAME / $GATEWAY_CLASS
- HTTPRoute: $HTTPROUTE_NAME
- Grid Gateway Service: $NAMESPACE/$SERVICE_NAME:$SERVICE_PORT
- Listener hostname: $HOSTNAME
- Gateway controller address: ${gateway_address:-not reported}
- TLS source: current-run Grid operator Secret metadata; identity values are not recorded
- Praxis config source: $(if [[ "$CONFIG_SOURCE" == operator ]]; then printf 'Grid operator-generated consumer config in %s; overlay-aware routing and mounted revision are verified by the operator handoff gate' "$OPERATOR_CONFIG_MAP_NAME"; elif [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then printf 'Grid operator-generated ConfigMap %s is ownership-checked and mounted by the task Gateway chart with overlay %s' "$PRAXIS_CONFIG_MAP_NAME" "$OVERLAY_CONFIGMAP"; elif [[ -n "$PRAXIS_CONFIG_FILE" ]]; then printf 'task-owned BYO ConfigMap %s with Grid overlay mount %s' "$PRAXIS_CONFIG_MAP_NAME" "$OVERLAY_CONFIGMAP"; else printf 'chart bootstrap responder only; signal routing is not configured'; fi)

No Secret values or private keys are in this evidence directory.
EOF
jq -n --arg mode "$MODE" --arg evidence "$EVIDENCE_DIR" \
  --arg image "$IMAGE" --arg source "$SOURCE_REVISION" --arg address "$gateway_address" \
  --arg praxisConfigMode "$(if [[ "$CONFIG_SOURCE" == operator ]]; then printf operator-generated-overlay-aware; elif [[ "$PRAXIS_CONFIG_MAP_EXTERNAL" == true ]]; then printf grid-generated-external-overlay-aware; elif [[ -n "$PRAXIS_CONFIG_FILE" ]]; then printf task-owned-byo-overlay; else printf bootstrap-responder-only; fi)" \
  '{mode:$mode,status:(if $mode=="render" then "rendered" elif $praxisConfigMode=="operator-generated-overlay-aware" or $praxisConfigMode=="grid-generated-external-overlay-aware" or $praxisConfigMode=="task-owned-byo-overlay" then "ready" else "bootstrap-only" end),evidenceDirectory:$evidence,image:$image,sourceRevision:$source,gatewayAddress:$address,praxisConfigMode:$praxisConfigMode,inferenceTrafficSent:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
log "task Grid Gateway Service and HTTPS route $MODE complete: source=$SOURCE_REVISION image=$IMAGE; no inference traffic sent"
printf 'Evidence: %s\n' "$EVIDENCE_DIR"
