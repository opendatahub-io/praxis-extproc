#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
SOURCE_DIR=""
SOURCE_SHA=""
SOURCE_CONTEXT_SHA=""
IMAGE_REPOSITORY=""
IMAGE_TAG=""
IMAGE_PROVENANCE=""
IMAGE_BUILD_METADATA=""
EVIDENCE_DIR=""
TIMEOUT="5m"
IMAGE_PULL_POLICY="IfNotPresent"
FAILURE_CAPTURED=false

usage() {
  cat <<'USAGE'
Usage: install-kind-kserve.sh --context CTX --kubeconfig PRIVATE-FILE \
  --expected-api-server URL --source-dir CLEAN-KSERVE-WORKTREE --source-sha SHA \
  --source-context-sha SHA256 --image-repository REGISTRY/REPO \
  --image-provenance FILE --image-build-metadata FILE --evidence-dir NEW-DIR \
  [--image-tag TAG] [--timeout 5m]

Installs the source-matched KServe LLMInferenceService CRDs, controller/webhooks,
and runtime configurations into a fresh Kind cluster. The controller image must
already be built from the exact source SHA and pushed to the run-owned registry;
its verified build metadata supplies the published tag. An optional --image-tag
is accepted only when it matches that record. This helper does not install a
manually assembled InferencePool or fabricate controller status.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --source-dir) SOURCE_DIR="${2:?}"; shift 2 ;;
    --source-sha) SOURCE_SHA="${2:?}"; shift 2 ;;
    --source-context-sha) SOURCE_CONTEXT_SHA="${2:?}"; shift 2 ;;
    --image-repository) IMAGE_REPOSITORY="${2:?}"; shift 2 ;;
    --image-tag) IMAGE_TAG="${2:?}"; shift 2 ;;
    --image-provenance) IMAGE_PROVENANCE="${2:?}"; shift 2 ;;
    --image-build-metadata) IMAGE_BUILD_METADATA="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --timeout) TIMEOUT="${2:?}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { printf 'install-kind-kserve: %s\n' "$*" >&2; exit 1; }
log() { printf '%s\n' "$*" | tee -a "$EVIDENCE_DIR/run.log"; }
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
h() { command helm --kubeconfig "$KUBECONFIG_FILE" --kube-context "$CONTEXT" "$@"; }

capture_failure() {
  local status="$1"
  ((status != 0)) || return 0
  [[ "$FAILURE_CAPTURED" == true ]] && return 0
  FAILURE_CAPTURED=true
  set +e
  k get namespace kserve -o json >"$EVIDENCE_DIR/failure-kserve-namespace.json" 2>&1
  k get deployments,pods,services,endpointslices,certificates -n kserve -o yaml >"$EVIDENCE_DIR/failure-kserve-workloads.yaml" 2>&1
  k get crds -o json | jq '[.items[]|select(.metadata.name|test("llminferenceservice|inferencepool"))|{name:.metadata.name,uid:.metadata.uid,conditions:.status.conditions}]' >"$EVIDENCE_DIR/failure-kserve-crds.json" 2>&1
  k get validatingwebhookconfigurations,mutatingwebhookconfigurations -o json |
    jq '[.items[]|{kind:.kind,name:.metadata.name,webhooks:[.webhooks[]?|{name:.name,service:.clientConfig.service,caBundleBytes:(.clientConfig.caBundle//""|length)}]}]' \
    >"$EVIDENCE_DIR/failure-kserve-webhooks.json" 2>&1
  k get events -n kserve --sort-by=.lastTimestamp -o json |
    jq '[.items[]|{type:.type,reason:.reason,message:.message,object:.involvedObject.name,lastTimestamp:.lastTimestamp}]' \
    >"$EVIDENCE_DIR/failure-kserve-events.json" 2>&1
  k logs deployment/llmisvc-controller-manager -n kserve --all-containers=true --prefix --tail=250 \
    >"$EVIDENCE_DIR/failure-controller-logs.txt" 2>&1
  set -e
  return "$status"
}

