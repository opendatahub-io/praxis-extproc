#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Prepare and restore Kind-specific MaaS AITenant revocation prerequisites.
set -Eeuo pipefail
umask 077

MODE="${KIND_MAAS_CLEANUP_MODE:-plan}"
CONTEXT="${KUBE_CONTEXT:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
RUN_ID="${E2E_RUN_ID:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
CLUSTER="${KIND_CLUSTER_NAME:-}"
NAMESPACE="${KIND_MAAS_NAMESPACE:-maas-system}"
CONTROLLER="${KIND_MAAS_CONTROLLER_DEPLOYMENT:-maas-controller}"
CA_SOURCE="${KIND_MAAS_CA_SOURCE_SECRET:-maas-api-pool-a-serving-cert}"
CA_SECRET="${KIND_MAAS_CA_BUNDLE_SECRET:-opendatahub-ca}"
API_HOST="${KIND_MAAS_API_SERVICE_HOST:-maas-api-pool-a.maas-system.svc}"
IMAGE_REPOSITORY="${KIND_MAAS_CLEANUP_IMAGE_REPOSITORY:-kind-run/maas-api-key-cleanup}"
STATE_FILE="${KIND_MAAS_CLEANUP_STATE_FILE:-}"
EVIDENCE_DIR="${KIND_MAAS_CLEANUP_EVIDENCE_DIR:-}"
SOURCE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTAINERFILE="$SOURCE_DIR/kind-maas-cleanup.Containerfile"

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Plan: build a run-tagged non-root MaaS cleanup helper from a pinned UBI digest,
load it into the run-owned Kind cluster, verify the live MaaS API CA/hostname,
create the run-owned public CA bundle Secret, and temporarily override the
  MaaS cleanup-job image. If maas-system uses classic sidecar injection, disable
  injection for newly created revocation Jobs while they run. Restore the exact
  original namespace label and controller env, and remove only the run-created
  CA Secret after both AITenant finalizers complete.
PLAN
  exit 0
