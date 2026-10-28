#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install/verify the pinned downstream ODH Grid operator for one qualification run.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${GRID_OPERATOR_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
RESOURCE_PROFILE="${RESOURCE_PROFILE:-strict}"
CHART_FILE="${GRID_OPERATOR_CHART_FILE:-}"
CHART_SHA256="${GRID_OPERATOR_CHART_SHA256:-}"
CHART_VERSION="${GRID_OPERATOR_CHART_VERSION:-0.2.2}"
SOURCE_REVISION="${GRID_OPERATOR_SOURCE_REVISION:-}"
IMAGE="${GRID_OPERATOR_IMAGE:-}"
RELEASE_NAME="${GRID_OPERATOR_RELEASE_NAME:-grid-operator}"
RELEASE_NAMESPACE="${GRID_OPERATOR_NAMESPACE:-praxis-grid-operator-system}"
RESOURCE_NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
DEPLOYMENT_NAME="${GRID_OPERATOR_DEPLOYMENT_NAME:-grid-operator}"
SERVICE_ACCOUNT_NAME="${GRID_OPERATOR_SERVICE_ACCOUNT_NAME:-grid-operator}"
GRID_NETWORK_NAME="${GRID_NETWORK_NAME:-grid-llmd-e2e}"
GRID_SITE_NAME="${GRID_SITE_NAME:-grid-llmd-site}"
POST_RENDERER="$SCRIPT_DIR/gridnetwork-scoring-post-renderer.py"
GRID_ID="${GRID_ID:-grid-llmd-e2e}"
GRID_SITE_REGION="${GRID_SITE_REGION:-us-west-1}"
GRID_SITE_ZONE="${GRID_SITE_ZONE:-}"
GRID_GATEWAY_SERVICE_NAME="${GRID_GATEWAY_SERVICE_NAME:-grid-gateway}"
GRID_GATEWAY_NAMESPACE="${GRID_GATEWAY_NAMESPACE:-$RESOURCE_NAMESPACE}"
GRID_GATEWAY_PORT="${GRID_GATEWAY_PORT:-8080}"
GRID_GATEWAY_ADDRESS="${GRID_GATEWAY_ADDRESS:-}"
GRID_GATEWAY_DISCOVERY_ENABLED="${GRID_GATEWAY_DISCOVERY_ENABLED:-false}"
GRID_PRAXIS_CONFIG_GENERATE="${GRID_PRAXIS_CONFIG_GENERATE:-false}"
GRID_PRAXIS_CONFIG_MAP_NAME="${GRID_PRAXIS_CONFIG_MAP_NAME:-grid-llmd-praxis-config}"
GRID_PRAXIS_CONFIG_LISTENER_PORT="${GRID_PRAXIS_CONFIG_LISTENER_PORT:-8080}"
GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE="${GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE:-}"
GRID_PROVIDER_HOP_ENDPOINTS_FILE="${GRID_PROVIDER_HOP_ENDPOINTS_FILE:-}"
GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION="${GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION:-false}"
GRID_PRAXIS_SITE_ROUTER="${GRID_PRAXIS_SITE_ROUTER:-intelligentRoute}"
GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME="${GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME:-grid-gateway}"
GRID_PRAXIS_CONFIG_CONTAINER_NAME="${GRID_PRAXIS_CONFIG_CONTAINER_NAME:-praxis}"
GRID_TLS_SECRET_NAMESPACE="${GRID_TLS_SECRET_NAMESPACE:-}"
GRID_SWIM_REQUIRE_KEY="${GRID_SWIM_REQUIRE_KEY:-false}"
GRID_SWIM_KEY_SECRET_NAME="${GRID_SWIM_KEY_SECRET_NAME:-}"
CPU_REQUEST="${GRID_OPERATOR_CPU_REQUEST:-}"
MEMORY_REQUEST="${GRID_OPERATOR_MEMORY_REQUEST:-128Mi}"
CPU_LIMIT="${GRID_OPERATOR_CPU_LIMIT:-500m}"
MEMORY_LIMIT="${GRID_OPERATOR_MEMORY_LIMIT:-512Mi}"
TIMEOUT="${GRID_OPERATOR_TIMEOUT:-10m}"
RUNTIME_READY_TIMEOUT_SECONDS="${GRID_OPERATOR_RUNTIME_TIMEOUT_SECONDS:-300}"
EVIDENCE_DIR="${GRID_OPERATOR_EVIDENCE_DIR:-}"

case "$RESOURCE_PROFILE" in
  strict) PROFILE_CPU_REQUEST=50m ;;
  reduced-dev)
    [[ "$CLUSTER_ENV" == ephemeral-dev ]] || {
      printf 'RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev\n' >&2
      exit 2
    }
    PROFILE_CPU_REQUEST=25m
    ;;
  *) printf 'RESOURCE_PROFILE must be strict or reduced-dev\n' >&2; exit 2 ;;
esac
[[ "$RUNTIME_READY_TIMEOUT_SECONDS" =~ ^[1-9][0-9]{0,3}$ ]] || {
  printf 'GRID_OPERATOR_RUNTIME_TIMEOUT_SECONDS must be an integer from 1 to 9999\n' >&2
  exit 2
}
if [[ -n "$CPU_REQUEST" && "$CPU_REQUEST" != "$PROFILE_CPU_REQUEST" ]]; then
  printf 'GRID_OPERATOR_CPU_REQUEST conflicts with the selected RESOURCE_PROFILE\n' >&2
  exit 2
fi
CPU_REQUEST="$PROFILE_CPU_REQUEST"

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
Pinned ODH Grid operator plan (plan mode makes no cluster calls):
  1. Verify API server/context and record the explicitly selected API TLS mode.
  2. Verify the immutable chart archive SHA, chart source revision, and ODH image digest.
  3. Render and save the chart with task-run labels and the configured low-resource request.
  4. Refuse pre-existing namespaces or Helm releases owned by another run.
  5. Create/reuse only matching task-owned namespaces, then install the pinned ODH chart.
  6. Optionally enable GridNetwork.praxisConfig generation only with a nonempty, validated endpoint inventory.
  7. Verify configured image and runtime imageID, Deployment availability, CRD establishment, ServiceAccounts/RBAC, and any declared webhook endpoints.
  8. Capture bounded operator logs, events, sanitized invocation, and source/artifact identity.

Required: GRID_OPERATOR_CHART_FILE, GRID_OPERATOR_CHART_SHA256,
GRID_OPERATOR_SOURCE_REVISION, GRID_OPERATOR_IMAGE (image@sha256), GRID_RUN_LABEL,
EXPECTED_API_SERVER. TLS bypass requires CLUSTER_ENV=ephemeral-dev and
ALLOW_INSECURE_DEV_API=true. The single-site default omits the SWIM key
reference; a multi-site run must provide a task-owned valid key Secret.
Application TLS checks are not changed by this script.
Set GRID_PRAXIS_CONFIG_GENERATE=true only after real provider endpoints are ready,
and provide GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE as a JSON array containing each
candidate cluster's address and explicit transport. The helper fails before
cluster mutation if the endpoint inventory is absent, empty, duplicated, or invalid.
Set GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION=true only when that task-owned Gateway
is configured for operator source and carries the matching mount-reconciliation
annotations/RBAC. The helper then waits for both current-generation Praxis config
rendering and mount reconciliation Ready before the run can qualify traffic.
Set GRID_PRAXIS_SITE_ROUTER=gridSiteRoute during bootstrap to select the future
router and start the operator in poll mode. Once providers and real endpoints
are ready, config generation also requires mount reconciliation; the final
Gateway chart rollout must mount the corresponding grid-serving ConfigMap.
GRID_GATEWAY_ADDRESS can explicitly advertise the reachable task-owned Gateway
API address for a ClusterIP Praxis Service; discovery is only for LoadBalancer
Services. The default config generation false preserves overlay-only behavior.
Modes: plan, render, install, verify, collect. Render performs read-only ownership checks and server-side dry-run only.
PLAN
  printf 'Selected resource profile: %s (operator CPU request %s; memory and limits preserved)\n' "$RESOURCE_PROFILE" "$CPU_REQUEST"
  exit 0
fi