[[ -n "$EVIDENCE_DIR" ]] || fail 'new evidence directory is required'
[[ -f "$KUBECONFIG_FILE" && -n "$CONTEXT" && -n "$EXPECTED_API_SERVER" ]] || fail 'explicit context, private kubeconfig, and expected API server are required'
[[ "$(git -C "$SOURCE_DIR" rev-parse --is-inside-work-tree 2>/dev/null || true)" == true && -f "$IMAGE_PROVENANCE" && -f "$IMAGE_BUILD_METADATA" ]] || fail 'source worktree and both image provenance records are required'
mkdir -p "$(dirname "$EVIDENCE_DIR")"
[[ ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory already exists; refusing overwrite'
mkdir -m 0700 "$EVIDENCE_DIR"
trap 'status=$?; capture_failure "$status"' EXIT

for binary in kubectl helm jq git sha256sum rg python3 date seq sleep; do
  command -v "$binary" >/dev/null || fail "$binary is required"
done
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ && "$SOURCE_CONTEXT_SHA" =~ ^[0-9a-f]{64}$ ]] || fail 'source SHA or source context digest is malformed'
[[ "$IMAGE_REPOSITORY" =~ ^[a-zA-Z0-9._:/-]+$ && "$IMAGE_REPOSITORY" != *'@'* ]] || fail 'image repository must be a run-owned taggable repository path'
if [[ -n "$IMAGE_TAG" ]]; then
  [[ "$IMAGE_TAG" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] || fail 'image tag is malformed'
fi
[[ "$TIMEOUT" =~ ^[0-9]+[smh]$ ]] || fail 'timeout must be a duration such as 5m'
[[ "$IMAGE_PULL_POLICY" == IfNotPresent ]] || fail 'Kind qualification requires the local registry pull policy IfNotPresent'

tag_args=()
[[ -z "$IMAGE_TAG" ]] || tag_args+=(--requested-tag "$IMAGE_TAG")
IMAGE_TAG="$(python3 "$SCRIPT_DIR/resolve_kind_image_tag.py" \
  --metadata "$IMAGE_BUILD_METADATA" \
  --image-repository "$IMAGE_REPOSITORY" \
  --component kserve-llmisvc-controller \
  "${tag_args[@]}")" || fail 'build metadata does not contain a matching run-registry tag'

actual_server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$actual_server" == "$EXPECTED_API_SERVER" ]] || fail 'selected context API server does not match the expected Kind API server'
actual_readyz="$(k get --raw=/readyz)"
[[ "$actual_readyz" == *ok* ]] || fail 'Kubernetes API /readyz is not healthy'
[[ "$(git -C "$SOURCE_DIR" rev-parse HEAD)" == "$SOURCE_SHA" ]] || fail 'KServe source HEAD does not match the run lock'
[[ -z "$(git -C "$SOURCE_DIR" branch --show-current)" ]] || fail 'KServe source worktree must be detached'
[[ -z "$(git -C "$SOURCE_DIR" status --porcelain --untracked-files=all)" ]] || fail 'KServe source worktree must be clean'
[[ "$(stat -c '%a' "$KUBECONFIG_FILE")" == 600 || "$(stat -c '%a' "$KUBECONFIG_FILE")" == 400 ]] || fail 'KServe kubeconfig must have mode 0600 or 0400'
actual_context_sha="$(git -C "$SOURCE_DIR" archive --format=tar "$SOURCE_SHA" | sha256sum | awk '{print $1}')"
[[ "$actual_context_sha" == "$SOURCE_CONTEXT_SHA" ]] || fail 'KServe source archive hash does not match the run lock'

jq -e --arg sha "$SOURCE_SHA" --arg context "$SOURCE_CONTEXT_SHA" --arg repo "$IMAGE_REPOSITORY" \
  '.verified==true and .source_sha==$sha and .build_context_sha256==$context and
   .oci_revision_label==$sha and .oci_build_context_label==$context and
   .source_url=="https://github.com/kserve/kserve.git" and
   (.image|startswith($repo+"@sha256:"))' "$IMAGE_PROVENANCE" >/dev/null || fail 'KServe controller image provenance does not match the locked source and registry repository'
IMAGE_DIGEST="$(jq -er '.image|capture("@(?<digest>sha256:[0-9a-f]{64})$").digest' "$IMAGE_PROVENANCE")"
jq -e --arg sha "$SOURCE_SHA" --arg context "$SOURCE_CONTEXT_SHA" --argjson image "$(cat "$IMAGE_PROVENANCE")" \
  '.schema=="praxis-extproc.image-build/v1" and .component=="kserve-llmisvc-controller" and
   .source_sha==$sha and .build_context_sha256==$context and .build_platform=="linux/amd64" and
   .deployed_image==$image.image' "$IMAGE_BUILD_METADATA" >/dev/null || fail 'KServe controller build metadata does not match the source, image provenance, or linux/amd64 target'
CONTROLLER_IMAGE="$IMAGE_REPOSITORY:$IMAGE_TAG"

if k get namespace kserve >/dev/null 2>&1; then
  h list --all --namespace kserve --output json >"$EVIDENCE_DIR/preexisting-helm-releases.json"
  [[ "$(jq 'length' "$EVIDENCE_DIR/preexisting-helm-releases.json")" == 0 ]] || fail 'KServe namespace already contains Helm releases; inspect/verify rather than reinstall'
  if k get crd llminferenceservices.serving.kserve.io >/dev/null 2>&1; then
    fail 'LLMInferenceService CRD already exists outside this fresh install path; inspect its source and owner before proceeding'
  fi
else
  printf '[]\n' >"$EVIDENCE_DIR/preexisting-helm-releases.json"
fi

printf '%s\n' "$actual_server" >"$EVIDENCE_DIR/api-server.txt"
printf '%s\n' "$actual_readyz" >"$EVIDENCE_DIR/readyz-before.txt"
printf '%s\n' "$SOURCE_SHA" >"$EVIDENCE_DIR/kserve-source-sha.txt"
printf '%s\n' "$SOURCE_CONTEXT_SHA" >"$EVIDENCE_DIR/kserve-build-context-sha256.txt"
cp "$IMAGE_PROVENANCE" "$EVIDENCE_DIR/controller-image-provenance.json"
cp "$IMAGE_BUILD_METADATA" "$EVIDENCE_DIR/controller-image-build-metadata.json"
chmod 0600 "$EVIDENCE_DIR/controller-image-provenance.json"
chmod 0600 "$EVIDENCE_DIR/controller-image-build-metadata.json"

charts_dir="$EVIDENCE_DIR/charts"
mkdir -m 0700 "$charts_dir"
crd_chart="$SOURCE_DIR/charts/kserve-llmisvc-crd"
resources_chart="$SOURCE_DIR/charts/kserve-llmisvc-resources"
runtime_chart="$SOURCE_DIR/charts/kserve-runtime-configs"
python3 "$SCRIPT_DIR/resolve_kind_kserve_chart_metadata.py" --source-dir "$SOURCE_DIR" --source-sha "$SOURCE_SHA" \
  >"$EVIDENCE_DIR/chart-source-metadata.json"
chmod 0600 "$EVIDENCE_DIR/chart-source-metadata.json"
crd_chart_version="$(jq -er '.charts["kserve-llmisvc-crd"].version' "$EVIDENCE_DIR/chart-source-metadata.json")"
resources_chart_version="$(jq -er '.charts["kserve-llmisvc-resources"].version' "$EVIDENCE_DIR/chart-source-metadata.json")"
runtime_chart_version="$(jq -er '.charts["kserve-runtime-configs"].version' "$EVIDENCE_DIR/chart-source-metadata.json")"
for chart in "$crd_chart" "$resources_chart" "$runtime_chart"; do
  [[ -f "$chart/Chart.yaml" ]] || fail "source chart is missing: $chart"
  h package "$chart" --destination "$charts_dir" >"$EVIDENCE_DIR/helm-package-$(basename "$chart").txt"
done
sha256sum "$charts_dir"/*.tgz >"$EVIDENCE_DIR/chart-archives.sha256"
printf 'kserve:\n  llmisvc:\n    createGIECRDs: true\n    controller:\n      image: "%s"\n      tag: "%s"\n      imagePullPolicy: IfNotPresent\n' \
  "$IMAGE_REPOSITORY" "$IMAGE_TAG" >"$EVIDENCE_DIR/kserve-kind-values.yaml"
chmod 0600 "$EVIDENCE_DIR/kserve-kind-values.yaml"
h template kserve-llmisvc-crd "$charts_dir/kserve-llmisvc-crd-$crd_chart_version.tgz" --namespace kserve --include-crds \
  >"$EVIDENCE_DIR/rendered-crds.yaml"
h template kserve-llmisvc-resources "$charts_dir/kserve-llmisvc-resources-$resources_chart_version.tgz" --namespace kserve \
  --values "$EVIDENCE_DIR/kserve-kind-values.yaml" --include-crds >"$EVIDENCE_DIR/rendered-controller-resources.yaml"
h template kserve-runtime-configs "$charts_dir/kserve-runtime-configs-$runtime_chart_version.tgz" --namespace kserve \
  --set kserve.llmisvcConfigs.enabled=true --set kserve.servingruntime.enabled=false \
  >"$EVIDENCE_DIR/rendered-runtime-configs.yaml"
rg -Fq "image: $CONTROLLER_IMAGE" "$EVIDENCE_DIR/rendered-controller-resources.yaml" || fail 'rendered controller image does not match the run image tag'
rg -Fq 'kind: CustomResourceDefinition' "$EVIDENCE_DIR/rendered-crds.yaml" || fail 'KServe CRD chart rendered no CRDs'
rg -Fq 'name: llmisvc-webhook-server-service' "$EVIDENCE_DIR/rendered-controller-resources.yaml" || fail 'KServe controller chart rendered no webhook Service'
rg -Fq 'name: inferencepools.inference.networking.k8s.io' "$EVIDENCE_DIR/rendered-controller-resources.yaml" || fail 'KServe chart rendered no Gateway Inference Extension CRD'
rg -Fq 'kind: LLMInferenceServiceConfig' "$EVIDENCE_DIR/rendered-runtime-configs.yaml" || fail 'KServe runtime chart rendered no LLMInferenceServiceConfig resources'

log "Installing source-locked KServe LLMInferenceService stack on $actual_server"
h upgrade --install kserve-llmisvc-crd "$charts_dir/kserve-llmisvc-crd-$crd_chart_version.tgz" \
  --namespace kserve --create-namespace --wait --timeout "$TIMEOUT" >"$EVIDENCE_DIR/install-crds.txt" 2>&1
for crd in llminferenceservices.serving.kserve.io llminferenceserviceconfigs.serving.kserve.io; do
  k wait --for=condition=Established "crd/$crd" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/wait-${crd}.txt"
done
k get crd llminferenceservices.serving.kserve.io -o json >"$EVIDENCE_DIR/llmisvc-crd.json"
jq -e 'any(.spec.versions[];.name=="v1alpha2" and .served==true and .storage==true)' "$EVIDENCE_DIR/llmisvc-crd.json" >/dev/null || fail 'LLMInferenceService CRD does not serve and store v1alpha2'

h upgrade --install kserve-llmisvc-resources "$charts_dir/kserve-llmisvc-resources-$resources_chart_version.tgz" \
  --namespace kserve --create-namespace --values "$EVIDENCE_DIR/kserve-kind-values.yaml" \
  --timeout "$TIMEOUT" >"$EVIDENCE_DIR/install-controller.txt" 2>&1
k wait --for=condition=Ready certificate/llmisvc-serving-cert -n kserve --timeout="$TIMEOUT" \
  >"$EVIDENCE_DIR/wait-webhook-certificate.txt"
k get certificate llmisvc-serving-cert -n kserve -o json >"$EVIDENCE_DIR/webhook-certificate.json"
k rollout status deployment/llmisvc-controller-manager -n kserve --timeout="$TIMEOUT" >"$EVIDENCE_DIR/controller-rollout.txt"
k get deployment llmisvc-controller-manager -n kserve -o json >"$EVIDENCE_DIR/controller-deployment.json"
jq -e '(.status.observedGeneration//0)>=(.metadata.generation//0) and
       (.status.readyReplicas//0)==(.spec.replicas//0) and (.spec.replicas//0)==1 and
       (.status.availableReplicas//0)==1 and
       any(.status.conditions[]?;.type=="Available" and .status=="True")' \
  "$EVIDENCE_DIR/controller-deployment.json" >/dev/null || fail 'KServe LLMInferenceService controller did not become current and Available'
selector="$(jq -er '.spec.selector.matchLabels|to_entries|map("\(.key)=\(.value)")|join(",")' "$EVIDENCE_DIR/controller-deployment.json")"
k get pods -n kserve -l "$selector" -o json >"$EVIDENCE_DIR/controller-pods.json"
jq -e --arg repository "$IMAGE_REPOSITORY" --arg tag "$IMAGE_TAG" --arg digest "$IMAGE_DIGEST" \
  '[.items[]|select(.metadata.deletionTimestamp==null and .status.phase=="Running" and
    any(.status.conditions[]?;.type=="Ready" and .status=="True"))] as $ready |
   ($ready|length)==1 and
   all($ready[0].spec.containers[]|select(.name=="manager");.image==($repository+":"+$tag)) and
   all($ready[0].status.containerStatuses[]|select(.name=="manager");.ready==true and (.imageID|endswith($digest)))' \
  "$EVIDENCE_DIR/controller-pods.json" >/dev/null || fail 'KServe controller must have exactly one Ready Pod with the pinned runtime image digest'

# A Deployment rollout can report Available while an old Pod remains
# terminating in the webhook EndpointSlice. Wait for one fully converged Pod
# and its matching Ready EndpointSlice before creating webhook-dependent CRs.
webhook_converged=false
: >"$EVIDENCE_DIR/webhook-convergence-polls.tsv"
for attempt in $(seq 1 60); do
  k get deployment llmisvc-controller-manager -n kserve -o json >"$EVIDENCE_DIR/controller-deployment.json"
  selector="$(jq -er '.spec.selector.matchLabels|to_entries|map("\(.key)=\(.value)")|join(",")' "$EVIDENCE_DIR/controller-deployment.json")"
  k get pods -n kserve -l "$selector" -o json >"$EVIDENCE_DIR/controller-pods.json"
  k get service llmisvc-webhook-server-service -n kserve -o json >"$EVIDENCE_DIR/webhook-service.json"
  k get endpointslices -n kserve -l kubernetes.io/service-name=llmisvc-webhook-server-service -o json \
    >"$EVIDENCE_DIR/webhook-endpointslices.json"
  if python3 "$SCRIPT_DIR/verify_kind_kserve_webhook_readiness.py" \
    --deployment "$EVIDENCE_DIR/controller-deployment.json" \
    --pods "$EVIDENCE_DIR/controller-pods.json" \
    --service "$EVIDENCE_DIR/webhook-service.json" \
    --endpointslices "$EVIDENCE_DIR/webhook-endpointslices.json" \
    --image-repository "$IMAGE_REPOSITORY" --image-tag "$IMAGE_TAG" --image-digest "$IMAGE_DIGEST" \
    >"$EVIDENCE_DIR/webhook-readiness.json" 2>"$EVIDENCE_DIR/webhook-readiness-last-error.txt"; then
    printf '%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$attempt" ready >>"$EVIDENCE_DIR/webhook-convergence-polls.tsv"
    webhook_converged=true
    break
  fi
  printf '%s\t%s\t%s\n' "$(date -u +%FT%TZ)" "$attempt" waiting >>"$EVIDENCE_DIR/webhook-convergence-polls.tsv"
  sleep 2
done
[[ "$webhook_converged" == true ]] || {
  cat "$EVIDENCE_DIR/webhook-readiness-last-error.txt" >&2
  fail 'KServe webhook deployment and EndpointSlice did not converge to one current Ready Pod'
}

for crd in inferencepools.inference.networking.k8s.io inferencepools.inference.networking.x-k8s.io; do
  k wait --for=condition=Established "crd/$crd" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/wait-${crd}.txt"
done
h upgrade --install kserve-runtime-configs "$charts_dir/kserve-runtime-configs-$runtime_chart_version.tgz" \
  --namespace kserve --create-namespace \
  --set kserve.llmisvcConfigs.enabled=true --set kserve.servingruntime.enabled=false \
  --wait --timeout "$TIMEOUT" >"$EVIDENCE_DIR/install-runtime-configs.txt" 2>&1
k get llminferenceserviceconfigs.serving.kserve.io -n kserve -o json >"$EVIDENCE_DIR/runtime-configs.json"
jq -e 'any(.items[]?;.metadata.name=="kserve-config-llm-scheduler") and (.items|length)>0' \
  "$EVIDENCE_DIR/runtime-configs.json" >/dev/null || fail 'KServe scheduler runtime configuration is absent'
k get endpointslices -n kserve -l kubernetes.io/service-name=llmisvc-webhook-server-service -o json >"$EVIDENCE_DIR/webhook-endpointslices.json"
jq -e 'any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true and (.addresses|length)>0))' \
  "$EVIDENCE_DIR/webhook-endpointslices.json" >/dev/null || fail 'KServe webhook Service has no Ready endpoint'
k get validatingwebhookconfigurations,mutatingwebhookconfigurations -o json >"$EVIDENCE_DIR/webhook-configurations.json"
jq -e '[.items[].webhooks[]?|select(.clientConfig.service.name=="llmisvc-webhook-server-service" and .clientConfig.service.namespace=="kserve")] as $w |
  ($w|length)>=2 and all($w[];(.clientConfig.caBundle//"")|length>0)' \
  "$EVIDENCE_DIR/webhook-configurations.json" >/dev/null || fail 'KServe admission webhook CA bundle or service reference is missing'
k get crd llminferenceserviceconfigs.serving.kserve.io inferencepools.inference.networking.k8s.io inferencepools.inference.networking.x-k8s.io -o json \
  | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,conditions:.status.conditions}]' >"$EVIDENCE_DIR/serving-crd-status.json"
h list --all --namespace kserve --output json >"$EVIDENCE_DIR/helm-releases.json"
k get --raw=/readyz >"$EVIDENCE_DIR/readyz-after.txt"
jq -n --arg sourceSha "$SOURCE_SHA" --arg contextSha "$SOURCE_CONTEXT_SHA" \
  --arg image "$CONTROLLER_IMAGE" --arg digest "$IMAGE_DIGEST" --arg server "$actual_server" \
  --argjson chartMetadata "$(cat "$EVIDENCE_DIR/chart-source-metadata.json")" \
  '{status:"PASS",apiServer:$server,sourceSha:$sourceSha,buildContextSha256:$contextSha,
    controllerImage:$image,controllerManifestDigest:$digest,controllerReadyReplicas:1,
    chartSourceMetadata:$chartMetadata,
    llmInferenceServiceAPI:{group:"serving.kserve.io",version:"v1alpha2",served:true,stored:true},
    controllerWebhookCertificateReady:true,webhookReadyEndpoint:true,webhookCABundlesPresent:true,
    gatewayInferenceExtensionCRDsEstablished:true,runtimeSchedulerConfigPresent:true,
    llmInferenceServiceCreated:false,simulatorReceiptObserved:false}' >"$EVIDENCE_DIR/RESULT.json"
log "KServe controller, webhooks, LLMInferenceService API, Gateway inference CRDs, and runtime config are ready; no inference request was sent"
