#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Deploy the task-owned ExtProc service and attach it to one task Gateway.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${GRID_EXTPROC_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
GATEWAY_NAME="${GRID_API_GATEWAY_NAME:-grid-llmd-ingress}"
SERVICE_NAME="${GRID_EXTPROC_SERVICE_NAME:-praxis-extproc}"
CONFIGMAP_NAME="${GRID_EXTPROC_CONFIGMAP_NAME:-praxis-extproc-config}"
DEPLOYMENT_NAME="${GRID_EXTPROC_DEPLOYMENT_NAME:-praxis-extproc}"
ENVOYFILTER_NAME="${GRID_EXTPROC_ENVOYFILTER_NAME:-praxis-grid-llmd-extproc}"
IMAGE="${GRID_EXTPROC_IMAGE:-}"
SOURCE_REVISION="${GRID_EXTPROC_SOURCE_REVISION:-}"
SOURCE_LOCKS_FILE="${SOURCE_LOCKS_FILE:-}"
IMAGE_PROVENANCE_FILE="${GRID_EXTPROC_IMAGE_PROVENANCE_FILE:-}"
EVIDENCE_DIR="${GRID_EXTPROC_EVIDENCE_DIR:-}"
TIMEOUT="${GRID_EXTPROC_TIMEOUT:-5m}"

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Task-owned Praxis ExtProc plan (plan mode makes no cluster calls):
  1. Verify the source lock, image provenance, API target, run-owned namespace, and task-owned Gateway.
  2. Render a single ExtProc Deployment/Service/ConfigMap and a Gateway-specific EnvoyFilter.
  3. Server-side dry-run all four objects before any write.
  4. Install only the task-owned resources and wait for the exact image digest.
  5. Read the selected Envoy proxy config dump and require both the ext_proc HTTP filter and its gRPC cluster.

The ExtProc filter fails closed, sends request/response headers, and buffers the
bounded qualification request body for its SHA-256 oracle. The service uses
in-namespace cleartext h2c; external application TLS and GridSite CA
verification remain enabled. No inference request is sent.
Modes: plan, render, install, verify, collect. API TLS bypass requires the explicit ephemeral-dev opt-in.
PLAN
  exit 0
fi

for bin in oc jq python3 sha256sum; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
[[ "$MODE" =~ ^(render|install|verify|collect)$ ]] || {
  printf 'unsupported GRID_EXTPROC_MODE: %s\n' "$MODE" >&2
  exit 2
}
[[ -n "$EXPECTED_API_SERVER" && -n "$RUN_LABEL" && -n "$KUBE_CONTEXT" ]] || {
  printf 'EXPECTED_API_SERVER, GRID_RUN_LABEL, and explicit KUBE_CONTEXT are required\n' >&2
  exit 2
}
[[ "$IMAGE" =~ ^(.+)@(sha256:[0-9a-f]{64})$ ]] || {
  printf 'GRID_EXTPROC_IMAGE must be a repository@sha256:<64 lowercase hex> reference\n' >&2
  exit 2
}
IMAGE_DIGEST="${BASH_REMATCH[2]}"
[[ "$SOURCE_REVISION" =~ ^[0-9a-f]{40}$ ]] || {
  printf 'GRID_EXTPROC_SOURCE_REVISION must be a full Git SHA\n' >&2
  exit 2
}
[[ -s "$SOURCE_LOCKS_FILE" && -s "$IMAGE_PROVENANCE_FILE" ]] || {
  printf 'SOURCE_LOCKS_FILE and GRID_EXTPROC_IMAGE_PROVENANCE_FILE are required\n' >&2
  exit 2
}
jq -e --arg sha "$SOURCE_REVISION" \
  '.components.extproc.resolved_sha==$sha and .components.extproc.resolved_worktree_clean==true' \
  "$SOURCE_LOCKS_FILE" >/dev/null || {
  printf 'ExtProc source SHA does not match a clean resolved source lock\n' >&2
  exit 2
}
jq -e --arg sha "$SOURCE_REVISION" --arg image "$IMAGE" \
  '.verified==true and .source_sha==$sha and .image==$image and .oci_revision_label==$sha' \
  "$IMAGE_PROVENANCE_FILE" >/dev/null || {
  printf 'ExtProc image is not verified against the locked source SHA and immutable image reference\n' >&2
  exit 2
}
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || {
  printf 'GRID_RUN_LABEL is not a Kubernetes label value\n' >&2
  exit 2
}
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="$REPO_ROOT/evidence/grid-extproc-$(date -u +%Y%m%dT%H%M%SZ)"
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
  trap - ERR
  {
    printf '# Praxis ExtProc deployment attempt\n\n'
    printf -- '- Failed phase: `%s`\n' "${CURRENT_PHASE:-initialization}"
    printf -- '- Failure detail: `%s`\n' "${CURRENT_FAILURE:-see bounded phase evidence}"
    printf -- '- Exit code: `%s`\n' "$code"
    printf -- '- API server: `%s`\n' "$EXPECTED_API_SERVER"
    printf -- '- Namespace/Gateway: `%s/%s`\n' "$NAMESPACE" "$GATEWAY_NAME"
    printf -- '- Image/source: `%s` / `%s`\n' "$IMAGE" "$SOURCE_REVISION"
    printf -- '- Evidence: `%s`\n' "$EVIDENCE_DIR"
    printf '\nNo credential values, Secret data, or inference request were captured/sent.\n'
  } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  jq -n --arg mode "$MODE" --arg phase "${CURRENT_PHASE:-initialization}" \
    --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:"blocked",phase:$phase,evidenceDirectory:$evidence,exitCode:$exitCode,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json" 2>/dev/null || true
  log "Praxis ExtProc $MODE stopped with exit=$code at phase=${CURRENT_PHASE:-initialization}"
  exit "$code"
}
trap 'capture_failure $?' ERR