for bin in oc helm jq sha256sum awk python3; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
type -P helm >/dev/null || { printf 'helm executable is required\n' >&2; exit 2; }
[[ -x "$POST_RENDERER" ]] || {
  printf 'GridNetwork scoring post-renderer is missing or not executable: %s\n' "$POST_RENDERER" >&2
  exit 2
}
[[ "$MODE" == render || "$MODE" == install || "$MODE" == verify || "$MODE" == collect ]] || {
  printf 'unsupported GRID_OPERATOR_MODE: %s\n' "$MODE" >&2
  exit 2
}
[[ -n "$EXPECTED_API_SERVER" && -n "$SOURCE_REVISION" && -n "$RUN_LABEL" ]] || {
  printf 'EXPECTED_API_SERVER, GRID_OPERATOR_SOURCE_REVISION, and GRID_RUN_LABEL are required\n' >&2
  exit 2
}
[[ "$IMAGE" =~ ^(.+)@(sha256:[0-9a-fA-F]{64})$ ]] || {
  printf 'GRID_OPERATOR_IMAGE must be pinned as repository@sha256:<64 hex>\n' >&2
  exit 2
}
IMAGE_REPOSITORY="${BASH_REMATCH[1]}"
IMAGE_DIGEST="${BASH_REMATCH[2]}"
[[ -f "$CHART_FILE" && "$CHART_SHA256" =~ ^[0-9a-fA-F]{64}$ ]] || {
  printf 'provide an existing chart archive and its 64-character SHA-256\n' >&2
  exit 2
}
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="$REPO_ROOT/evidence/grid-operator-$(date -u +%Y%m%dT%H%M%SZ)"
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
  {
    printf '# Grid operator attempt bug capture\n\n'
    printf -- '- First failing phase: `%s`\n' "${CURRENT_PHASE:-initialization}"
    printf -- '- Exit code: `%s`\n' "$code"
    printf -- '- Time: `%s`\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf -- '- API server: `%s`\n' "$EXPECTED_API_SERVER"
    printf -- '- ODH source revision: `%s`\n' "$SOURCE_REVISION"
    printf -- '- Expected image: `%s`\n' "$IMAGE"
    printf -- '- Failure detail: `%s`\n' "${CURRENT_FAILURE:-see bounded phase evidence}"
    printf -- '- Evidence directory: `%s`\n' "$EVIDENCE_DIR"
    printf '\nNo credentials, Secret values, CA/key contents, or tokens are captured. No inference traffic is sent by this script.\n'
  } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  jq -n --arg mode "$MODE" --arg status blocked --arg phase "${CURRENT_PHASE:-initialization}" \
    --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:$status,phase:$phase,exitCode:$exitCode,evidenceDirectory:$evidence,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json" 2>/dev/null || true
  log "Grid operator mode $MODE stopped with exit=$code at phase=${CURRENT_PHASE:-initialization}"
  exit "$code"
}
capture_exit() {
  local code="$1"
  if ((code != 0)); then capture_failure "$code"; fi
}
trap 'capture_failure $?' ERR
trap 'capture_exit $?' EXIT