fi
[[ "$MODE" == prepare || "$MODE" == restore ]] || { printf 'KIND_MAAS_CLEANUP_MODE must be plan, prepare, or restore\n' >&2; exit 2; }
for bin in kubectl kind docker jq openssl git curl; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ -n "$CONTEXT" && -n "$EXPECTED_SERVER" && -n "$RUN_ID" && -n "$RUN_LABEL" && -n "$CLUSTER" && -n "$STATE_FILE" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, EXPECTED_API_SERVER, E2E_RUN_ID, GRID_RUN_LABEL, KIND_CLUSTER_NAME, KIND_MAAS_CLEANUP_STATE_FILE, and KIND_MAAS_CLEANUP_EVIDENCE_DIR are required\n' >&2
  exit 2
}
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,80}$ && "$RUN_LABEL" == "$RUN_ID" ]] || { printf 'run ID and ownership label must match a safe run identifier\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR" "$(dirname "$STATE_FILE")"
chmod 0700 "$EVIDENCE_DIR"

k() { command kubectl --context="$CONTEXT" --request-timeout=20s "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
fail() { log "BLOCKED: $*"; exit 1; }
API_PROXY_PID=""
TMP_CLEANUP_DIR=""
cleanup_on_exit() {
  if [[ -n "$API_PROXY_PID" ]]; then
    kill "$API_PROXY_PID" 2>/dev/null || true
    wait "$API_PROXY_PID" 2>/dev/null || true
  fi
  [[ -z "$TMP_CLEANUP_DIR" ]] || rm -rf "$TMP_CLEANUP_DIR"
}
trap cleanup_on_exit EXIT

server="$(k config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
[[ "$server" == "$EXPECTED_SERVER" ]] || fail 'Kubernetes server identity mismatch'
[[ "$(k get --raw=/readyz)" == *ok* ]] || fail 'Kubernetes API /readyz failed'
kind get clusters | rg -qx "$CLUSTER" || fail 'the named run-owned Kind cluster is absent'

if [[ "$MODE" == prepare ]]; then
  [[ ! -e "$STATE_FILE" ]] || fail 'cleanup state already exists; refusing a second prepare'
  cm_object="$(k get configmap maas-parameters -n "$NAMESPACE" -o json)" || fail 'MaaS parameters ConfigMap is absent'
  deployment_object="$(k get deployment "$CONTROLLER" -n "$NAMESPACE" -o json)" || fail 'MaaS controller Deployment is absent'
  namespace_object="$(k get namespace "$NAMESPACE" -o json)" || fail 'MaaS namespace is absent'
  python3 "$SOURCE_DIR/kind_maas_injection.py" snapshot <<<"$namespace_object" \
    >"$EVIDENCE_DIR/maas-namespace-injection-before.json" || fail 'MaaS namespace injection mode is unsupported or invalid'
  source_secret_metadata="$(k get secret "$CA_SOURCE" -n "$NAMESPACE" -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},hasCABundle:((.data//{})|has("ca.crt")),hasTLSCertificate:((.data//{})|has("tls.crt"))}')" \
    || fail 'MaaS serving certificate Secret is absent'
  ca_b64="$(k get secret "$CA_SOURCE" -n "$NAMESPACE" -o json | jq -r '.data["ca.crt"]')" || fail 'could not read the public MaaS CA bundle'
  tls_b64="$(k get secret "$CA_SOURCE" -n "$NAMESPACE" -o json | jq -r '.data["tls.crt"]')" || fail 'could not read the MaaS API public leaf certificate'

  jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,
       cleanupImageKeyPresent:((.data//{})|has("maas-api-key-cleanup-image")),
       cleanupImage:.data["maas-api-key-cleanup-image"]}' <<<"$cm_object" \
    >"$EVIDENCE_DIR/maas-parameters-before.json"
  jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,
       cleanupImageEnv:[.spec.template.spec.containers[].env[]?|select(.name=="RELATED_IMAGE_UBI_MINIMAL_IMAGE")],
       templateAnnotations:.spec.template.metadata.annotations}' <<<"$deployment_object" >"$EVIDENCE_DIR/maas-controller-before.json"
  printf '%s\n' "$source_secret_metadata" >"$EVIDENCE_DIR/maas-api-ca-source.json"
  jq -e '.cleanupImageEnv|length==1 and .[0].valueFrom.configMapKeyRef.name=="maas-parameters" and .[0].valueFrom.configMapKeyRef.key=="maas-api-key-cleanup-image"' \
    "$EVIDENCE_DIR/maas-controller-before.json" >/dev/null || fail 'MaaS controller cleanup image is not sourced from the expected ConfigMap key'
  jq -e '.hasCABundle and .hasTLSCertificate' "$EVIDENCE_DIR/maas-api-ca-source.json" >/dev/null || fail 'MaaS serving Secret lacks CA or leaf certificate'

  image_ref="$IMAGE_REPOSITORY:$RUN_ID"
  context_hash="$(sha256sum "$CONTAINERFILE" | awk '{print $1}')"
  source_revision="$(git -C "$SOURCE_DIR/../.." rev-parse HEAD)"
  build_context="$EVIDENCE_DIR/build-context"
  mkdir -m 0700 "$build_context"
  cp "$CONTAINERFILE" "$build_context/Containerfile"
  docker build --pull --file "$build_context/Containerfile" --tag "$image_ref" \
    --label "org.opencontainers.image.revision=$source_revision" \
    --label "praxis.fast/qualification-run=$RUN_ID" \
    --label "org.opencontainers.image.build-context-sha256=$context_hash" \
    "$build_context" >"$EVIDENCE_DIR/image-build.log" 2>&1 || fail 'MaaS cleanup helper image build failed'
  docker image inspect "$image_ref" >"$EVIDENCE_DIR/image-inspect.raw.json" || fail 'built helper image cannot be inspected'
  jq '.[0]|{Id,RepoTags,RepoDigests,User:.Config.User,Architecture,Os,Labels:.Config.Labels}' \
    "$EVIDENCE_DIR/image-inspect.raw.json" >"$EVIDENCE_DIR/image-identity.json"
  jq -e --arg revision "$source_revision" --arg run "$RUN_ID" --arg hash "$context_hash" '.[0].Config.User=="1001" and .[0].Config.Labels["org.opencontainers.image.revision"]==$revision and .[0].Config.Labels["praxis.fast/qualification-run"]==$run and .[0].Config.Labels["org.opencontainers.image.build-context-sha256"]==$hash' \
    "$EVIDENCE_DIR/image-inspect.raw.json" >/dev/null || fail 'helper image identity does not match its run and build context'
  kind load docker-image "$image_ref" --name "$CLUSTER" >"$EVIDENCE_DIR/kind-image-load.txt" 2>&1 || fail 'could not load MaaS cleanup helper into the run cluster'

  tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/kind-maas-cleanup.XXXXXX")"
  TMP_CLEANUP_DIR="$tmp_dir"
  [[ -n "$ca_b64" && "$ca_b64" != null && -n "$tls_b64" && "$tls_b64" != null ]] || fail 'MaaS serving Secret certificate data is incomplete'
  printf '%s' "$ca_b64" | base64 -d >"$tmp_dir/ca.crt"
  printf '%s' "$tls_b64" | base64 -d >"$tmp_dir/tls.crt"
  openssl verify -CAfile "$tmp_dir/ca.crt" -verify_hostname "$API_HOST" "$tmp_dir/tls.crt" >"$EVIDENCE_DIR/maas-api-ca-verify.txt" 2>&1 || fail 'MaaS API CA does not verify the live API certificate and service hostname'
  ca_uid=""
  destination_secret="$(k get secret "$CA_SECRET" -n "$NAMESPACE" --ignore-not-found -o json)" || fail 'could not inspect the MaaS CA bundle Secret'
  if [[ -n "$destination_secret" ]]; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and .data["tls.crt"]!=null' \
      <<<"$destination_secret" >/dev/null || fail 'existing MaaS CA bundle Secret is not positively run-owned'
    jq -e --arg ca "$ca_b64" '.data["tls.crt"]==$ca' <<<"$destination_secret" >/dev/null || fail 'existing run CA bundle does not match the live MaaS CA'
    ca_uid="$(jq -r '.metadata.uid' <<<"$destination_secret")"
    printf 'false\n' >"$EVIDENCE_DIR/ca-secret-created-by-this-run.txt"
  else
    jq -n --arg namespace "$NAMESPACE" --arg name "$CA_SECRET" --arg run "$RUN_LABEL" \
      --arg ca "$ca_b64" \
      '{apiVersion:"v1",kind:"Secret",metadata:{name:$name,namespace:$namespace,labels:{"app.kubernetes.io/managed-by":"grid-llmd-e2e-run","app.kubernetes.io/part-of":"grid-llmd-e2e","praxis.fast/qualification-run":$run}},type:"Opaque",data:{"tls.crt":$ca}}' \
      | k create -f - -o json >"$tmp_dir/maas-api-ca-created.json" || fail 'could not create the run-owned MaaS API CA bundle Secret'
    ca_uid="$(jq -r '.metadata.uid' "$tmp_dir/maas-api-ca-created.json")"
    printf 'true\n' >"$EVIDENCE_DIR/ca-secret-created-by-this-run.txt"
  fi
  jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,keys:((.data//{})|keys)}' \
    <<<"$(k get secret "$CA_SECRET" -n "$NAMESPACE" -o json)" >"$EVIDENCE_DIR/maas-api-ca-destination.json"

  controller_uid="$(jq -r '.metadata.uid' <<<"$deployment_object")"
  container_index="$(jq -r '(.spec.template.spec.containers|to_entries|map(select(.value.env|any(.name=="RELATED_IMAGE_UBI_MINIMAL_IMAGE")))|first|.key)' <<<"$deployment_object")"
  env_index="$(jq -r --argjson ci "$container_index" '(.spec.template.spec.containers[$ci].env|to_entries|map(select(.value.name=="RELATED_IMAGE_UBI_MINIMAL_IMAGE"))|first|.key)' <<<"$deployment_object")"
  original_env="$(jq -c --argjson ci "$container_index" --argjson ei "$env_index" '.spec.template.spec.containers[$ci].env[$ei]' <<<"$deployment_object")"
  jq -n --arg run "$RUN_ID" --arg controllerUid "$controller_uid" --arg caUid "$ca_uid" --arg image "$image_ref" \
    --arg originalEnv "$original_env" --argjson containerIndex "$container_index" --argjson envIndex "$env_index" \
    --argjson createdCaSecret "$(cat "$EVIDENCE_DIR/ca-secret-created-by-this-run.txt")" \
    --argjson namespaceInjection "$(cat "$EVIDENCE_DIR/maas-namespace-injection-before.json")" \
    '{schema:"praxis-extproc.kind-maas-cleanup-state/v2",run:$run,controllerUID:$controllerUid,caSecretUID:$caUid,
      caSecretCreatedByThisRun:$createdCaSecret,image:$image,containerIndex:$containerIndex,envIndex:$envIndex,
      originalCleanupImageEnv:($originalEnv|fromjson),namespaceInjection:$namespaceInjection,
      prepared:false,restored:false}' >"$STATE_FILE.tmp"
  mv "$STATE_FILE.tmp" "$STATE_FILE"
  patch="$(jq -nc --arg uid "$controller_uid" --arg image "$image_ref" --argjson ci "$container_index" --argjson ei "$env_index" \
    '[{op:"test",path:"/metadata/uid",value:$uid},{op:"replace",path:("/spec/template/spec/containers/"+($ci|tostring)+"/env/"+($ei|tostring)),value:{name:"RELATED_IMAGE_UBI_MINIMAL_IMAGE",value:$image}}]')"
  k patch deployment "$CONTROLLER" -n "$NAMESPACE" --type=json -p "$patch" >"$EVIDENCE_DIR/maas-controller-image-override-patch.txt" || fail 'could not apply the temporary MaaS cleanup image override'
  k rollout status deployment/"$CONTROLLER" -n "$NAMESPACE" --timeout=180s >"$EVIDENCE_DIR/maas-controller-override-rollout.txt" || fail 'MaaS controller did not converge after cleanup image override'
  k get deployment "$CONTROLLER" -n "$NAMESPACE" -o json | jq '{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,cleanupImageEnv:[.spec.template.spec.containers[].env[]?|select(.name=="RELATED_IMAGE_UBI_MINIMAL_IMAGE")]}' \
    >"$EVIDENCE_DIR/maas-controller-override.json"
  jq -e --arg image "$image_ref" '.generation==.observedGeneration and .readyReplicas>=1 and (.cleanupImageEnv|length==1 and .[0].value==$image)' \
    "$EVIDENCE_DIR/maas-controller-override.json" >/dev/null || fail 'MaaS controller is not converged on the run-built non-root cleanup image'
  namespace_patch="$(python3 "$SOURCE_DIR/kind_maas_injection.py" prepare-patch \
    --state "$EVIDENCE_DIR/maas-namespace-injection-before.json")" || fail 'could not build the guarded MaaS namespace injection patch'
  if [[ "$namespace_patch" != '[]' ]]; then
    k patch namespace "$NAMESPACE" --type=json -p "$namespace_patch" \
      >"$EVIDENCE_DIR/maas-namespace-injection-prepare-patch.txt" || fail 'could not disable classic Istio sidecar injection for MaaS cleanup Jobs'
  else
    printf 'not-required\n' >"$EVIDENCE_DIR/maas-namespace-injection-prepare-patch.txt"
  fi
  namespace_after_prepare="$(k get namespace "$NAMESPACE" -o json)" || fail 'MaaS namespace disappeared after cleanup preparation'
  printf '%s\n' "$namespace_after_prepare" | jq '{name:.metadata.name,uid:.metadata.uid,labels:{"istio-injection":.metadata.labels["istio-injection"],"istio.io/rev":.metadata.labels["istio.io/rev"],"istio.io/dataplane-mode":.metadata.labels["istio.io/dataplane-mode"]}}' \
    >"$EVIDENCE_DIR/maas-namespace-injection-prepared.json"
  if [[ "$(jq -r '.changed' "$EVIDENCE_DIR/maas-namespace-injection-before.json")" == true ]]; then
    jq -e --arg uid "$(jq -r '.uid' "$EVIDENCE_DIR/maas-namespace-injection-before.json")" \
      '.metadata.uid==$uid and .metadata.labels["istio-injection"]=="disabled"' \
      <<<"$namespace_after_prepare" >/dev/null || fail 'MaaS namespace sidecar injection did not switch to disabled'
  fi
  jq '.prepared=true' "$STATE_FILE" >"$STATE_FILE.tmp"
  mv "$STATE_FILE.tmp" "$STATE_FILE"
  jq -n --arg run "$RUN_ID" --arg revision "$source_revision" --arg image "$image_ref" --arg digest "$(jq -r '.[0].Id' "$EVIDENCE_DIR/image-inspect.raw.json")" \
    --arg contextHash "$context_hash" --arg caSecret "$CA_SECRET" --arg caSecretUID "$ca_uid" \
    --argjson namespaceInjectionChanged "$(jq -r '.namespaceInjection.changed' "$STATE_FILE")" \
    '{status:"prepared",run:$run,sourceRevision:$revision,cleanupImage:$image,localImageID:$digest,containerfileSHA256:$contextHash,
      runOwnedPublicCABundleSecret:$caSecret,caSecretUID:$caSecretUID,controllerRolloutVerified:true,
      maasApiTLSHostnameVerified:true,originalControllerEnvSnapshotted:true,
      namespaceUIDSnapshotted:true,namespaceInjectionAdjusted:$namespaceInjectionChanged}' >"$EVIDENCE_DIR/RESULT.json"
  log 'Kind MaaS cleanup prerequisites are ready; no tenant has been removed'
  exit 0
fi

# Restore mode: use the private state journal, never infer the original values.
[[ -f "$STATE_FILE" ]] || fail 'cleanup state journal is absent'
state="$(cat "$STATE_FILE")"
jq -e --arg run "$RUN_ID" '.schema=="praxis-extproc.kind-maas-cleanup-state/v2" and .run==$run and .prepared==true and .restored==false' <<<"$state" >/dev/null || fail 'cleanup state journal has the wrong run, is incomplete, or is already restored'
controller_uid="$(jq -r '.controllerUID' <<<"$state")"
ca_uid="$(jq -r '.caSecretUID' <<<"$state")"
image_ref="$(jq -r '.image' <<<"$state")"
container_index="$(jq -r '.containerIndex' <<<"$state")"
env_index="$(jq -r '.envIndex' <<<"$state")"
original_env="$(jq -c '.originalCleanupImageEnv' <<<"$state")"
for tenant in pool-a pool-b; do
  tenant_object="$(k get aitenant "$tenant" -n ai-tenants --ignore-not-found -o json)" || fail "could not verify AITenant/$tenant cleanup"
  [[ -z "$tenant_object" ]] || fail "AITenant/$tenant remains; restore only after both revocation finalizers finish"
done
namespace_before_restore="$(k get namespace "$NAMESPACE" -o json)" || fail 'MaaS namespace disappeared before restore'
printf '%s\n' "$namespace_before_restore" | jq '{name:.metadata.name,uid:.metadata.uid,labels:{"istio-injection":.metadata.labels["istio-injection"],"istio.io/rev":.metadata.labels["istio.io/rev"],"istio.io/dataplane-mode":.metadata.labels["istio.io/dataplane-mode"]}}' \
  >"$EVIDENCE_DIR/maas-namespace-injection-before-restore.json"
namespace_patch="$(python3 "$SOURCE_DIR/kind_maas_injection.py" restore-patch \
  --state <(jq -c '.namespaceInjection' <<<"$state") \
  --current <(printf '%s\n' "$namespace_before_restore"))" || fail 'MaaS namespace injection state changed unexpectedly before restore'
if [[ "$namespace_patch" != '[]' ]]; then
  k patch namespace "$NAMESPACE" --type=json -p "$namespace_patch" \
    >"$EVIDENCE_DIR/maas-namespace-injection-restore-patch.json" || fail 'could not restore the original MaaS namespace injection label'
else
  printf 'already-original-or-unchanged\n' >"$EVIDENCE_DIR/maas-namespace-injection-restore-patch.json"
fi
namespace_after_restore="$(k get namespace "$NAMESPACE" -o json)" || fail 'MaaS namespace disappeared after injection-label restore'
python3 "$SOURCE_DIR/kind_maas_injection.py" restore-patch \
  --state <(jq -c '.namespaceInjection' <<<"$state") \
  --current <(printf '%s\n' "$namespace_after_restore") >/dev/null || fail 'MaaS namespace injection label did not restore to its exact original state'
printf '%s\n' "$namespace_after_restore" | jq '{name:.metadata.name,uid:.metadata.uid,labels:{"istio-injection":.metadata.labels["istio-injection"],"istio.io/rev":.metadata.labels["istio.io/rev"],"istio.io/dataplane-mode":.metadata.labels["istio.io/dataplane-mode"]}}' \
  >"$EVIDENCE_DIR/maas-namespace-injection-after-restore.json"
controller_before_restore="$(k get deployment "$CONTROLLER" -n "$NAMESPACE" -o json)" || fail 'MaaS controller disappeared before restore'
jq -e --arg uid "$controller_uid" '.metadata.uid==$uid' <<<"$controller_before_restore" >/dev/null || fail 'MaaS controller UID changed before restore'
jq -e --argjson ci "$container_index" --argjson ei "$env_index" --argjson env "$original_env" \
  '.spec.template.spec.containers[$ci].env[$ei]==$env' <<<"$controller_before_restore" >/dev/null && current_is_original=true || current_is_original=false
if [[ "$current_is_original" == true ]]; then
  printf 'already-restored\n' >"$EVIDENCE_DIR/maas-controller-restore-mode.txt"
else
  jq -e --arg image "$image_ref" --argjson ci "$container_index" --argjson ei "$env_index" '.spec.template.spec.containers[$ci].env[$ei].value==$image and .spec.template.spec.containers[$ci].env[$ei].name=="RELATED_IMAGE_UBI_MINIMAL_IMAGE"' \
    <<<"$controller_before_restore" >/dev/null || fail 'MaaS cleanup image override changed unexpectedly before restore'
  patch="$(jq -nc --arg uid "$controller_uid" --arg env "$original_env" --argjson ci "$container_index" --argjson ei "$env_index" \
    '[{op:"test",path:"/metadata/uid",value:$uid},{op:"replace",path:("/spec/template/spec/containers/"+($ci|tostring)+"/env/"+($ei|tostring)),value:($env|fromjson)}]')"
  k patch deployment "$CONTROLLER" -n "$NAMESPACE" --type=json -p "$patch" >"$EVIDENCE_DIR/maas-controller-image-restore-patch.json" || fail 'could not restore the MaaS controller cleanup image environment entry'
  k rollout status deployment/"$CONTROLLER" -n "$NAMESPACE" --timeout=180s >"$EVIDENCE_DIR/maas-controller-restore-rollout.txt" || fail 'MaaS controller did not converge after restoring its cleanup image'
  printf 'restored-by-this-invocation\n' >"$EVIDENCE_DIR/maas-controller-restore-mode.txt"
fi
controller_after_restore="$(k get deployment "$CONTROLLER" -n "$NAMESPACE" -o json)" || fail 'MaaS controller disappeared after restore'
jq -e --arg uid "$controller_uid" --argjson ci "$container_index" --argjson ei "$env_index" --argjson env "$original_env" \
  '.metadata.uid==$uid and .metadata.generation==.status.observedGeneration and .spec.template.spec.containers[$ci].env[$ei]==$env' \
  <<<"$controller_after_restore" >/dev/null || fail 'MaaS controller cleanup image environment did not restore exactly'
jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,
     observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,
     cleanupImageEnv:[.spec.template.spec.containers[].env[]?|select(.name=="RELATED_IMAGE_UBI_MINIMAL_IMAGE")]}' \
  <<<"$controller_after_restore" >"$EVIDENCE_DIR/maas-controller-after-restore.json"
[[ "$(jq -r '.status.readyReplicas' <<<"$controller_after_restore")" -ge 1 ]] || fail 'MaaS controller has no Ready replica after restore'

if [[ "$(jq -r '.caSecretCreatedByThisRun' <<<"$state")" == true ]]; then
  secret="$(k get secret "$CA_SECRET" -n "$NAMESPACE" --ignore-not-found -o json)" || fail 'could not inspect the run-created MaaS CA bundle Secret'
  if [[ -n "$secret" ]]; then
    jq -e --arg run "$RUN_LABEL" --arg uid "$ca_uid" '.metadata.uid==$uid and .metadata.labels["praxis.fast/qualification-run"]==$run' <<<"$secret" >/dev/null || fail 'MaaS CA bundle Secret UID or run label changed before restore'
    jq '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,keys:((.data//{})|keys)}' \
      <<<"$secret" >"$EVIDENCE_DIR/maas-api-ca-secret-before-delete.json"
    command kubectl --context="$CONTEXT" proxy --address=127.0.0.1 --port=0 >"$EVIDENCE_DIR/api-proxy.log" 2>&1 &
    API_PROXY_PID=$!
    api_port=""
    for _ in $(seq 1 50); do
      api_port="$(sed -nE 's/.*127\.0\.0\.1:([0-9]+).*/\1/p' "$EVIDENCE_DIR/api-proxy.log" | head -n 1)"
      if [[ -n "$api_port" ]] && curl -fsS --max-time 1 "http://127.0.0.1:$api_port/readyz" >/dev/null 2>&1; then break; fi
      sleep 0.2
    done
    [[ -n "$api_port" ]] || fail 'loopback Kubernetes API proxy did not become ready'
    jq -n --arg uid "$ca_uid" '{apiVersion:"v1",kind:"DeleteOptions",preconditions:{uid:$uid}}' >"$EVIDENCE_DIR/maas-api-ca-secret-delete-options.json"
    http_status="$(curl -sS --max-time 30 -o "$EVIDENCE_DIR/maas-api-ca-secret-delete-response.json" -w '%{http_code}' \
      -H 'Content-Type: application/json' -X DELETE --data-binary "@$EVIDENCE_DIR/maas-api-ca-secret-delete-options.json" \
      "http://127.0.0.1:$api_port/api/v1/namespaces/$NAMESPACE/secrets/$CA_SECRET")" || fail 'UID-preconditioned MaaS CA Secret deletion failed'
    [[ "$http_status" == 200 || "$http_status" == 202 ]] || fail "MaaS CA Secret delete returned HTTP $http_status"
    jq -e --arg uid "$ca_uid" 'if .metadata.uid then .metadata.uid==$uid else .kind=="Status" and .status=="Success" end' \
      "$EVIDENCE_DIR/maas-api-ca-secret-delete-response.json" >/dev/null || fail 'MaaS CA Secret DELETE returned an invalid response'
    printf '%s\n' "$http_status" >"$EVIDENCE_DIR/maas-api-ca-secret-http-status.txt"
    for _ in $(seq 1 50); do
      [[ -z "$(k get secret "$CA_SECRET" -n "$NAMESPACE" --ignore-not-found -o name)" ]] && break
      sleep 0.2
    done
    [[ -z "$(k get secret "$CA_SECRET" -n "$NAMESPACE" --ignore-not-found -o name)" ]] || fail 'MaaS CA Secret remains after UID-preconditioned deletion'
    printf 'removed-by-restore\n' >"$EVIDENCE_DIR/maas-api-ca-secret-final-state.txt"
  else
    printf 'already-removed-after-tenant-cleanup\n' >"$EVIDENCE_DIR/maas-api-ca-secret-final-state.txt"
  fi
fi

jq '.restored=true' <<<"$state" >"$STATE_FILE.tmp"
mv "$STATE_FILE.tmp" "$STATE_FILE"
jq -n --arg run "$RUN_ID" --arg controllerUid "$controller_uid" --arg caSecretUID "$ca_uid" \
  --argjson caSecretCreated "$(jq -r '.caSecretCreatedByThisRun' <<<"$state")" \
  --arg namespaceUID "$(jq -r '.namespaceInjection.uid' <<<"$state")" \
  --argjson namespaceInjectionChanged "$(jq -r '.namespaceInjection.changed' <<<"$state")" \
  '{status:"restored",run:$run,controllerUID:$controllerUid,caSecretUID:$caSecretUID,
    originalControllerEnvRestored:true,runCreatedCABundleSecretRemoved:$caSecretCreated,controllerRolloutVerified:true,
    namespaceUID:$namespaceUID,originalNamespaceInjectionLabelRestored:true,namespaceInjectionChanged:$namespaceInjectionChanged}' >"$EVIDENCE_DIR/RESULT.json"
log 'Original MaaS cleanup image configuration and namespace injection label were restored; the run-created CA bundle was removed'