CURRENT_PHASE=api-access
context="$KUBE_CONTEXT"
user="$(oc whoami)"
server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || {
  CURRENT_FAILURE='current oc context points at a different API server'
  printf 'API server mismatch\n' >&2
  exit 1
}
ready="$(oc get --raw=/readyz)"
[[ "$ready" == *ok* ]] || {
  CURRENT_FAILURE='API /readyz did not return ok'
  printf 'API /readyz did not return ok\n' >&2
  exit 1
}
insecure="$(command oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
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
  CURRENT_FAILURE='ExtProc namespace is not owned by this run'
  printf 'ExtProc namespace ownership mismatch\n' >&2
  exit 1
}
oc get gateway "$GATEWAY_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gateway.json"
jq -e --arg run "$RUN_LABEL" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  .metadata.generation as $g |
  all(.status.conditions[]?|select(.type=="Accepted" or .type=="Programmed"); .status=="True" and .observedGeneration==$g)
' "$EVIDENCE_DIR/gateway.json" >/dev/null || {
  CURRENT_FAILURE='task Gateway is not current-generation Accepted and Programmed'
  printf 'task Gateway ownership/readiness gate failed\n' >&2
  exit 1
}

for resource in "configmap/$CONFIGMAP_NAME" "deployment/$DEPLOYMENT_NAME" "service/$SERVICE_NAME" "envoyfilter.networking.istio.io/$ENVOYFILTER_NAME"; do
  kind="${resource%%/*}"
  name="${resource#*/}"
  if oc get "$kind" "$name" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/existing-${kind//\//-}-$name.json" 2>/dev/null; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
      "$EVIDENCE_DIR/existing-${kind//\//-}-$name.json" >/dev/null || {
      CURRENT_FAILURE="$resource exists without matching task-run ownership"
      printf '%s exists outside this run ownership\n' "$resource" >&2
      exit 1
    }
  fi
done

CURRENT_PHASE=render-review
cat >"$EVIDENCE_DIR/extproc-resources.yaml" <<YAML
apiVersion: v1
kind: ConfigMap
metadata:
  name: $CONFIGMAP_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/name: praxis-extproc
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
  annotations:
    e2e.praxis.fast/source-revision: "$SOURCE_REVISION"
data:
  extproc.yaml: |
    server:
      grpc_address: "0.0.0.0:9004"
      health_address: "0.0.0.0:50052"
      metrics_address: "0.0.0.0:9090"
      tls:
        mode: none
    filter_chains:
      - name: grid-llmd-smoke
        filters:
          - filter: headers
            response_set:
              - name: X-Praxis-ExtProc-Response
                value: "$RUN_LABEL"
          - filter: e2e_body_oracle
YAML
cat >>"$EVIDENCE_DIR/extproc-resources.yaml" <<YAML
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: $DEPLOYMENT_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/name: praxis-extproc
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
  annotations:
    e2e.praxis.fast/source-revision: "$SOURCE_REVISION"
spec:
  replicas: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: praxis-extproc
      praxis.fast/qualification-run: "$RUN_LABEL"
  template:
    metadata:
      labels:
        app.kubernetes.io/name: praxis-extproc
        app.kubernetes.io/part-of: grid-llmd-e2e
        praxis.fast/qualification-run: "$RUN_LABEL"
    spec:
      automountServiceAccountToken: false
      terminationGracePeriodSeconds: 30
      securityContext:
        runAsNonRoot: true
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: praxis-extproc
          image: "$IMAGE"
          imagePullPolicy: IfNotPresent
          args: ["-c", "/etc/praxis/extproc.yaml"]
          ports:
            - name: grpc
              containerPort: 9004
              protocol: TCP
            - name: health
              containerPort: 50052
              protocol: TCP
            - name: metrics
              containerPort: 9090
              protocol: TCP
          resources:
            requests:
              cpu: 50m
              memory: 64Mi
            limits:
              cpu: 500m
              memory: 256Mi
          securityContext:
            # Let the OpenShift restricted SCC inject the namespace UID. A
            # fixed UID outside the namespace range prevents Pod creation.
            allowPrivilegeEscalation: false
            readOnlyRootFilesystem: true
            capabilities:
              drop: ["ALL"]
            seccompProfile:
              type: RuntimeDefault
          livenessProbe:
            tcpSocket:
              port: health
            initialDelaySeconds: 15
            periodSeconds: 20
          readinessProbe:
            grpc:
              port: 50052
              service: envoy.service.ext_proc.v3.ExternalProcessor
            initialDelaySeconds: 5
            periodSeconds: 10
          volumeMounts:
            - name: config
              mountPath: /etc/praxis
              readOnly: true
      volumes:
        - name: config
          configMap:
            name: $CONFIGMAP_NAME
            items:
              - key: extproc.yaml
                path: extproc.yaml
---
apiVersion: v1
kind: Service
metadata:
  name: $SERVICE_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/name: praxis-extproc
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  type: ClusterIP
  selector:
    app.kubernetes.io/name: praxis-extproc
    praxis.fast/qualification-run: "$RUN_LABEL"
  ports:
    - name: grpc
      appProtocol: HTTP2
      protocol: TCP
      port: 9004
      targetPort: grpc
---
apiVersion: networking.istio.io/v1alpha3
kind: EnvoyFilter
metadata:
  name: $ENVOYFILTER_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/name: praxis-extproc
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    praxis.fast/qualification-run: "$RUN_LABEL"
  annotations:
    e2e.praxis.fast/source-revision: "$SOURCE_REVISION"
spec:
  workloadSelector:
    labels:
      gateway.networking.k8s.io/gateway-name: "$GATEWAY_NAME"
  configPatches:
    - applyTo: CLUSTER
      match:
        context: GATEWAY
      patch:
        operation: ADD
        value:
          name: praxis-extproc-grpc
          type: STRICT_DNS
          connect_timeout: 5s
          typed_extension_protocol_options:
            envoy.extensions.upstreams.http.v3.HttpProtocolOptions:
              "@type": type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions
              explicit_http_config:
                http2_protocol_options: {}
          load_assignment:
            cluster_name: praxis-extproc-grpc
            endpoints:
              - lb_endpoints:
                  - endpoint:
                      address:
                        socket_address:
                          address: "$SERVICE_NAME.$NAMESPACE.svc.cluster.local"
                          port_value: 9004
    - applyTo: HTTP_FILTER
      match:
        context: GATEWAY
        listener:
          filterChain:
            filter:
              name: envoy.filters.network.http_connection_manager
              subFilter:
                name: envoy.filters.http.router
      patch:
        operation: INSERT_BEFORE
        value:
          name: envoy.filters.http.ext_proc
          typed_config:
            "@type": type.googleapis.com/envoy.extensions.filters.http.ext_proc.v3.ExternalProcessor
            failure_mode_allow: false
            grpc_service:
              envoy_grpc:
                cluster_name: praxis-extproc-grpc
                authority: "$SERVICE_NAME.$NAMESPACE.svc.cluster.local"
              timeout: 5s
            message_timeout: 5s
            processing_mode:
              request_header_mode: SEND
              response_header_mode: SEND
              request_body_mode: BUFFERED
              response_body_mode: NONE
YAML
chmod 0600 "$EVIDENCE_DIR/extproc-resources.yaml"
sha256sum "$EVIDENCE_DIR/extproc-resources.yaml" >"$EVIDENCE_DIR/render-sha256.txt"

oc apply --dry-run=server -f "$EVIDENCE_DIR/extproc-resources.yaml" >"$EVIDENCE_DIR/server-dry-run.txt"
if [[ "$MODE" == render ]]; then
  jq -n --arg evidence "$EVIDENCE_DIR" --arg image "$IMAGE" --arg source "$SOURCE_REVISION" \
    '{mode:"render",status:"rendered",evidenceDirectory:$evidence,image:$image,sourceRevision:$source,clusterMutated:false,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
  log 'ExtProc resources rendered and server-side dry-run passed; no cluster resources changed'
  exit 0
fi

cat >"$EVIDENCE_DIR/INVOCATION.md" <<EOF
# Task-owned Praxis ExtProc deployment

- Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- Context: $context
- User: $user
- API server: $server
- API readiness: $ready
- API TLS mode: $API_TLS_MODE
- Run label: $RUN_LABEL
- ExtProc image/source: $IMAGE / $SOURCE_REVISION
- Image provenance: $IMAGE_PROVENANCE_FILE
- Namespace/Gateway/filter: $NAMESPACE/$GATEWAY_NAME/$ENVOYFILTER_NAME
- ExtProc Service: $SERVICE_NAME.$NAMESPACE.svc.cluster.local:9004 (h2c)
- Failure mode: closed
- Response marker: X-Praxis-ExtProc-Response=$RUN_LABEL
- Request body mode: BUFFERED
- Request body digest oracle: e2e_body_oracle; response digest header: x-qualification-request-sha256

No credentials, Secret values, certificate bytes, or inference requests are recorded/sent.
EOF
printf 'context=%s\nuser=%s\nserver=%s\nreadyz=%s\napi_tls_mode=%s\n' \
  "$context" "$user" "$server" "$ready" "$API_TLS_MODE" >"$EVIDENCE_DIR/api-access-summary.txt"

if [[ "$MODE" == install ]]; then
  CURRENT_PHASE='resource-apply'
  oc apply -f "$EVIDENCE_DIR/extproc-resources.yaml" >"$EVIDENCE_DIR/apply.txt"
  oc wait deployment/"$DEPLOYMENT_NAME" -n "$NAMESPACE" --for=condition=Available --timeout="$TIMEOUT" \
    >"$EVIDENCE_DIR/deployment-wait.txt"
fi

CURRENT_PHASE=runtime-readiness
oc get deployment "$DEPLOYMENT_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/deployment.json"
jq -e --arg run "$RUN_LABEL" --arg image "$IMAGE" \
  '.metadata.labels["praxis.fast/qualification-run"]==$run and
   any(.spec.template.spec.containers[]?;.name=="praxis-extproc" and .image==$image)' \
  "$EVIDENCE_DIR/deployment.json" >/dev/null || {
  CURRENT_FAILURE='ExtProc Deployment ownership or pinned image check failed'
  printf 'ExtProc Deployment readiness/image gate failed\n' >&2
  exit 1
}
pod_deadline=$((SECONDS + 60))
while (( SECONDS < pod_deadline )); do
  oc get deployment "$DEPLOYMENT_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/deployment.json"
  oc get pods -n "$NAMESPACE" -l "praxis.fast/qualification-run=$RUN_LABEL,app.kubernetes.io/name=praxis-extproc" -o json \
    | jq '{items:[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp,labels:.metadata.labels},phase:.status.phase,conditions:.status.conditions,containers:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}]}' \
    >"$EVIDENCE_DIR/pods.json"
  if jq -e --arg run "$RUN_LABEL" --arg image "$IMAGE" '
       .metadata.labels["praxis.fast/qualification-run"]==$run and
       any(.spec.template.spec.containers[]?;.name=="praxis-extproc" and .image==$image) and
       (.spec.replicas//1)==1 and
       .status.observedGeneration==.metadata.generation and
       (.status.updatedReplicas//0)==1 and (.status.replicas//0)==1 and
       (.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1
     ' "$EVIDENCE_DIR/deployment.json" >/dev/null \
    && jq -e --arg image "$IMAGE" --arg digest "$IMAGE_DIGEST" '
       [.items[]] as $pods |
       [$pods[].containers[]|select(.name=="praxis-extproc")] as $runtime |
       ($pods|length)==1 and
       all($pods[];.metadata.deletionTimestamp==null and .phase=="Running" and
         any(.conditions[]?;.type=="Ready" and .status=="True")) and
       all($pods[].containers[];.ready==true) and
       ($runtime|length)==1 and
       all($runtime[];.image==$image and (.imageID|endswith($digest)) and .ready==true)
     ' "$EVIDENCE_DIR/pods.json" >/dev/null; then
    break
  fi
  sleep 2
done
jq -e --arg run "$RUN_LABEL" --arg image "$IMAGE" '
     .metadata.labels["praxis.fast/qualification-run"]==$run and
     any(.spec.template.spec.containers[]?;.name=="praxis-extproc" and .image==$image) and
     (.spec.replicas//1)==1 and .status.observedGeneration==.metadata.generation and
     (.status.updatedReplicas//0)==1 and (.status.replicas//0)==1 and
     (.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1
   ' "$EVIDENCE_DIR/deployment.json" >/dev/null || {
  CURRENT_FAILURE='ExtProc Deployment did not complete its single-replica rollout at the observed generation'
  printf 'ExtProc Deployment rollout status gate failed after bounded wait\n' >&2
  exit 1
}
jq -e --arg image "$IMAGE" --arg digest "$IMAGE_DIGEST" '
     [.items[]] as $pods |
     [$pods[].containers[]|select(.name=="praxis-extproc")] as $runtime |
     ($pods|length)==1 and
     all($pods[];.metadata.deletionTimestamp==null and .phase=="Running" and
       any(.conditions[]?;.type=="Ready" and .status=="True")) and
     all($pods[].containers[];.ready==true) and
     ($runtime|length)==1 and
     all($runtime[];.image==$image and (.imageID|endswith($digest)) and .ready==true)
   ' "$EVIDENCE_DIR/pods.json" >/dev/null || {
  CURRENT_FAILURE='ExtProc rollout did not converge to one current-generation Ready Pod with the pinned immutable image and no terminating or unready old Pods'
  printf 'ExtProc active Pod/imageID readiness gate failed after bounded rollout wait\n' >&2
  exit 1
}
extproc_runtime_record="$(jq -cer --arg digest "$IMAGE_DIGEST" '
  [.items[]][0] as $pod
  | [$pod.containers[]|select(.name=="praxis-extproc")][0] as $container
  | {pod:$pod.metadata.name,podUid:$pod.metadata.uid,imageId:$container.imageID,
     imageDigest:$digest,ready:$container.ready}
' "$EVIDENCE_DIR/pods.json")"
extproc_pod="$(jq -er '.pod' <<<"$extproc_runtime_record")"
extproc_pod_uid="$(jq -er '.podUid' <<<"$extproc_runtime_record")"
oc get service "$SERVICE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/service.json"
oc get endpointslice -n "$NAMESPACE" -l "kubernetes.io/service-name=$SERVICE_NAME" -o json \
  | jq '[.items[]|{metadata:{name:.metadata.name,labels:.metadata.labels},ports:.ports,endpoints:[.endpoints[]?|{conditions:.conditions,targetRef:.targetRef,addresses:.addresses}]}]' \
  >"$EVIDENCE_DIR/endpointslices.json"
jq -e 'any(.[];.endpoints|any(.[];.conditions.ready==true))' "$EVIDENCE_DIR/endpointslices.json" >/dev/null || {
  CURRENT_FAILURE='ExtProc Service has no ready EndpointSlice target'
  printf 'ExtProc Service has no ready endpoints\n' >&2
  exit 1
}
oc get envoyfilter "$ENVOYFILTER_NAME" -n "$NAMESPACE" -o json \
  | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels,annotations:.metadata.annotations},workloadSelector:.spec.workloadSelector,patches:[.spec.configPatches[]|{applyTo:.applyTo,context:.match.context,operation:.patch.operation,filterName:.patch.value.name,clusterName:.patch.value.name}]}' \
  >"$EVIDENCE_DIR/envoyfilter-summary.json"
jq -e --arg run "$RUN_LABEL" --arg gateway "$GATEWAY_NAME" \
  '.metadata.labels["praxis.fast/qualification-run"]==$run and .workloadSelector.labels["gateway.networking.k8s.io/gateway-name"]==$gateway and any(.patches[];.applyTo=="HTTP_FILTER" and .filterName=="envoy.filters.http.ext_proc")' \
  "$EVIDENCE_DIR/envoyfilter-summary.json" >/dev/null || {
  CURRENT_FAILURE='EnvoyFilter ownership, selector, or ext_proc patch gate failed'
  printf 'EnvoyFilter scope/patch gate failed\n' >&2
  exit 1
}

CURRENT_PHASE=data-plane-attachment
gateway_pod="$(oc get pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$GATEWAY_NAME" -o json \
  | jq -r '[.items[]|select(.status.phase=="Running" and any(.status.containerStatuses[]?;.name=="istio-proxy" and .ready==true))]|first|.metadata.name // empty')"
[[ -n "$gateway_pod" ]] || {
  CURRENT_FAILURE='task Gateway has no ready Istio data-plane Pod'
  printf 'task Gateway has no ready Istio data-plane Pod\n' >&2
  exit 1
}
if ! oc exec "$gateway_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET config_dump \
    2>"$EVIDENCE_DIR/proxy-config-query.stderr" \
    | jq -e --arg cluster praxis-extproc-grpc '
      {extprocFilterCount:([..|objects|select(.name?=="envoy.filters.http.ext_proc")]|length),
       extprocClusterPresent:any(..|objects;.name?==$cluster)}
      | select(.extprocFilterCount>0 and .extprocClusterPresent==true)
    ' >"$EVIDENCE_DIR/attached-filter-summary.json"; then
  CURRENT_FAILURE='Envoy proxy config dump does not contain both the ext_proc HTTP filter and task ExtProc cluster'
  printf 'ExtProc EnvoyFilter object exists but the active Gateway proxy does not show the filter and cluster\n' >&2
  exit 1
fi

jq -n --arg mode "$MODE" --arg evidence "$EVIDENCE_DIR" --arg image "$IMAGE" --arg source "$SOURCE_REVISION" \
  --arg gateway "$GATEWAY_NAME" --arg pod "$gateway_pod" \
  --argjson runtime "$extproc_runtime_record" \
  '{mode:$mode,status:"ready",evidenceDirectory:$evidence,image:$image,sourceRevision:$source,gateway:$gateway,gatewayPod:$pod,
    extprocRuntime:$runtime,extprocAttached:true,inferenceTrafficSent:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
cat >"$EVIDENCE_DIR/FINAL-RESULT.md" <<EOF
# Praxis ExtProc deployment result

- Status: READY; no inference traffic sent.
- Image/source: $IMAGE / $SOURCE_REVISION
- Active ExtProc Pod: $extproc_pod (UID $extproc_pod_uid); runtime image digest matches the pinned digest.
- Task Gateway: $NAMESPACE/$GATEWAY_NAME
- Active proxy config contains the ext_proc HTTP filter and praxis-extproc-grpc cluster.
- Request/response marker configuration and buffered body digest oracle are active; traffic oracle remains not run.
- Evidence: $EVIDENCE_DIR
EOF
log "Praxis ExtProc $MODE complete; Deployment digest and active Gateway EnvoyFilter verified; no inference traffic sent"
find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