CURRENT_PHASE=configuration-validation
GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON='[]'
GRID_PROVIDER_HOP_ENDPOINTS_JSON=''
if [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]]; then
  if [[ -z "$GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE" || ! -f "$GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE" ]]; then
    CURRENT_FAILURE='Praxis config generation requested without a provider endpoint inventory; no cluster mutation was attempted'
    printf 'GRID_PRAXIS_CONFIG_GENERATE=true requires GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE with real provider endpoints\n' >&2
    capture_failure 2
  fi
  if ! jq -e '
    type == "array" and length > 0 and
    all(.[];
      (.cluster | type == "string" and length > 0) and
      (.address | type == "string" and length > 0) and
      (.transport | type == "object") and
      (.transport.mode | IN("mutual_tls", "tls", "plaintext")) and
      ((.transport.mode == "plaintext") or
        (.transport.sni | type == "string" and length > 0)) and
      ((.transport.mode != "plaintext") or
        ((.transport.sni // "") == "" and .transport.caSecretRef == null)) and
      ((.transport.mode == "tls") or .transport.caSecretRef == null)
    ) and ([.[].cluster] | unique | length) == length
  ' "$GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE" >/dev/null; then
    CURRENT_FAILURE='Praxis config endpoint inventory was empty, duplicated, or failed explicit transport validation; no cluster mutation was attempted'
    printf 'cluster endpoint inventory must be a nonempty JSON array with unique clusters and explicit valid transport settings\n' >&2
    capture_failure 2
  fi
  GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON="$(jq -ce '[.[] | {cluster,address,transport}]' "$GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE")"
  cp "$GRID_PRAXIS_CLUSTER_ENDPOINTS_FILE" "$EVIDENCE_DIR/operator-cluster-endpoints.json"
  chmod 0600 "$EVIDENCE_DIR/operator-cluster-endpoints.json"
elif [[ "$GRID_PRAXIS_CONFIG_GENERATE" != false ]]; then
  CURRENT_FAILURE='GRID_PRAXIS_CONFIG_GENERATE must be true or false; no cluster mutation was attempted'
  printf 'GRID_PRAXIS_CONFIG_GENERATE must be true or false\n' >&2
  capture_failure 2
fi
if [[ -n "$GRID_PROVIDER_HOP_ENDPOINTS_FILE" ]]; then
  [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true && "$GRID_PRAXIS_SITE_ROUTER" == intelligentRoute ]] || {
    CURRENT_FAILURE='provider-hop allowlist requires generated intelligentRoute Praxis configuration'
    printf 'GRID_PROVIDER_HOP_ENDPOINTS_FILE requires GRID_PRAXIS_CONFIG_GENERATE=true and GRID_PRAXIS_SITE_ROUTER=intelligentRoute\n' >&2
    capture_failure 2
  }
  [[ -f "$GRID_PROVIDER_HOP_ENDPOINTS_FILE" ]] || {
    CURRENT_FAILURE='provider-hop endpoint inventory file is missing'
    printf 'GRID_PROVIDER_HOP_ENDPOINTS_FILE does not exist\n' >&2
    capture_failure 2
  }
  if ! jq -e --argjson endpoints "$GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON" '
    type == "array" and length == 2 and
    all(.[]; (.cluster|type=="string" and length>0) and
      .transport.mode=="mutual_tls" and (.transport.sni|type=="string" and length>0)) and
    ([.[].cluster]|unique|length)==2 and
    ([.[]|[.cluster,.transport.sni]]|sort) ==
      ([$endpoints[]|select(.transport.mode=="mutual_tls")|[.cluster,.transport.sni]]|sort) and
    ([$endpoints[]|select(.transport.mode=="mutual_tls")]|length)==2
  ' "$GRID_PROVIDER_HOP_ENDPOINTS_FILE" >/dev/null; then
    CURRENT_FAILURE='provider-hop allowlist does not exactly match the two generated Praxis mTLS endpoint clusters and SNI values'
    printf 'providerHopEndpoints must match exactly the two mTLS Praxis cluster endpoints\n' >&2
    capture_failure 2
  fi
  GRID_PROVIDER_HOP_ENDPOINTS_JSON="$(jq -ce '[.[]|{cluster,transport:{mode:"mutual_tls",sni:.transport.sni}}]' "$GRID_PROVIDER_HOP_ENDPOINTS_FILE")"
  cp "$GRID_PROVIDER_HOP_ENDPOINTS_FILE" "$EVIDENCE_DIR/operator-provider-hop-endpoints.json"
  chmod 0600 "$EVIDENCE_DIR/operator-provider-hop-endpoints.json"
fi
[[ "$GRID_PRAXIS_SITE_ROUTER" == intelligentRoute || "$GRID_PRAXIS_SITE_ROUTER" == gridSiteRoute ]] || {
  CURRENT_FAILURE='GRID_PRAXIS_SITE_ROUTER must be intelligentRoute or gridSiteRoute; no cluster mutation was attempted'
  printf 'GRID_PRAXIS_SITE_ROUTER must be intelligentRoute or gridSiteRoute\n' >&2
  capture_failure 2
}
if [[ "$GRID_PRAXIS_SITE_ROUTER" == gridSiteRoute ]]; then
  if [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true && "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" != true ]]; then
    CURRENT_FAILURE='generated gridSiteRoute config requires mount reconciliation; no cluster mutation was attempted'
    printf 'generated GRID_PRAXIS_SITE_ROUTER=gridSiteRoute requires GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION=true\n' >&2
    capture_failure 2
  fi
fi
[[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true || "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == false ]] || {
  CURRENT_FAILURE='GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION must be true or false; no cluster mutation was attempted'
  printf 'GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION must be true or false\n' >&2
  capture_failure 2
}
if [[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true ]]; then
  GRID_TLS_SECRET_NAMESPACE="${GRID_TLS_SECRET_NAMESPACE:-$GRID_GATEWAY_NAMESPACE}"
  [[ "$GRID_TLS_SECRET_NAMESPACE" == "$GRID_GATEWAY_NAMESPACE" ]] || {
    CURRENT_FAILURE='mount reconciliation requires GridNetwork TLS Secret references in the delegated Gateway namespace'
    printf 'GRID_TLS_SECRET_NAMESPACE must match GRID_GATEWAY_NAMESPACE when mount reconciliation is enabled\n' >&2
    capture_failure 2
  }
  [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]] || {
    CURRENT_FAILURE='mount reconciliation requires operator Praxis config generation'
    printf 'GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION=true requires GRID_PRAXIS_CONFIG_GENERATE=true\n' >&2
    capture_failure 2
  }
  [[ "$GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ &&
     "$GRID_PRAXIS_CONFIG_CONTAINER_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || {
    CURRENT_FAILURE='mount reconciliation Deployment/container names are invalid'
    printf 'Grid Praxis mount reconciliation needs valid Deployment and container names\n' >&2
    capture_failure 2
  }
fi
if [[ -n "$GRID_GATEWAY_ADDRESS" && ! "$GRID_GATEWAY_ADDRESS" =~ ^([^[:space:]:]+|\[[0-9a-fA-F:]+\]):[1-9][0-9]{0,4}$ ]]; then
  CURRENT_FAILURE='GRID_GATEWAY_ADDRESS did not match host:port or [ipv6]:port'
  printf 'GRID_GATEWAY_ADDRESS must be a host:port or [ipv6]:port value\n' >&2
  capture_failure 2
fi
[[ "$GRID_GATEWAY_DISCOVERY_ENABLED" == true || "$GRID_GATEWAY_DISCOVERY_ENABLED" == false ]] || {
  CURRENT_FAILURE='GRID_GATEWAY_DISCOVERY_ENABLED must be true or false'
  printf 'GRID_GATEWAY_DISCOVERY_ENABLED must be true or false\n' >&2
  capture_failure 2
}

CURRENT_PHASE=artifact-validation
observed_chart_sha="$(sha256sum "$CHART_FILE" | awk '{print $1}')"
[[ "$observed_chart_sha" == "$CHART_SHA256" ]] || {
  printf 'chart SHA mismatch: expected %s, observed %s\n' "$CHART_SHA256" "$observed_chart_sha" >&2
  exit 1
}
chart_metadata="$(helm show chart "$CHART_FILE")"
grep -Fq 'name: grid-operator' <<<"$chart_metadata" || {
  CURRENT_FAILURE='operator chart name does not match grid-operator'
  printf 'operator chart archive does not contain the expected grid-operator chart\n' >&2
  exit 1
}
grep -Fq "version: $CHART_VERSION" <<<"$chart_metadata"

write_values() {
  local grid_signals=''
  if [[ "$GRID_PRAXIS_SITE_ROUTER" == gridSiteRoute ]]; then grid_signals=poll; fi
  cat >"$EVIDENCE_DIR/operator-values.yaml" <<YAML
image:
  repository: $IMAGE_REPOSITORY
  tag: odh-stable
  digest: $IMAGE_DIGEST
  pullPolicy: Always
resourceNamespaces:
  - $RESOURCE_NAMESPACE
rbac:
  create: true
  enrollmentNamespace: ""
commonLabels:
  app.kubernetes.io/part-of: grid-llmd-e2e
  praxis.fast/qualification-run: "$RUN_LABEL"
enrollment:
  enabled: false
site:
  name: $GRID_SITE_NAME
  region: $GRID_SITE_REGION
  zone: "$GRID_SITE_ZONE"
grid:
  id: $GRID_ID
  network: $GRID_NETWORK_NAME
  signals: "$grid_signals"
  swimKeySecretName: "$GRID_SWIM_KEY_SECRET_NAME"
signals:
  enabled: false
  route:
    enabled: false
gateway:
  address: "$GRID_GATEWAY_ADDRESS"
  discoveryEnabled: $GRID_GATEWAY_DISCOVERY_ENABLED
  serviceName: $GRID_GATEWAY_SERVICE_NAME
  namespace: $GRID_GATEWAY_NAMESPACE
  port: "$GRID_GATEWAY_PORT"
  praxisConfig:
    generate: $GRID_PRAXIS_CONFIG_GENERATE
    configMapName: $GRID_PRAXIS_CONFIG_MAP_NAME
    listenerPort: $GRID_PRAXIS_CONFIG_LISTENER_PORT
    siteRouter: $GRID_PRAXIS_SITE_ROUTER
    clusterEndpoints: $GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON
YAML
  if [[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true ]]; then
    cat >>"$EVIDENCE_DIR/operator-values.yaml" <<YAML
    mountReconciliation:
      enabled: true
      deploymentName: $GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME
      containerName: $GRID_PRAXIS_CONFIG_CONTAINER_NAME
YAML
  fi
  cat >>"$EVIDENCE_DIR/operator-values.yaml" <<YAML
swim:
  requireKey: $GRID_SWIM_REQUIRE_KEY
  service:
    enabled: true
    type: ClusterIP
crds:
  enabled: true
  keep: true
resources:
  requests:
    cpu: $CPU_REQUEST
    memory: $MEMORY_REQUEST
  limits:
    cpu: $CPU_LIMIT
    memory: $MEMORY_LIMIT
YAML
}
write_values
export GRID_NETWORK_NAME GRID_RUN_LABEL GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON GRID_PROVIDER_HOP_ENDPOINTS_JSON GRID_TLS_SECRET_NAMESPACE

helm template "$RELEASE_NAME" "$CHART_FILE" --namespace "$RELEASE_NAMESPACE" \
  --values "$EVIDENCE_DIR/operator-values.yaml" --include-crds \
  --api-versions security.openshift.io/v1 --post-renderer "$POST_RENDERER" \
  >"$EVIDENCE_DIR/rendered-operator.yaml"
python3 "$SCRIPT_DIR/prepare-grid-operator-bootstrap-values.py" \
  "$EVIDENCE_DIR/operator-values.yaml" "$EVIDENCE_DIR/operator-bootstrap-values.yaml"
helm template "$RELEASE_NAME" "$CHART_FILE" --namespace "$RELEASE_NAMESPACE" \
  --values "$EVIDENCE_DIR/operator-bootstrap-values.yaml" --include-crds \
  --api-versions security.openshift.io/v1 >"$EVIDENCE_DIR/rendered-operator-bootstrap.yaml"
grid_crd_render_count="$(grep -c '^kind: CustomResourceDefinition$' "$EVIDENCE_DIR/rendered-operator-bootstrap.yaml" || true)"
[[ "$grid_crd_render_count" == 5 ]] || {
  printf 'bootstrap render must contain exactly five Grid CRDs\n' >&2
  exit 1
}
if grep -Eq '^kind: (GridNetwork|GridSite|InferenceProvider)$' "$EVIDENCE_DIR/rendered-operator-bootstrap.yaml"; then
  printf 'bootstrap render still contains Grid custom resources\n' >&2
  exit 1
fi
grep -Fq 'kind: GridNetwork' "$EVIDENCE_DIR/rendered-operator.yaml" || {
  printf 'full operator render omitted the task GridNetwork\n' >&2
  exit 1
}
grep -Fq 'kind: GridSite' "$EVIDENCE_DIR/rendered-operator.yaml" || {
  printf 'full operator render omitted the task GridSite\n' >&2
  exit 1
}
network_render="$(helm template "$RELEASE_NAME" "$CHART_FILE" --namespace "$RELEASE_NAMESPACE" \
  --values "$EVIDENCE_DIR/operator-values.yaml" --api-versions security.openshift.io/v1 \
  --show-only templates/gridnetwork.yaml --post-renderer "$POST_RENDERER")"
printf '%s\n' "$network_render" >"$EVIDENCE_DIR/rendered-gridnetwork.yaml"
grep -Fq 'routingPolicy: scoreFirst' <<<"$network_render"
grep -Fq 'strategy: queueDepth' <<<"$network_render"
grep -Fq 'mode: deterministic' <<<"$network_render"
grep -Fq 'metricsRefreshInterval: "10s"' <<<"$network_render"
if [[ -z "$GRID_SWIM_KEY_SECRET_NAME" ]]; then
  if grep -Eq '^[[:space:]]+swimKeySecretRef:' <<<"$network_render"; then
    printf 'single-site GridNetwork unexpectedly renders a SWIM key reference\n' >&2
    exit 1
  fi
else
  grep -Fq "name: \"$GRID_SWIM_KEY_SECRET_NAME\"" <<<"$network_render" || {
    printf 'GridNetwork did not render the configured SWIM key Secret name\n' >&2
    exit 1
  }
fi
if [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]]; then
  grep -Fq 'praxisConfig:' <<<"$network_render" || {
    printf 'GridNetwork render omitted configured praxisConfig generation\n' >&2
    exit 1
  }
  grep -Fq 'generate: true' <<<"$network_render" || {
    printf 'GridNetwork render did not enable praxisConfig generation\n' >&2
    exit 1
  }
  grep -Fq "configMapName: $GRID_PRAXIS_CONFIG_MAP_NAME" <<<"$network_render" || {
    printf 'GridNetwork render does not use the configured Praxis ConfigMap name\n' >&2
    exit 1
  }
  grep -Fq "listenerPort: $GRID_PRAXIS_CONFIG_LISTENER_PORT" <<<"$network_render" || {
    printf 'GridNetwork render does not use the configured Praxis listener port\n' >&2
    exit 1
  }
  if [[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true ]]; then
    grep -Fq 'mountReconciliation:' <<<"$network_render"
    grep -Fq 'enabled: true' <<<"$network_render"
    grep -Fq "deploymentName: $GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" <<<"$network_render"
    grep -Fq "containerName: $GRID_PRAXIS_CONFIG_CONTAINER_NAME" <<<"$network_render"
  fi
fi
if [[ -n "$GRID_PROVIDER_HOP_ENDPOINTS_JSON" ]]; then
  grep -Fq 'providerHopEndpoints:' <<<"$network_render" || {
    printf 'GridNetwork render omitted the verified mTLS provider-hop allowlist\n' >&2
    exit 1
  }
fi
if [[ "$GRID_PRAXIS_SITE_ROUTER" == gridSiteRoute ]]; then
  grep -Fq 'signalTransport:' <<<"$network_render" || {
    printf 'gridSiteRoute requires poll signal transport in the rendered GridNetwork\n' >&2
    exit 1
  }
  grep -Fq 'mode: "poll"' <<<"$network_render" || {
    printf 'gridSiteRoute requires signalTransport.mode=poll in the rendered GridNetwork\n' >&2
    exit 1
  }
  grep -Fq 'siteRouter: gridSiteRoute' <<<"$network_render" || {
    printf 'GridNetwork render does not select gridSiteRoute\n' >&2
    exit 1
  }
fi
chmod 0600 "$EVIDENCE_DIR/rendered-operator.yaml" "$EVIDENCE_DIR/operator-values.yaml" \
  "$EVIDENCE_DIR/rendered-operator-bootstrap.yaml" "$EVIDENCE_DIR/operator-bootstrap-values.yaml"
grep -Fq "image: $IMAGE" "$EVIDENCE_DIR/rendered-operator.yaml" || {
  printf 'rendered chart does not contain the expected image reference\n' >&2
  exit 1
}
grep -Fq "cpu: $CPU_REQUEST" "$EVIDENCE_DIR/rendered-operator.yaml"
grep -Fq "memory: $MEMORY_REQUEST" "$EVIDENCE_DIR/rendered-operator.yaml"
grep -Fq "cpu: $CPU_LIMIT" "$EVIDENCE_DIR/rendered-operator.yaml"
grep -Fq "memory: $MEMORY_LIMIT" "$EVIDENCE_DIR/rendered-operator.yaml"
sha256sum "$EVIDENCE_DIR/operator-values.yaml" "$EVIDENCE_DIR/rendered-operator.yaml" \
  "$EVIDENCE_DIR/operator-bootstrap-values.yaml" "$EVIDENCE_DIR/rendered-operator-bootstrap.yaml" \
  >"$EVIDENCE_DIR/render-sha256.txt"

if [[ "$MODE" == verify || "$MODE" == collect ]]; then
  CURRENT_PHASE=api-access
else
  CURRENT_PHASE=api-access
fi
context="$KUBE_CONTEXT"
user="$(oc whoami)"
server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 1; }
ready="$(oc get --raw=/readyz)"
[[ "$ready" == *ok* ]] || { printf 'API /readyz did not return ok\n' >&2; exit 1; }
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
if [[ "$insecure" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
    printf 'insecure kubeconfig rejected without the explicit ephemeral-dev opt-in\n' >&2
    exit 1
  }
  API_TLS_MODE=explicit-ephemeral-dev-unverified
else
  API_TLS_MODE=kubeconfig-verified
fi
cat >"$EVIDENCE_DIR/INVOCATION.md" <<EOF
# ODH Grid operator invocation

- Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- Context: $context
- Helm Kubernetes context: $KUBE_CONTEXT
- User: $user
- API server: $server
- API readiness: $ready
- API TLS mode: $API_TLS_MODE
- Run label: $RUN_LABEL
- ODH source revision: $SOURCE_REVISION
- Chart version: $CHART_VERSION
- Chart SHA-256: $observed_chart_sha
- Operator image pin: $IMAGE
- Resource profile: $RESOURCE_PROFILE
- Result label: $(if [[ "$RESOURCE_PROFILE" == reduced-dev ]]; then printf 'DEV-SMOKE / REDUCED-CAPACITY'; else printf 'STRICT / FULL-CAPACITY'; fi)
- Resource request/limit: CPU $CPU_REQUEST/$CPU_LIMIT; memory $MEMORY_REQUEST/$MEMORY_LIMIT
- Release: $RELEASE_NAME in $RELEASE_NAMESPACE
- Grid resource namespace: $RESOURCE_NAMESPACE
- Grid network/site identity: $GRID_NETWORK_NAME / $GRID_SITE_NAME ($GRID_ID)
- Grid Gateway Service: $GRID_GATEWAY_NAMESPACE/$GRID_GATEWAY_SERVICE_NAME:$GRID_GATEWAY_PORT
- Advertised Grid Gateway address: ${GRID_GATEWAY_ADDRESS:-service discovery only}
- Grid operator Praxis config generation: $GRID_PRAXIS_CONFIG_GENERATE
- Grid Praxis mount reconciliation: $GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION ($GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME/$GRID_PRAXIS_CONFIG_CONTAINER_NAME)
- GridNetwork TLS Secret namespace: ${GRID_TLS_SECRET_NAMESPACE:-chart default ($RELEASE_NAMESPACE)}
- Grid Praxis site router: $GRID_PRAXIS_SITE_ROUTER
- Praxis cluster endpoint inventory: $([[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]] && printf '%s entries' "$(jq 'length' "$EVIDENCE_DIR/operator-cluster-endpoints.json")" || printf deferred)
- Generated Praxis ConfigMap: $([[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]] && printf '%s/%s:%s' "$GRID_GATEWAY_NAMESPACE" "$GRID_PRAXIS_CONFIG_MAP_NAME" "$GRID_PRAXIS_CONFIG_LISTENER_PORT" || printf disabled)
- Grid SWIM key Secret reference: ${GRID_SWIM_KEY_SECRET_NAME:-omitted for single-site dev topology}
- Enrollment integration: disabled; no Enrollment Secret RBAC configured

No credential, token, kubeconfig data, Secret contents, private key, or certificate bytes are recorded.
EOF
printf 'context=%s\nuser=%s\nserver=%s\nreadyz=%s\napi_tls_mode=%s\n' \
  "$context" "$user" "$server" "$ready" "$API_TLS_MODE" >"$EVIDENCE_DIR/api-access-summary.txt"
chmod 0600 "$EVIDENCE_DIR/api-access-summary.txt"

ensure_owned_namespace() {
  local namespace="$1" owner
  if oc get namespace "$namespace" -o json >"$EVIDENCE_DIR/namespace-$namespace-before.json" 2>/dev/null; then
    owner="$(jq -r '.metadata.labels["praxis.fast/qualification-run"] // ""' "$EVIDENCE_DIR/namespace-$namespace-before.json")"
    [[ "$owner" == "$RUN_LABEL" ]] || {
      printf 'namespace %s already exists without this run ownership label; refusing adoption\n' "$namespace" >&2
      return 1
    }
  else
    oc create namespace "$namespace" -o json >"$EVIDENCE_DIR/namespace-$namespace-created.json"
    oc label namespace "$namespace" app.kubernetes.io/part-of=grid-llmd-e2e \
      app.kubernetes.io/managed-by=grid-llmd-e2e-run praxis.fast/qualification-run="$RUN_LABEL" \
      --overwrite=true >/dev/null
  fi
}

ensure_internal_image_pull_access() {
  local prefix='image-registry.openshift-image-registry.svc:5000/'
  local repository_path source_namespace binding_name binding_file existing_file
  [[ "$IMAGE_REPOSITORY" == "$prefix"* ]] || return 0

  repository_path="${IMAGE_REPOSITORY#"$prefix"}"
  source_namespace="${repository_path%%/*}"
  [[ -n "$source_namespace" && "$source_namespace" == "$RESOURCE_NAMESPACE" ]] || {
    printf 'integrated-registry image must be stored in the run-owned Grid resource namespace\n' >&2
    return 1
  }
  [[ "$source_namespace" != "$RELEASE_NAMESPACE" ]] || return 0

  binding_name="${RELEASE_NAME}-image-puller"
  binding_file="$EVIDENCE_DIR/$binding_name-rolebinding.yaml"
  existing_file="$EVIDENCE_DIR/$binding_name-rolebinding-before.json"
  if oc get rolebinding "$binding_name" -n "$source_namespace" -o json >"$existing_file" 2>/dev/null; then
    jq -e --arg run "$RUN_LABEL" --arg serviceAccount "$SERVICE_ACCOUNT_NAME" \
      --arg namespace "$RELEASE_NAMESPACE" '
        .metadata.labels["praxis.fast/qualification-run"]==$run and
        .roleRef.apiGroup=="rbac.authorization.k8s.io" and
        .roleRef.kind=="ClusterRole" and .roleRef.name=="system:image-puller" and
        any(.subjects[]?;.kind=="ServiceAccount" and .name==$serviceAccount and .namespace==$namespace)
      ' "$existing_file" >/dev/null || {
      printf 'existing image-puller RoleBinding is not owned by this run with the expected subject; refusing adoption\n' >&2
      return 1
    }
    return 0
  fi

  cat >"$binding_file" <<YAML
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: $binding_name
  namespace: $source_namespace
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
  annotations:
    praxis.fast/qualification-run-id: "$RUN_LABEL"
subjects:
- kind: ServiceAccount
  name: $SERVICE_ACCOUNT_NAME
  namespace: $RELEASE_NAMESPACE
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: system:image-puller
YAML
  chmod 0600 "$binding_file"
  CURRENT_PHASE=image-pull-rbac-dry-run
  oc apply --server-side --dry-run=server --field-manager=grid-llmd-e2e \
    -f "$binding_file" -o yaml >"$EVIDENCE_DIR/$binding_name-rolebinding-dry-run.yaml"
  CURRENT_PHASE=image-pull-rbac-apply
  oc apply --server-side --field-manager=grid-llmd-e2e -f "$binding_file" \
    >"$EVIDENCE_DIR/$binding_name-rolebinding-apply.txt"
  oc get rolebinding "$binding_name" -n "$source_namespace" -o json |
    jq -e --arg run "$RUN_LABEL" --arg serviceAccount "$SERVICE_ACCOUNT_NAME" \
      --arg namespace "$RELEASE_NAMESPACE" '
        .metadata.labels["praxis.fast/qualification-run"]==$run and
        .roleRef.name=="system:image-puller" and
        any(.subjects[]?;.kind=="ServiceAccount" and .name==$serviceAccount and .namespace==$namespace)
      ' >"$EVIDENCE_DIR/$binding_name-rolebinding-observed.json"
}

verify_runtime() {
  local deployment_json crd_json configured
  CURRENT_PHASE=operator-runtime-verification
  deployment_json="$EVIDENCE_DIR/operator-deployment.json"
  oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$deployment_json"
  configured="$(jq -r '[.spec.template.spec.containers[].image]|join("\n")' "$deployment_json")"
  [[ "$configured" == "$IMAGE" ]] || { printf 'configured image does not match the locked digest\n' >&2; return 1; }
  jq -e --arg cpu "$CPU_REQUEST" --arg memory "$MEMORY_REQUEST" \
    --arg cpuLimit "$CPU_LIMIT" --arg memoryLimit "$MEMORY_LIMIT" \
    '.spec.replicas==1 and all(.spec.template.spec.containers[]; .resources.requests.cpu==$cpu and .resources.requests.memory==$memory and .resources.limits.cpu==$cpuLimit and .resources.limits.memory==$memoryLimit)' \
    "$deployment_json" >/dev/null || { printf 'operator replicas or resource requests/limits differ from the pinned run values\n' >&2; return 1; }
  wait_for_operator_deployment "$deployment_json"
  wait_for_operator_pod_image
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,containers:[.spec.template.spec.containers[]|{name,image,resources}]},status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
    "$deployment_json" >"$EVIDENCE_DIR/operator-deployment-summary.json"
  oc get crd -o json | jq '[.items[]|select(.metadata.name|test("^(agenttoolproviders|gridnetworks|gridsites|inferenceproviders|siteinvites)\\.grid\\.praxis\\.fast$"))|{name:.metadata.name,established:any(.status.conditions[]?;.type=="Established" and .status=="True")}]' \
    >"$EVIDENCE_DIR/grid-crd-readiness.json"
  crd_json="$EVIDENCE_DIR/grid-crd-readiness.json"
  jq -e 'length==5 and all(.[];.established==true)' "$crd_json" >/dev/null || {
    printf 'one or more expected Grid CRDs are missing or not Established\n' >&2
    return 1
  }
  oc get validatingwebhookconfigurations,mutatingwebhookconfigurations -o json 2>/dev/null |
    jq --arg ns "$RELEASE_NAMESPACE" '[.items[]|{name:.metadata.name,kind:.kind,services:[.webhooks[]?.clientConfig.service?|select(.namespace==$ns)|{name,namespace,port,path}]}|select(.services|length>0)]' \
      >"$EVIDENCE_DIR/operator-webhook-configurations.json" || printf '[]\n' >"$EVIDENCE_DIR/operator-webhook-configurations.json"
  oc get svc,endpoints,endpointslices -n "$RELEASE_NAMESPACE" -o yaml >"$EVIDENCE_DIR/operator-services-endpoints.yaml"
  oc get serviceaccounts,role,rolebinding -n "$RELEASE_NAMESPACE" -o yaml >"$EVIDENCE_DIR/operator-namespaced-rbac.yaml"
  oc get clusterrole,clusterrolebinding -o json | jq '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]=="'"$RUN_LABEL"'")|{kind:.kind,name:.metadata.name,labels:.metadata.labels,subjects:.subjects}]' \
    >"$EVIDENCE_DIR/operator-cluster-rbac.json"
  oc logs deployment/"$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" --all-containers=true --tail=300 \
    >"$EVIDENCE_DIR/operator-logs.txt" 2>&1 || true
  oc get events -n "$RELEASE_NAMESPACE" --sort-by=.lastTimestamp >"$EVIDENCE_DIR/operator-events.txt" 2>&1 || true
  verify_praxis_config
}

verify_live_gridnetwork_scoring() {
  local network_json="$EVIDENCE_DIR/gridnetwork-scoring-live.json"
  CURRENT_PHASE=gridnetwork-scoring-live-verification
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json |
    jq '{metadata:{name:.metadata.name,generation:.metadata.generation,labels:.metadata.labels},spec:{routingPolicy:.spec.routingPolicy,scoringPolicy:.spec.scoringPolicy,selectionPolicy:.spec.selectionPolicy,metricsRefreshInterval:.spec.metricsRefreshInterval},status:{phase:.status.phase,routingMapStatus:.status.routingMapStatus}}' \
      >"$network_json"
  jq -e --arg network "$GRID_NETWORK_NAME" --arg run "$RUN_LABEL" '
    .metadata.name==$network and .metadata.labels["praxis.fast/qualification-run"]==$run and
    .spec.routingPolicy=="scoreFirst" and .spec.scoringPolicy.strategy=="queueDepth" and
    .spec.selectionPolicy.mode=="deterministic" and .spec.metricsRefreshInterval=="10s"
  ' "$network_json" >/dev/null || {
    printf 'live task GridNetwork does not match the rendered scoreFirst/queueDepth/deterministic/10s policy\n' >&2
    return 1
  }
}

wait_for_operator_deployment() {
  local deployment_json="$1" deadline
  deadline=$((SECONDS + RUNTIME_READY_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$deployment_json"
    if jq -e '.spec.replicas==1 and .status.observedGeneration==.metadata.generation and
      (.status.readyReplicas // 0)==1 and
      any(.status.conditions[]?; .type=="Available" and .status=="True")' \
      "$deployment_json" >/dev/null; then
      jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation},status:{observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
        "$deployment_json" >"$EVIDENCE_DIR/operator-deployment-readiness.json"
      return 0
    fi
    sleep 5
  done
  printf 'operator Deployment did not become Available for its current generation within %ss\n' \
    "$RUNTIME_READY_TIMEOUT_SECONDS" >&2
  return 1
}

wait_for_operator_pod_image() {
  local pod_json="$EVIDENCE_DIR/operator-pods.json" attempts_jsonl deadline
  attempts_jsonl="$EVIDENCE_DIR/operator-pod-readiness-attempts.jsonl"
  : >"$attempts_jsonl"
  deadline=$((SECONDS + RUNTIME_READY_TIMEOUT_SECONDS))
  while ((SECONDS < deadline)); do
    if oc get pods -n "$RELEASE_NAMESPACE" -l app.kubernetes.io/instance="$RELEASE_NAME" -o json |
      jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,phase:.status.phase,node:.spec.nodeName,conditions:.status.conditions,containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]' \
        >"$pod_json"; then
      jq -c --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
        '{observedAt:$observedAt,pods:[.[]|{name,phase,containers:[.containers[]|{image,imageID,ready,restartCount}]}]}' \
        "$pod_json" >>"$attempts_jsonl"
      if jq -e --arg image "$IMAGE" --arg digest "$IMAGE_DIGEST" \
        'length==1 and .[0].phase=="Running" and
        all(.[0].containers[];.ready==true and .image==$image and (.imageID|endswith($digest)))' \
        "$pod_json" >/dev/null; then
        return 0
      fi
    fi
    sleep 5
  done
  printf 'operator Pod did not become Ready on the exact pinned imageID within %ss; see readiness attempts\n' \
    "$RUNTIME_READY_TIMEOUT_SECONDS" >&2
  return 1
}

wait_for_praxis_config() {
  [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]] || return 0
  local network_json deadline
  deadline=$((SECONDS + ${GRID_PRAXIS_CONFIG_TIMEOUT_SECONDS:-300}))
  while ((SECONDS < deadline)); do
    if oc get gridnetwork "$GRID_NETWORK_NAME" -o json >"$EVIDENCE_DIR/gridnetwork-praxis-config.json" 2>/dev/null &&
      jq -e --arg gateway "$GRID_GATEWAY_SERVICE_NAME" \
        --arg namespace "$GRID_GATEWAY_NAMESPACE" --arg cm "$GRID_PRAXIS_CONFIG_MAP_NAME" \
        --arg deployment "$GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" \
        --argjson mountReconciliation "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" \
        '.metadata.generation as $generation |
         ([.status.praxisConfigStatus[]? | select(.gatewayName==$gateway and .namespace==$namespace and .configMapName==$cm and .phase=="Rendered" and .observedGeneration==$generation)] | length)==1 and
         (if $mountReconciliation then
           ([.status.mountReconciliationStatus[]? | select(.gatewayName==$gateway and .namespace==$namespace and .deploymentName==$deployment and .phase=="Ready" and .observedGeneration==$generation and .appliedRevision!="" and .reason=="")] | length)==1
          else true end)' \
        "$EVIDENCE_DIR/gridnetwork-praxis-config.json" >/dev/null; then
      return 0
    fi
    sleep 5
  done
  printf 'Grid operator did not render the current Praxis config and satisfy the configured mount-reconciliation gate for %s/%s before timeout\n' \
    "$GRID_GATEWAY_NAMESPACE" "$GRID_PRAXIS_CONFIG_MAP_NAME" >&2
  return 1
}

verify_praxis_config() {
  [[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]] || return 0
  local network_json config_summary deployment_json overlay_cm config_cm gateway_pod distributed_revision
  network_json="$EVIDENCE_DIR/gridnetwork-praxis-config.json"
  config_summary="$EVIDENCE_DIR/praxis-configmap-metadata.json"
  oc get gridnetwork "$GRID_NETWORK_NAME" -o json >"$network_json"
  jq -e --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
    --arg cm "$GRID_PRAXIS_CONFIG_MAP_NAME" \
    --arg deployment "$GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" \
    --arg container "$GRID_PRAXIS_CONFIG_CONTAINER_NAME" \
    --argjson mountReconciliation "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" \
    '.spec.consumerGateways | any(.[]; .name==$gateway and .namespace==$namespace and .praxisConfig.generate==true and .praxisConfig.configMapName==$cm and
      (if $mountReconciliation then .praxisConfig.mountReconciliation.enabled==true and .praxisConfig.mountReconciliation.deploymentName==$deployment and .praxisConfig.mountReconciliation.containerName==$container else true end))' \
    "$network_json" >/dev/null || { printf 'live GridNetwork does not declare the expected generated Praxis config\n' >&2; return 1; }
  jq -e --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
    --arg cm "$GRID_PRAXIS_CONFIG_MAP_NAME" \
    --arg deployment "$GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" \
    --argjson mountReconciliation "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" \
    '.metadata.generation as $generation |
     ([.status.praxisConfigStatus[]? | select(.gatewayName==$gateway and .namespace==$namespace and .configMapName==$cm and .phase=="Rendered" and .observedGeneration==$generation)] | length)==1 and
     (if $mountReconciliation then
       ([.status.mountReconciliationStatus[]? | select(.gatewayName==$gateway and .namespace==$namespace and .deploymentName==$deployment and .phase=="Ready" and .observedGeneration==$generation and .appliedRevision!="" and .reason=="")] | length)==1
      else true end)' \
    "$network_json" >/dev/null || { printf 'Praxis config status is not Rendered for the current GridNetwork generation\n' >&2; return 1; }
  deployment_json="$EVIDENCE_DIR/gateway-deployment-praxis-config.json"
  oc get deployment "$GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" -n "$GRID_GATEWAY_NAMESPACE" -o json >"$deployment_json"
  jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.generation==.status.observedGeneration and (.status.readyReplicas // 0)==1 and any(.status.conditions[]?;.type=="Available" and .status=="True")' "$deployment_json" >/dev/null || {
    printf 'task Praxis Gateway Deployment is not current and Available during operator config verification\n' >&2
    return 1
  }
  if [[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true ]]; then
    local gateway_deployment_generation
    gateway_deployment_generation="$(jq -er '.metadata.generation' "$deployment_json")"
    jq -e --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
      --arg deployment "$GRID_PRAXIS_CONFIG_DEPLOYMENT_NAME" --argjson generation "$gateway_deployment_generation" \
      '.status.mountReconciliationStatus[] | select(.gatewayName==$gateway and .namespace==$namespace and .deploymentName==$deployment and .phase=="Ready" and .deploymentGeneration==$generation and .appliedRevision!="" and .reason=="")' \
      "$network_json" >/dev/null || {
      printf 'Grid mount reconciliation is not Ready for the current Gateway Deployment generation\n' >&2
      return 1
    }
    jq -e --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" \
      '.metadata.annotations["grid.praxis-proxy.io/mount-reconciliation"]=="enabled" and
       .metadata.annotations["grid.praxis-proxy.io/network"]==$network and
       .metadata.annotations["grid.praxis-proxy.io/gateway"]==$gateway' \
      "$deployment_json" >/dev/null || {
      printf 'task Gateway Deployment is missing the chart-owned Grid mount-reconciliation opt-in annotations\n' >&2
      return 1
    }
  fi
  config_cm="$(jq -er --arg container "$GRID_PRAXIS_CONFIG_CONTAINER_NAME" '
    .spec.template.spec as $pod |
    ($pod.containers[] | select(.name==$container) | .volumeMounts[] | select(.mountPath=="/etc/praxis") | .name) as $volume |
    ($pod.volumes[] | select(.name==$volume) | .configMap.name)
  ' "$deployment_json")"
  local gateway_config_json
  gateway_config_json="$(oc get configmap "$config_cm" -n "$GRID_GATEWAY_NAMESPACE" -o json)"
  if [[ "$config_cm" != "$GRID_PRAXIS_CONFIG_MAP_NAME" ]]; then
    [[ "$GRID_PRAXIS_CONFIG_MOUNT_RECONCILIATION" == true ]] || {
      printf 'Praxis Gateway mounts an alternate config map while operator mount reconciliation is disabled\n' >&2
      return 1
    }
    jq -e --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" '
      .metadata.labels["app.kubernetes.io/managed-by"]=="grid-operator" and
      .metadata.labels["grid.praxis.fast/network"]==$network and
      .metadata.labels["grid.praxis.fast/gateway"]==$gateway and
      ((.data["praxis.yaml"] // "")|length)>0
    ' <<<"$gateway_config_json" >/dev/null || {
      printf 'Praxis Gateway alternate config map is not the matching Grid-operator-generated consumer config\n' >&2
      return 1
    }
  else
    jq -e --arg run "$RUN_LABEL" --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" '
      ((.metadata.labels["praxis.fast/qualification-run"]==$run) or
       (.metadata.labels["app.kubernetes.io/managed-by"]=="grid-operator" and
        .metadata.labels["grid.praxis.fast/network"]==$network and
        .metadata.labels["grid.praxis.fast/gateway"]==$gateway)) and
      ((.data["praxis.yaml"] // "")|length)>0
    ' <<<"$gateway_config_json" >/dev/null || {
      printf 'Praxis Gateway config map is not owned by this run or the matching Grid operator\n' >&2
      return 1
    }
  fi
  unset gateway_config_json
  overlay_cm="$(jq -er --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
    '.status.routingMapStatus[] | select(.gatewayName==$gateway and .namespace==$namespace and .phase=="Distributed" and .renderedRevision==.distributedRevision) | .configMapName' \
    "$network_json")"
  distributed_revision="$(jq -er --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
    '.status.routingMapStatus[] | select(.gatewayName==$gateway and .namespace==$namespace and .phase=="Distributed" and .renderedRevision==.distributedRevision) | .distributedRevision' \
    "$network_json")"
  jq -e --arg overlay "$overlay_cm" --arg container "$GRID_PRAXIS_CONFIG_CONTAINER_NAME" '
    .spec.template.spec as $pod |
    ($pod.volumes[] | select(.configMap.name==$overlay) | .name) as $volume |
    any($pod.containers[] | select(.name==$container) | .volumeMounts[]; .name==$volume and (.mountPath|startswith("/etc/praxis/routing")))
  ' "$deployment_json" >/dev/null || {
    printf 'Praxis Gateway does not mount the current distributed Grid routing overlay\n' >&2
    return 1
  }
  oc get configmap "$config_cm" -n "$GRID_GATEWAY_NAMESPACE" -o json |
    python3 -c 'import hashlib,json,re,sys; o=json.load(sys.stdin); d=o.get("data",{}); p=d.get("praxis.yaml",""); print(json.dumps({"name":o["metadata"]["name"],"namespace":o["metadata"]["namespace"],"labels":o["metadata"].get("labels",{}),"annotations":o["metadata"].get("annotations",{}),"keys":{k:{"length":len(v),"sha256":hashlib.sha256(v.encode()).hexdigest()} for k,v in sorted(d.items())},"overlayFile":"/etc/praxis/routing/routing-overlay.json" in p,"overlayReloadEnabled":bool(re.search(r"(?m)^[ \t]*reload:[ \t]*\r?\n[ \t]+enabled:[ \t]+true(?:[ \t]*$)",p))}))' \
    >"$config_summary"
  jq -e '.keys["praxis.yaml"].length > 0 and .overlayFile==true and .overlayReloadEnabled==true' "$config_summary" >/dev/null || {
    printf 'generated Praxis config does not read and reload the mounted routing overlay\n' >&2
    return 1
  }
  oc get configmap "$overlay_cm" -n "$GRID_GATEWAY_NAMESPACE" -o json |
    jq --arg revision "$distributed_revision" --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" \
      '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,annotations:.metadata.annotations,dataKeys:(.data|keys),revision:$revision,network:$network,gateway:$gateway}' \
      >"$EVIDENCE_DIR/gateway-routing-overlay-source.json"
  jq -e --arg revision "$distributed_revision" --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" \
    '.metadata.labels["grid.praxis.fast/network"]==$network and .metadata.labels["grid.praxis.fast/gateway"]==$gateway and .metadata.annotations["grid.praxis.fast/overlay-revision"]==$revision and ((.data["routing-overlay.json"] // "")|length)>0' \
    <(oc get configmap "$overlay_cm" -n "$GRID_GATEWAY_NAMESPACE" -o json) >/dev/null || {
    printf 'live routing overlay ConfigMap metadata/revision does not match Grid status\n' >&2
    return 1
  }
  local projection_attempts projection_current projection_deadline projection_reason mounted_revision gateway_selector
  projection_attempts="$EVIDENCE_DIR/gateway-overlay-projection-attempts.jsonl"
  projection_current="$EVIDENCE_DIR/gateway-mounted-overlay-current.json"
  : >"$projection_attempts"
  projection_deadline=$((SECONDS + ${GRID_PRAXIS_CONFIG_TIMEOUT_SECONDS:-300}))
  projection_reason='Grid overlay projection has not been observed yet'
  mounted_revision=''
  while ((SECONDS < projection_deadline)); do
    if ! oc get gridnetwork "$GRID_NETWORK_NAME" -o json >"$network_json" 2>/dev/null; then
      projection_reason='GridNetwork could not be read while waiting for overlay projection'
    elif ! jq -e --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
      '.metadata.generation as $generation |
       any(.status.routingMapStatus[]?; .gatewayName==$gateway and .namespace==$namespace and .phase=="Distributed" and .observedGeneration==$generation and .renderedRevision==.distributedRevision and .distributedRevision!="")' \
      "$network_json" >/dev/null; then
      projection_reason='GridNetwork routing map is not Distributed at the current generation'
    else
      overlay_cm="$(jq -er --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
        '.status.routingMapStatus[] | select(.gatewayName==$gateway and .namespace==$namespace and .phase=="Distributed" and .renderedRevision==.distributedRevision) | .configMapName' \
        "$network_json")"
      distributed_revision="$(jq -er --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
        '.status.routingMapStatus[] | select(.gatewayName==$gateway and .namespace==$namespace and .phase=="Distributed" and .renderedRevision==.distributedRevision) | .distributedRevision' \
        "$network_json")"
      gateway_selector="$(jq -er '.spec.selector.matchLabels|to_entries|map("\(.key)=\(.value)")|join(",")' "$deployment_json")"
      gateway_pod="$(oc get pods -n "$GRID_GATEWAY_NAMESPACE" -l "$gateway_selector" -o json |
        jq -r '[.items[]|select(.status.phase=="Running" and any(.status.containerStatuses[]?;.name=="praxis" and .ready==true))][0].metadata.name // empty' 2>/dev/null || true)"
      if [[ -z "$gateway_pod" ]]; then
        projection_reason='No ready task Gateway Pod was available to inspect the mounted overlay'
      elif oc exec "$gateway_pod" -n "$GRID_GATEWAY_NAMESPACE" -c "$GRID_PRAXIS_CONFIG_CONTAINER_NAME" -- cat /etc/praxis/routing/routing-overlay.json 2>/dev/null |
        jq -ce --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
          '{revision:.revision.value,scope:.scope,candidates:[.overlay.candidates[]|{name,cluster,site,score,rank,score_breakdown}]}' \
          >"$projection_current"; then
        mounted_revision="$(jq -r '.revision // empty' "$projection_current")"
        if jq -e --arg revision "$distributed_revision" --arg network "$GRID_NETWORK_NAME" --arg gateway "$GRID_GATEWAY_SERVICE_NAME" --arg namespace "$GRID_GATEWAY_NAMESPACE" \
          '.revision==$revision and .scope.network==$network and .scope.gateway==$gateway and .scope.namespace==$namespace and
           (.candidates|length==2) and ([.candidates[].cluster]|unique|length)==2' \
          "$projection_current" >/dev/null; then
          jq -n --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg pod "$gateway_pod" \
            --arg expected "$distributed_revision" --arg mounted "$mounted_revision" \
            '{observedAt:$observedAt,result:"matched",gatewayPod:$pod,expectedRevision:$expected,mountedRevision:$mounted}' \
            >>"$projection_attempts"
          mv "$projection_current" "$EVIDENCE_DIR/gateway-mounted-overlay-summary.json"
          jq -e '(.candidates|length)==2 and ([.candidates[].cluster]|unique|length)==2' \
            "$EVIDENCE_DIR/gateway-mounted-overlay-summary.json" >/dev/null || return 1
          log "operator-generated Praxis config and mounted Grid overlay match current distributed revision $distributed_revision after bounded projection wait; raw config and overlay values were not recorded"
          return 0
        fi
        projection_reason="Gateway Pod has not projected the current overlay revision (mounted=${mounted_revision:-unknown}, distributed=$distributed_revision) or the two-candidate scope is incomplete"
      else
        projection_reason='Gateway Pod overlay file is not yet readable as the expected JSON contract'
      fi
    fi
    jq -n --arg observedAt "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --arg pod "${gateway_pod:-}" \
      --arg expected "${distributed_revision:-}" --arg mounted "${mounted_revision:-}" --arg reason "$projection_reason" \
      '{observedAt:$observedAt,result:"waiting",gatewayPod:$pod,expectedRevision:$expected,mountedRevision:$mounted,reason:$reason}' \
      >>"$projection_attempts"
    sleep 5
  done
  CURRENT_FAILURE="Gateway overlay projection did not converge before timeout: $projection_reason"
  printf '%s\n' "$CURRENT_FAILURE" >&2
  return 1
}

capture_grid_crds() {
  local destination="$1"
  oc get crd -o json | jq '[.items[]|select(.metadata.name|IN("agenttoolproviders.grid.praxis.fast","gridnetworks.grid.praxis.fast","gridsites.grid.praxis.fast","inferenceproviders.grid.praxis.fast","siteinvites.grid.praxis.fast"))|{name:.metadata.name,established:any(.status.conditions[]?;.type=="Established" and .status=="True"),release:.metadata.annotations["meta.helm.sh/release-name"],releaseNamespace:.metadata.annotations["meta.helm.sh/release-namespace"]}]' \
    >"$destination"
}

verify_grid_crds_owned() {
  local snapshot="$1"
  jq -e --arg release "$RELEASE_NAME" --arg namespace "$RELEASE_NAMESPACE" \
    'length==5 and all(.[];.established==true and .release==$release and .releaseNamespace==$namespace)' \
    "$snapshot" >/dev/null || {
      printf 'all five Grid CRDs must be Established and owned by this Helm release\n' >&2
      return 1
    }
}

case "$MODE" in
  render)
    CURRENT_PHASE=ownership-preflight
    helm status "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/helm-status-before.json" || {
      printf 'render mode requires the existing run-owned Grid operator release; it never creates namespaces or installs resources\n' >&2
      exit 1
    }
    helm get values "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" --all -o yaml >"$EVIDENCE_DIR/helm-values-before.yaml"
    helm get manifest "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" >"$EVIDENCE_DIR/helm-manifest-before.yaml"
    diff -u "$EVIDENCE_DIR/helm-manifest-before.yaml" "$EVIDENCE_DIR/rendered-operator.yaml" \
      >"$EVIDENCE_DIR/operator-manifest-change.patch" || true
    oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-deployment-before.json"
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
      "$EVIDENCE_DIR/operator-deployment-before.json" >/dev/null || {
        printf 'existing operator Deployment is not owned by this qualification run\n' >&2
        exit 1
      }
    jq -e --arg image "$IMAGE" 'all(.spec.template.spec.containers[];.image==$image)' \
      "$EVIDENCE_DIR/operator-deployment-before.json" >/dev/null || {
        printf 'render mode refuses a different existing operator image\n' >&2
        exit 1
      }
    capture_grid_crds "$EVIDENCE_DIR/grid-crds-before.json"
    verify_grid_crds_owned "$EVIDENCE_DIR/grid-crds-before.json"
    for namespace in "$RELEASE_NAMESPACE" "$RESOURCE_NAMESPACE"; do
      oc get namespace "$namespace" -o json >"$EVIDENCE_DIR/namespace-$namespace-before.json"
      jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
        "$EVIDENCE_DIR/namespace-$namespace-before.json" >/dev/null || {
          printf 'render mode refuses a namespace outside this qualification run: %s\n' "$namespace" >&2
          exit 1
        }
    done
    CURRENT_PHASE='server-dry-run'
    oc apply --dry-run=server -f "$EVIDENCE_DIR/rendered-operator.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
    jq -n --arg evidence "$EVIDENCE_DIR" --arg source "$SOURCE_REVISION" --arg image "$IMAGE" \
      --arg profile "$RESOURCE_PROFILE" --arg cpu "$CPU_REQUEST" \
      '{mode:"render",status:"rendered",clusterMutated:false,inferenceTrafficSent:false,evidenceDirectory:$evidence,sourceRevision:$source,image:$image,resourceProfile:$profile,operatorCpuRequest:$cpu}' \
      >"$EVIDENCE_DIR/RESULT.json"
    log "Grid operator render and server-side dry-run passed; no cluster resources changed"
    printf 'Evidence: %s\n' "$EVIDENCE_DIR"
    exit 0
    ;;
  install)
    CURRENT_PHASE=ownership-preflight
    GRID_FIRST_INSTALL=false
    if helm status "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" >/dev/null 2>&1; then
      helm status "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/helm-status-before.json"
      helm get values "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" --all -o yaml >"$EVIDENCE_DIR/helm-values-before.yaml"
      helm get manifest "$RELEASE_NAME" -n "$RELEASE_NAMESPACE" >"$EVIDENCE_DIR/helm-manifest-before.yaml"
      diff -u "$EVIDENCE_DIR/helm-manifest-before.yaml" "$EVIDENCE_DIR/rendered-operator.yaml" \
        >"$EVIDENCE_DIR/operator-manifest-change.patch" || true
      oc get deployment "$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" -o json >"$EVIDENCE_DIR/operator-deployment-before.json"
      jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
        "$EVIDENCE_DIR/operator-deployment-before.json" >/dev/null || {
          printf 'Helm release already exists; verify/collect it before changing ownership or values\n' >&2
          exit 1
        }
      jq -e --arg image "$IMAGE" 'all(.spec.template.spec.containers[];.image==$image)' \
        "$EVIDENCE_DIR/operator-deployment-before.json" >/dev/null || {
          printf 'existing task-owned Helm release uses a different operator image; refusing in-place replacement\n' >&2
          exit 1
        }
      capture_grid_crds "$EVIDENCE_DIR/grid-crds-before.json"
      verify_grid_crds_owned "$EVIDENCE_DIR/grid-crds-before.json"
      ensure_owned_namespace "$RELEASE_NAMESPACE"
      ensure_owned_namespace "$RESOURCE_NAMESPACE"
      ensure_internal_image_pull_access
      CURRENT_PHASE='server-dry-run'
      oc apply --dry-run=server -f "$EVIDENCE_DIR/rendered-operator.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
    else
      capture_grid_crds "$EVIDENCE_DIR/grid-crds-before.json"
      grid_crd_count="$(jq 'length' "$EVIDENCE_DIR/grid-crds-before.json")"
      if ((grid_crd_count != 0)); then
        printf 'Grid CRDs exist without the task Helm release; refusing to adopt or overwrite them\n' >&2
        exit 1
      fi
      ensure_owned_namespace "$RELEASE_NAMESPACE"
      ensure_owned_namespace "$RESOURCE_NAMESPACE"
      ensure_internal_image_pull_access
      CURRENT_PHASE='server-dry-run'
      oc apply --dry-run=server -f "$EVIDENCE_DIR/rendered-operator-bootstrap.yaml" \
        >"$EVIDENCE_DIR/server-dry-run-bootstrap.txt"
      printf 'First Helm stage contains the five CRDs and built-in resources with grid.id empty; GridNetwork/GridSite/InferenceProvider are enabled only after CRD establishment.\n' \
        >"$EVIDENCE_DIR/server-dry-run-decision.md"
      GRID_FIRST_INSTALL=true
    fi
    {
      if [[ "$GRID_FIRST_INSTALL" == true ]]; then
        printf 'helm --kube-context=%q upgrade --install %q %q --namespace %q --values %q --wait --timeout %q\n' \
          "$KUBE_CONTEXT" "$RELEASE_NAME" "$CHART_FILE" "$RELEASE_NAMESPACE" "$EVIDENCE_DIR/operator-bootstrap-values.yaml" "$TIMEOUT"
        for crd in agenttoolproviders.grid.praxis.fast gridnetworks.grid.praxis.fast gridsites.grid.praxis.fast inferenceproviders.grid.praxis.fast siteinvites.grid.praxis.fast; do
          printf 'oc wait --for=condition=Established crd/%s --timeout=%q\n' "$crd" "$TIMEOUT"
        done
        printf 'helm --kube-context=%q upgrade --install %q %q --namespace %q --values %q --wait --timeout %q\n' \
          "$KUBE_CONTEXT" "$RELEASE_NAME" "$CHART_FILE" "$RELEASE_NAMESPACE" "$EVIDENCE_DIR/operator-values.yaml" "$TIMEOUT"
      else
        printf 'helm --kube-context=%q upgrade --install %q %q --namespace %q --values %q --wait --timeout %q\n' \
          "$KUBE_CONTEXT" "$RELEASE_NAME" "$CHART_FILE" "$RELEASE_NAMESPACE" "$EVIDENCE_DIR/operator-values.yaml" "$TIMEOUT"
      fi
    } >"$EVIDENCE_DIR/replay-command.txt"
    if [[ "$GRID_FIRST_INSTALL" == true ]]; then
      CURRENT_PHASE=helm-crd-bootstrap
      helm upgrade --install "$RELEASE_NAME" "$CHART_FILE" --namespace "$RELEASE_NAMESPACE" \
        --values "$EVIDENCE_DIR/operator-bootstrap-values.yaml" --wait --timeout "$TIMEOUT" \
        >"$EVIDENCE_DIR/helm-crd-bootstrap.log" 2>&1
      for crd in agenttoolproviders.grid.praxis.fast gridnetworks.grid.praxis.fast gridsites.grid.praxis.fast inferenceproviders.grid.praxis.fast siteinvites.grid.praxis.fast; do
        oc wait --for=condition=Established "crd/$crd" --timeout="$TIMEOUT" \
          >"$EVIDENCE_DIR/wait-${crd%%.*}.txt"
      done
      capture_grid_crds "$EVIDENCE_DIR/grid-crds-after-bootstrap.json"
      verify_grid_crds_owned "$EVIDENCE_DIR/grid-crds-after-bootstrap.json"
      oc get gridnetwork.grid.praxis.fast --ignore-not-found=true -o name \
        >"$EVIDENCE_DIR/gridnetwork-api-discovery.txt"
      oc get gridsite.grid.praxis.fast --ignore-not-found=true -o name \
        >"$EVIDENCE_DIR/gridsite-api-discovery.txt"
      oc get inferenceprovider.grid.praxis.fast --ignore-not-found=true -o name \
        >"$EVIDENCE_DIR/inferenceprovider-api-discovery.txt"
      CURRENT_PHASE='server-dry-run'
      oc apply --dry-run=server -f "$EVIDENCE_DIR/rendered-operator.yaml" \
        >"$EVIDENCE_DIR/server-dry-run-after-crds.txt"
    fi
    CURRENT_PHASE=helm-install
    helm upgrade --install "$RELEASE_NAME" "$CHART_FILE" --namespace "$RELEASE_NAMESPACE" \
      --values "$EVIDENCE_DIR/operator-values.yaml" --post-renderer "$POST_RENDERER" \
      --wait --timeout "$TIMEOUT" \
      >"$EVIDENCE_DIR/helm-install.log" 2>&1
    oc wait deployment/"$DEPLOYMENT_NAME" -n "$RELEASE_NAMESPACE" --for=condition=Available --timeout="$TIMEOUT" \
      >"$EVIDENCE_DIR/deployment-wait.txt"
    wait_for_praxis_config
    verify_runtime
    verify_live_gridnetwork_scoring
    ;;
  verify|collect)
    verify_runtime
    if [[ "$MODE" == verify ]]; then verify_live_gridnetwork_scoring; fi
    ;;
esac

helm list -n "$RELEASE_NAMESPACE" --filter "^$RELEASE_NAME$" -o json |
  jq -e --arg name "$RELEASE_NAME" --arg namespace "$RELEASE_NAMESPACE" \
    '[.[]|select(.name==$name and .namespace==$namespace)|{name,namespace,revision,status,chart,appVersion:.app_version,updated}] as $releases | if ($releases|length)==1 and $releases[0].status=="deployed" then $releases[0] else error("expected exactly one deployed Helm release") end' \
    >"$EVIDENCE_DIR/helm-status.json"
cat >"$EVIDENCE_DIR/source-lock.md" <<EOF
# ODH Grid operator deployment lock

- ODH source revision: $SOURCE_REVISION
- Chart version: $CHART_VERSION
- Chart archive: $CHART_FILE
- Chart SHA-256: $observed_chart_sha
- Operator image: $IMAGE
- Resource profile: $RESOURCE_PROFILE (CPU request $CPU_REQUEST; strict rollback CPU request 50m)
- Run label: $RUN_LABEL
- Release: $RELEASE_NAME in $RELEASE_NAMESPACE
- Grid resource namespace: $RESOURCE_NAMESPACE
- Grid operator Praxis config generation: $GRID_PRAXIS_CONFIG_GENERATE
- GridNetwork scoring policy: routingPolicy=scoreFirst, scoringPolicy.strategy=queueDepth, selectionPolicy.mode=deterministic, metricsRefreshInterval=10s
- GridNetwork scoring post-renderer SHA-256: $(sha256sum "$POST_RENDERER" | awk '{print $1}')
- Rendered task GridNetwork SHA-256: $(sha256sum "$EVIDENCE_DIR/rendered-gridnetwork.yaml" | awk '{print $1}')
- Generated Praxis ConfigMap: $([[ "$GRID_PRAXIS_CONFIG_GENERATE" == true ]] && printf '%s/%s:%s' "$GRID_GATEWAY_NAMESPACE" "$GRID_PRAXIS_CONFIG_MAP_NAME" "$GRID_PRAXIS_CONFIG_LISTENER_PORT" || printf disabled)
- Render SHA-256: $(sha256sum "$EVIDENCE_DIR/rendered-operator.yaml" | awk '{print $1}')
- Values SHA-256: $(sha256sum "$EVIDENCE_DIR/operator-values.yaml" | awk '{print $1}')
- Bootstrap render SHA-256: $(sha256sum "$EVIDENCE_DIR/rendered-operator-bootstrap.yaml" | awk '{print $1}')
- Bootstrap values SHA-256: $(sha256sum "$EVIDENCE_DIR/operator-bootstrap-values.yaml" | awk '{print $1}')

No Enrollment Secret or credential data was captured. This operator install does not qualify Enrollment.
EOF
jq -n --arg mode "$MODE" --arg status ready --arg evidence "$EVIDENCE_DIR" --arg resourceProfile "$RESOURCE_PROFILE" \
  --arg image "$IMAGE" --arg source "$SOURCE_REVISION" \
  '{mode:$mode,status:$status,evidenceDirectory:$evidence,resourceProfile:$resourceProfile,resultLabel:(if $resourceProfile=="reduced-dev" then "DEV-SMOKE / REDUCED-CAPACITY" else "STRICT / FULL-CAPACITY" end),image:$image,sourceRevision:$source,inferenceTrafficSent:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
log "ODH Grid operator $MODE complete: source=$SOURCE_REVISION image=$IMAGE; no inference traffic sent"
printf 'Evidence: %s\n' "$EVIDENCE_DIR"
