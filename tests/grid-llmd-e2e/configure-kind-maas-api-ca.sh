#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Create the Kind MaaS API serving CA and configure Authorino to verify it.
set -Eeuo pipefail
umask 077

MODE="${KIND_MAAS_API_CA_MODE:-install}"
CONTEXT="${KUBE_CONTEXT:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
RUN_ID="${E2E_RUN_ID:-${GRID_RUN_LABEL:-}}"
RUN_LABEL="${GRID_RUN_LABEL:-$RUN_ID}"
CLUSTER="${KIND_CLUSTER_NAME:-}"
STATE_FILE="${KIND_MAAS_API_CA_STATE_FILE:-}"
EVIDENCE_DIR="${KIND_MAAS_API_CA_EVIDENCE_DIR:-}"
RECOVERY_DIR="${KIND_MAAS_API_CA_RECOVERY_DIR:-}"
NAMESPACE="maas-system"
AUTHORINO_NAMESPACE="kuadrant-system"
AUTHORINO_CA_CONFIGMAP="authorino-maas-api-ca"
AUTHORINO_CA_PATH="/etc/ssl/certs/maas-api-serving-ca.crt"
MAAS_OPENSHIFT_CA_CONFIGMAP="openshift-service-ca.crt"
MAAS_OPENSHIFT_CA_KEY="service-ca.crt"
# MaaS main's API-key cleanup CronJobs expect this OpenShift service-CA name;
# Kind supplies a run-owned alias containing the verified MaaS API issuer CA.
API_ISSUER="rhai-ca-issuer"
API_ROOT_NAMESPACE="cert-manager"
TIMEOUT="${KIND_MAAS_API_CA_TIMEOUT:-5m}"

[[ "$MODE" =~ ^(install|cleanup)$ ]] || { printf 'KIND_MAAS_API_CA_MODE must be install or cleanup\n' >&2; exit 2; }
for bin in kubectl kind jq openssl sha256sum base64; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ -n "$CONTEXT" && -n "$EXPECTED_SERVER" && -n "$RUN_ID" && "$RUN_LABEL" == "$RUN_ID" && -n "$CLUSTER" && -n "$STATE_FILE" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, EXPECTED_API_SERVER, E2E_RUN_ID, KIND_CLUSTER_NAME, state file, and evidence directory are required\n' >&2; exit 2;
}
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,80}$ ]] || { printf 'unsafe run identifier\n' >&2; exit 2; }
[[ "$TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'KIND_MAAS_API_CA_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR" "$(dirname "$STATE_FILE")"
chmod 0700 "$EVIDENCE_DIR" "$(dirname "$STATE_FILE")"

API_CERT="maas-api-serving-cert"
API_SECRET="maas-api-serving-cert"

k() { command kubectl --kubeconfig="${KUBECONFIG:?KUBECONFIG must point to the private run kubeconfig}" --context="$CONTEXT" --request-timeout=20s "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
capture_failure() {
  k get certificate,issuer -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/certificates-on-failure.json" 2>&1 || true
  k get clusterissuer -o json >"$EVIDENCE_DIR/clusterissuers-on-failure.json" 2>&1 || true
  k get authorino authorino -n "$AUTHORINO_NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},spec:.spec,status:.status}' \
    >"$EVIDENCE_DIR/authorino-on-failure.json" 2>&1 || true
  k get deployment authorino -n "$AUTHORINO_NAMESPACE" -o json |
    jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},spec:{template:{spec:{containers:[.spec.template.spec.containers[]|{name,env,volumeMounts}]}}},status:.status}' \
    >"$EVIDENCE_DIR/authorino-deployment-on-failure.json" 2>&1 || true
  k get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json |
    jq '[.items[-60:][]|{reason:.reason,message:.message,object:.involvedObject,lastTimestamp:.lastTimestamp}]' \
    >"$EVIDENCE_DIR/events-on-failure.json" 2>&1 || true
}
on_error() {
  local code="$1"
  trap - ERR
  capture_failure
  jq -n --arg mode "$MODE" --arg run "$RUN_ID" --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:"failed",run:$run,exitCode:$exitCode,evidenceDirectory:$evidence,privateKeyBytesRecorded:false}' >"$EVIDENCE_DIR/RESULT.json"
  log "MaaS API CA mode $MODE failed with exit=$code"
  exit "$code"
}
trap 'on_error $?' ERR

server="$(k config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
[[ "$server" == "$EXPECTED_SERVER" ]] || { printf 'Kubernetes API identity mismatch\n' >&2; exit 1; }
[[ "$(k get --raw=/readyz)" == *ok* ]] || { printf 'Kubernetes API is not ready\n' >&2; exit 1; }
kind get clusters | grep -Fxq "$CLUSTER" || { printf 'run-owned Kind cluster is absent\n' >&2; exit 1; }
k get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/maas-namespace.json"
k get deployment cert-manager cert-manager-webhook cert-manager-cainjector -n cert-manager -o json |
  jq '[.items[]|{name:.metadata.name,generation:.metadata.generation,observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas}]' \
  >"$EVIDENCE_DIR/cert-manager-readiness.json"
jq -e 'length==3 and all(.[];.observedGeneration==.generation and (.readyReplicas//0)>=1)' \
  "$EVIDENCE_DIR/cert-manager-readiness.json" >/dev/null || { printf 'cert-manager is not fully Ready\n' >&2; exit 1; }
k get authorino authorino -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-before.json"
k get deployment authorino -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-deployment-before.json"

if [[ "$MODE" == cleanup ]]; then
  [[ -s "$STATE_FILE" ]] || { printf 'run state file is required for cleanup\n' >&2; exit 2; }
  jq -e --arg run "$RUN_ID" '.runId==$run and .authorino.configMapCreated==true and .apiCA.controllerManaged==true' "$STATE_FILE" >/dev/null || {
    printf 'cleanup only supports a ConfigMap created by this run; state does not match\n' >&2; exit 1;
  }
  authorino_uid="$(jq -r '.authorino.uid' "$STATE_FILE")"
  k get authorino authorino -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-before-cleanup.json"
  jq -e --arg uid "$authorino_uid" '.metadata.uid==$uid' "$EVIDENCE_DIR/authorino-before-cleanup.json" >/dev/null || {
    printf 'Authorino UID changed; refusing to restore another resource\n' >&2; exit 1;
  }
  k get certificate "$API_CERT" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/api-certificate-before-cleanup.json"
  jq -e --arg uid "$(jq -r '.apiCA.certificateUID' "$STATE_FILE")" \
    '.metadata.uid==$uid and any(.status.conditions[]?;.type=="Ready" and .status=="True")' \
    "$EVIDENCE_DIR/api-certificate-before-cleanup.json" >/dev/null || {
    printf 'MaaS controller-owned API Certificate UID/readiness changed; refusing cleanup\n' >&2; exit 1;
  }
  k get clusterissuer "$API_ISSUER" -o json >"$EVIDENCE_DIR/api-issuer-before-cleanup.json"
  jq -e --arg uid "$(jq -r '.apiCA.issuerUID' "$STATE_FILE")" --arg run "$RUN_ID" \
    '.metadata.uid==$uid and .metadata.labels["praxis.fast/qualification-run"]==$run' \
    "$EVIDENCE_DIR/api-issuer-before-cleanup.json" >/dev/null || {
    printf 'MaaS API issuer UID/ownership changed; refusing cleanup\n' >&2; exit 1;
  }
  service_ca_cm_uid=""
  if jq -e '.maasServiceCA.configMapCreated==true' "$STATE_FILE" >/dev/null 2>&1; then
    service_ca_cm_json="$(k get configmap "$MAAS_OPENSHIFT_CA_CONFIGMAP" -n "$NAMESPACE" -o json)"
    service_ca_cm_uid="$(jq -r '.maasServiceCA.configMapUID' "$STATE_FILE")"
    jq -e --arg uid "$service_ca_cm_uid" --arg run "$RUN_ID" \
      '.metadata.uid==$uid and .metadata.labels["praxis.kind/run"]==$run and (.data|keys)==["service-ca.crt"]' \
      <<<"$service_ca_cm_json" >/dev/null || {
      printf 'MaaS service CA ConfigMap ownership/UID changed; refusing to treat it as this run\n' >&2; exit 1;
    }
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},dataKeys:(.data|keys)}' \
      <<<"$service_ca_cm_json" >"$EVIDENCE_DIR/maas-service-ca-configmap-before-cleanup.json"
  fi
  original_volumes="$(jq -c '.authorino.originalVolumes' "$STATE_FILE")"
  jq -n --argjson volumes "$original_volumes" '{spec:{volumes:$volumes}}' >"$EVIDENCE_DIR/authorino-volumes-restore-patch.json"
  k patch authorino authorino -n "$AUTHORINO_NAMESPACE" --type=merge --patch-file "$EVIDENCE_DIR/authorino-volumes-restore-patch.json" \
    >"$EVIDENCE_DIR/authorino-volumes-restore.txt"
  original_ssl="$(jq -r '.authorino.originalEnv.SSL_CERT_FILE // ""' "$STATE_FILE")"
  original_requests="$(jq -r '.authorino.originalEnv.REQUESTS_CA_BUNDLE // ""' "$STATE_FILE")"
  if [[ -n "$original_ssl" ]]; then
    k set env deployment/authorino -n "$AUTHORINO_NAMESPACE" "SSL_CERT_FILE=$original_ssl" >"$EVIDENCE_DIR/authorino-env-restore.txt"
  else
    k set env deployment/authorino -n "$AUTHORINO_NAMESPACE" SSL_CERT_FILE- >"$EVIDENCE_DIR/authorino-env-restore.txt"
  fi
  if [[ -n "$original_requests" ]]; then
    k set env deployment/authorino -n "$AUTHORINO_NAMESPACE" "REQUESTS_CA_BUNDLE=$original_requests" >>"$EVIDENCE_DIR/authorino-env-restore.txt"
  else
    k set env deployment/authorino -n "$AUTHORINO_NAMESPACE" REQUESTS_CA_BUNDLE- >>"$EVIDENCE_DIR/authorino-env-restore.txt"
  fi
  k rollout status deployment/authorino -n "$AUTHORINO_NAMESPACE" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/authorino-restore-rollout.txt"
  k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/configmap-before-delete.json"
  jq -e --arg uid "$(jq -r '.authorino.configMapUID' "$STATE_FILE")" --arg run "$RUN_ID" \
    '.metadata.uid==$uid and .metadata.labels["praxis.kind/run"]==$run' "$EVIDENCE_DIR/configmap-before-delete.json" >/dev/null || {
    printf 'CA ConfigMap ownership/UID changed; refusing deletion\n' >&2; exit 1;
  }
  k delete configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" --wait=true >"$EVIDENCE_DIR/configmap-delete.txt"
  stale_cert="$EVIDENCE_DIR/stale-run-certificate.json"
  stale_cert_status=0
  k get certificate maas-api-serving-certificate -n "$NAMESPACE" -o json >"$stale_cert" 2>/dev/null || stale_cert_status=$?
  if (( stale_cert_status == 0 )); then
    jq -e --arg run "$RUN_ID" --arg secret "$API_SECRET" \
      '.metadata.labels["praxis.kind/run"]==$run and .spec.secretName==$secret and .metadata.name=="maas-api-serving-certificate"' \
      "$stale_cert" >/dev/null || { printf 'refusing to delete an unowned or unexpected MaaS API certificate\n' >&2; exit 1; }
    k delete certificate maas-api-serving-certificate -n "$NAMESPACE" --wait=true >"$EVIDENCE_DIR/stale-run-certificate-delete.txt"
  fi
  jq -n --arg run "$RUN_ID" --arg authorinoUid "$authorino_uid" --arg serviceCAUid "$service_ca_cm_uid" \
    '{mode:"cleanup",status:"passed",run:$run,authorinoUID:$authorinoUid,serviceCAConfigMapUID:(if $serviceCAUid=="" then null else $serviceCAUid end),serviceCAConfigMapPreservedUntilClusterDestroy:($serviceCAUid!=""),privateKeyBytesRecorded:false}' >"$EVIDENCE_DIR/RESULT.json"
  log 'Restored Authorino volumes and trust environment on the same UID; any run-owned MaaS service CA alias remains only until Kind cluster destruction'
  exit 0
fi

if [[ -s "$STATE_FILE" ]]; then
  jq -e --arg run "$RUN_ID" '.runId==$run and .authorino.configMapCreated==true and .apiCA.controllerManaged==true' "$STATE_FILE" >/dev/null || {
    printf 'existing state file does not match this run; refusing re-install\n' >&2; exit 1;
  }
  original_volumes="$(jq -c '.authorino.originalVolumes' "$STATE_FILE")"
  original_ssl="$(jq -r '.authorino.originalEnv.SSL_CERT_FILE // ""' "$STATE_FILE")"
  original_requests="$(jq -r '.authorino.originalEnv.REQUESTS_CA_BUNDLE // ""' "$STATE_FILE")"
  jq -e --arg run "$RUN_ID" '.maasServiceCA.configMapCreated==true and .runId==$run' "$STATE_FILE" >/dev/null || {
    printf 'existing state predates the Kind MaaS service CA alias; use a fresh run rather than adopting the state\n' >&2; exit 1;
  }
elif [[ -n "$RECOVERY_DIR" ]]; then
  [[ -s "$RECOVERY_DIR/authorino-before.json" && -s "$RECOVERY_DIR/authorino-deployment-before.json" ]] || {
    printf 'recovery directory must contain the original Authorino snapshots\n' >&2; exit 2;
  }
  cp "$RECOVERY_DIR/authorino-before.json" "$EVIDENCE_DIR/authorino-before-recovered.json"
  cp "$RECOVERY_DIR/authorino-deployment-before.json" "$EVIDENCE_DIR/authorino-deployment-before-recovered.json"
  jq -e --arg uid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/authorino-before.json")" \
    '.metadata.uid==$uid' "$EVIDENCE_DIR/authorino-before-recovered.json" >/dev/null
  original_volumes="$(jq -c '.spec.volumes // {}' "$EVIDENCE_DIR/authorino-before-recovered.json")"
  original_ssl="$(jq -r '[.spec.template.spec.containers[]|select(.name=="authorino")|.env[]?|select(.name=="SSL_CERT_FILE")|.value][0] // ""' "$EVIDENCE_DIR/authorino-deployment-before-recovered.json")"
  original_requests="$(jq -r '[.spec.template.spec.containers[]|select(.name=="authorino")|.env[]?|select(.name=="REQUESTS_CA_BUNDLE")|.value][0] // ""' "$EVIDENCE_DIR/authorino-deployment-before-recovered.json")"
else
  existing_authorino_ca_cm="$(k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" --ignore-not-found -o json)" || {
    printf 'could not check Authorino MaaS CA ConfigMap state\n' >&2; exit 1;
  }
  if [[ -n "$existing_authorino_ca_cm" ]]; then
    jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},dataKeys:(.data|keys)}' \
      <<<"$existing_authorino_ca_cm" >"$EVIDENCE_DIR/authorino-ca-configmap-before.json"
    printf 'Authorino MaaS CA ConfigMap already exists; use an explicit recovery snapshot or investigate ownership\n' >&2; exit 1;
  fi
  original_volumes="$(jq -c '.spec.volumes // {}' "$EVIDENCE_DIR/authorino-before.json")"
  original_ssl="$(jq -r '[.spec.template.spec.containers[]|select(.name=="authorino")|.env[]?|select(.name=="SSL_CERT_FILE")|.value][0] // ""' "$EVIDENCE_DIR/authorino-deployment-before.json")"
  original_requests="$(jq -r '[.spec.template.spec.containers[]|select(.name=="authorino")|.env[]?|select(.name=="REQUESTS_CA_BUNDLE")|.value][0] // ""' "$EVIDENCE_DIR/authorino-deployment-before.json")"
fi

existing_cm_status=0
k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o json \
  >"$EVIDENCE_DIR/authorino-ca-configmap-before-apply.json" 2>/dev/null || existing_cm_status=$?
if (( existing_cm_status == 0 )); then
  jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run' \
    "$EVIDENCE_DIR/authorino-ca-configmap-before-apply.json" >/dev/null || {
    printf 'existing Authorino CA ConfigMap is not owned by this run; refusing to adopt it\n' >&2; exit 1;
  }
  if [[ -s "$STATE_FILE" ]]; then
    jq -e --arg uid "$(jq -r '.authorino.configMapUID' "$STATE_FILE")" \
      '.metadata.uid==$uid' "$EVIDENCE_DIR/authorino-ca-configmap-before-apply.json" >/dev/null || {
      printf 'Authorino CA ConfigMap UID changed; refusing re-install\n' >&2; exit 1;
    }
  fi
elif [[ -z "$RECOVERY_DIR" && -s "$STATE_FILE" ]]; then
  printf 'recorded Authorino CA ConfigMap is missing; refusing to recreate it silently\n' >&2; exit 1
fi

issuer_json="$EVIDENCE_DIR/api-cluster-issuer.json"
k get clusterissuer "$API_ISSUER" -o json >"$issuer_json"
jq -e --arg run "$RUN_ID" \
  '.metadata.labels["praxis.fast/qualification-run"]==$run and
   any(.status.conditions[]?;.type=="Ready" and .status=="True") and
   (.spec.ca.secretName|type=="string" and length>0)' "$issuer_json" >/dev/null || {
  printf 'MaaS API issuer is not the current run-owned ready ClusterIssuer/rhai-ca-issuer\n' >&2; exit 1;
}
ROOT_SECRET="$(jq -r '.spec.ca.secretName' "$issuer_json")"
root_secret_json="$EVIDENCE_DIR/api-root-secret-identity.json"
k get secret "$ROOT_SECRET" -n "$API_ROOT_NAMESPACE" -o json \
  | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},keys:((.data//{})|keys),hasTLSCert:((.data//{})|has("tls.crt")),hasTLSKey:((.data//{})|has("tls.key"))}' \
  >"$root_secret_json"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.fast/qualification-run"]==$run and .hasTLSCert and .hasTLSKey' "$root_secret_json" >/dev/null || {
  printf 'MaaS API issuer CA Secret is not complete and owned by this run\n' >&2; exit 1;
}
k get secret "$ROOT_SECRET" -n "$API_ROOT_NAMESPACE" -o jsonpath='{.data.tls\.crt}' | base64 -d >"$(dirname "$STATE_FILE")/maas-api-root-ca.crt"
k wait --for=condition=Ready "certificate/$API_CERT" -n "$NAMESPACE" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/api-certificate-ready.txt"
k get certificate "$API_CERT" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/api-certificate-live.json"
jq -e --arg secret "$API_SECRET" --arg issuer "$API_ISSUER" '
  .spec.secretName==$secret and .spec.issuerRef.kind=="ClusterIssuer" and .spec.issuerRef.name==$issuer and
  any(.metadata.ownerReferences[]?;.controller==true and .kind=="Config" and .name=="default") and
  (.spec.dnsNames|index("maas-api.maas-system.svc")!=null) and
  (.spec.dnsNames|index("maas-api.maas-system.svc.cluster.local")!=null) and
  any(.status.conditions[]?;.type=="Ready" and .status=="True")
' "$EVIDENCE_DIR/api-certificate-live.json" >/dev/null || {
  printf 'MaaS controller-owned API Certificate does not match its Service, issuer, owner, or Ready contract\n' >&2; exit 1;
}
k get secret "$API_SECRET" -n "$NAMESPACE" -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},type:.type,keys:((.data//{})|keys),hasTLSCert:((.data//{})|has("tls.crt")),hasTLSKey:((.data//{})|has("tls.key"))}' >"$EVIDENCE_DIR/api-secret-identity.json"
k get secret "$API_SECRET" -n "$NAMESPACE" -o jsonpath='{.data.tls\.crt}' | base64 -d >"$(dirname "$STATE_FILE")/maas-api-serving.crt"
chmod 0600 "$(dirname "$STATE_FILE")/maas-api-root-ca.crt" "$(dirname "$STATE_FILE")/maas-api-serving.crt"
openssl verify -CAfile "$(dirname "$STATE_FILE")/maas-api-root-ca.crt" -verify_hostname maas-api.maas-system.svc "$(dirname "$STATE_FILE")/maas-api-serving.crt" \
  >"$EVIDENCE_DIR/api-certificate-verify.txt"
openssl verify -CAfile "$(dirname "$STATE_FILE")/maas-api-root-ca.crt" -verify_hostname maas-api.maas-system.svc.cluster.local "$(dirname "$STATE_FILE")/maas-api-serving.crt" \
  >"$EVIDENCE_DIR/api-certificate-cluster-local-verify.txt"
openssl x509 -in "$(dirname "$STATE_FILE")/maas-api-serving.crt" -noout -fingerprint -sha256 -subject -issuer -ext subjectAltName \
  >"$EVIDENCE_DIR/api-certificate-public-identity.txt"
api_cert_pub_sha="$(openssl x509 -in "$(dirname "$STATE_FILE")/maas-api-serving.crt" -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum | awk '{print $1}')"
api_key_pub_sha="$(k get secret "$API_SECRET" -n "$NAMESPACE" -o jsonpath='{.data.tls\.key}' | base64 -d | openssl pkey -pubout -outform DER | sha256sum | awk '{print $1}')"
[[ "$api_cert_pub_sha" == "$api_key_pub_sha" ]] || { printf 'MaaS API serving certificate and private key do not match\n' >&2; exit 1; }
printf '%s\n' "$api_cert_pub_sha" >"$EVIDENCE_DIR/api-certificate-key-public-key-sha256.txt"

# MaaS API-key cleanup CronJobs use the OpenShift service CA ConfigMap name.
# On Kind, provide the same verified public CA under a run-owned alias. The
# cleanup request URL must use the API Service's certificate-covered FQDN.
service_ca_cm_before_json="$(k get configmap "$MAAS_OPENSHIFT_CA_CONFIGMAP" -n "$NAMESPACE" --ignore-not-found -o json)" || {
  printf 'could not check MaaS service CA ConfigMap state\n' >&2; exit 1;
}
expected_service_ca_sha="$(sha256sum "$(dirname "$STATE_FILE")/maas-api-root-ca.crt" | cut -d' ' -f1)"
if [[ -n "$service_ca_cm_before_json" ]]; then
  jq -e --arg run "$RUN_ID" \
    '.metadata.labels["praxis.kind/run"]==$run and (.data|keys)==["service-ca.crt"]' \
    <<<"$service_ca_cm_before_json" >/dev/null || {
    printf 'existing MaaS service CA ConfigMap is not the exact run-owned alias\n' >&2; exit 1;
  }
  existing_service_ca_sha="$(jq -j --arg key "$MAAS_OPENSHIFT_CA_KEY" '.data[$key]' <<<"$service_ca_cm_before_json" | sha256sum | cut -d' ' -f1)"
  [[ "$existing_service_ca_sha" == "$expected_service_ca_sha" ]] || {
    printf 'existing run-owned MaaS service CA alias does not contain the current issuer CA\n' >&2; exit 1;
  }
  if [[ -s "$STATE_FILE" ]]; then
    jq -e --arg uid "$(jq -r '.maasServiceCA.configMapUID' "$STATE_FILE")" \
      '.metadata.uid==$uid' <<<"$service_ca_cm_before_json" >/dev/null || {
      printf 'MaaS service CA ConfigMap UID changed; refusing re-install\n' >&2; exit 1;
    }
  else
    log 'Recovered a partially applied service CA alias because its run label and public CA digest match the current issuer'
  fi
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},dataKeys:(.data|keys)}' \
    <<<"$service_ca_cm_before_json" >"$EVIDENCE_DIR/maas-service-ca-configmap-before.json"
else
  [[ ! -s "$STATE_FILE" ]] || { printf 'recorded MaaS service CA ConfigMap is missing; refusing to recreate it silently\n' >&2; exit 1; }
  k create configmap "$MAAS_OPENSHIFT_CA_CONFIGMAP" -n "$NAMESPACE" \
    --from-file="$MAAS_OPENSHIFT_CA_KEY=$(dirname "$STATE_FILE")/maas-api-root-ca.crt" --dry-run=client -o json \
    | jq --arg run "$RUN_ID" '.metadata.labels={"praxis.kind/run":$run,"app.kubernetes.io/managed-by":"grid-llmd-kind","app.kubernetes.io/part-of":"grid-llmd-e2e"}' \
    | k apply --dry-run=server -f - >"$EVIDENCE_DIR/maas-service-ca-configmap-server-dry-run.txt"
  k create configmap "$MAAS_OPENSHIFT_CA_CONFIGMAP" -n "$NAMESPACE" \
    --from-file="$MAAS_OPENSHIFT_CA_KEY=$(dirname "$STATE_FILE")/maas-api-root-ca.crt" --dry-run=client -o json \
    | jq --arg run "$RUN_ID" '.metadata.labels={"praxis.kind/run":$run,"app.kubernetes.io/managed-by":"grid-llmd-kind","app.kubernetes.io/part-of":"grid-llmd-e2e"}' \
    | k apply -f - >"$EVIDENCE_DIR/maas-service-ca-configmap-apply.txt"
fi
service_ca_cm_json="$(k get configmap "$MAAS_OPENSHIFT_CA_CONFIGMAP" -n "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run and (.data|keys)==["service-ca.crt"]' \
  <<<"$service_ca_cm_json" >/dev/null || { printf 'MaaS service CA alias has unexpected ownership or data keys\n' >&2; exit 1; }
actual_service_ca_sha="$(jq -j --arg key "$MAAS_OPENSHIFT_CA_KEY" '.data[$key]' <<<"$service_ca_cm_json" | sha256sum | cut -d' ' -f1)"
[[ "$actual_service_ca_sha" == "$expected_service_ca_sha" ]] || { printf 'MaaS service CA alias digest does not match the run API CA\n' >&2; exit 1; }
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},dataKeys:(.data|keys)}' \
  <<<"$service_ca_cm_json" >"$EVIDENCE_DIR/maas-service-ca-configmap-summary.json"

k create configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" --from-file="ca.crt=$(dirname "$STATE_FILE")/maas-api-root-ca.crt" \
  --dry-run=client -o json | jq --arg run "$RUN_ID" '.metadata.labels={"praxis.kind/run":$run,"app.kubernetes.io/managed-by":"grid-llmd-kind"}' \
  >"$EVIDENCE_DIR/authorino-ca-configmap.yaml"
k apply --dry-run=server -f "$EVIDENCE_DIR/authorino-ca-configmap.yaml" >"$EVIDENCE_DIR/authorino-ca-configmap-server-dry-run.txt"
k apply -f "$EVIDENCE_DIR/authorino-ca-configmap.yaml" >"$EVIDENCE_DIR/authorino-ca-configmap-apply.txt"
jq -n --arg run "$RUN_ID" --arg authorinoUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/authorino-before.json")" \
  --arg cmUid "$(k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o jsonpath='{.metadata.uid}')" \
  --arg serviceCACmUid "$(jq -r '.metadata.uid' <<<"$service_ca_cm_json")" \
  --arg serviceCASha "$actual_service_ca_sha" \
  --arg caIssuer "$API_ISSUER" --arg apiCertificate "$API_CERT" --arg apiSecret "$API_SECRET" \
  --arg issuerUid "$(jq -r '.metadata.uid' "$issuer_json")" \
  --arg rootSecret "$ROOT_SECRET" --arg rootSecretUid "$(jq -r '.metadata.uid' "$root_secret_json")" \
  --arg apiCertificateUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/api-certificate-live.json")" \
  --arg apiSecretUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/api-secret-identity.json")" \
  --argjson volumes "$original_volumes" --arg ssl "$original_ssl" --arg requests "$original_requests" \
  '{runId:$run,authorino:{uid:$authorinoUid,originalVolumes:$volumes,originalEnv:{SSL_CERT_FILE:$ssl,REQUESTS_CA_BUNDLE:$requests},configMapCreated:true,configMapUID:$cmUid},maasServiceCA:{configMapCreated:true,configMapUID:$serviceCACmUid,sha256:$serviceCASha},apiCA:{issuer:$caIssuer,issuerUID:$issuerUid,rootSecret:$rootSecret,rootSecretUID:$rootSecretUid,certificate:$apiCertificate,certificateUID:$apiCertificateUid,secret:$apiSecret,secretUID:$apiSecretUid,controllerManaged:true},privateKeyBytesRecorded:false}' \
  >"$STATE_FILE.tmp"
chmod 0600 "$STATE_FILE.tmp"
mv "$STATE_FILE.tmp" "$STATE_FILE"
jq -n --arg cm "$AUTHORINO_CA_CONFIGMAP" --arg path "$AUTHORINO_CA_PATH" \
  '{spec:{volumes:{defaultMode:420,items:[{name:"maas-api-serving-ca",mountPath:"/etc/ssl/certs",configMaps:[$cm],items:[{key:"ca.crt",path:($path|split("/")|last)}]}]}}}' \
  >"$EVIDENCE_DIR/authorino-volume-patch.json"
k patch authorino authorino -n "$AUTHORINO_NAMESPACE" --type=merge --patch-file "$EVIDENCE_DIR/authorino-volume-patch.json" \
  >"$EVIDENCE_DIR/authorino-volume-patch-result.txt"
k rollout status deployment/authorino -n "$AUTHORINO_NAMESPACE" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/authorino-volume-rollout.txt"
k set env deployment/authorino -n "$AUTHORINO_NAMESPACE" "SSL_CERT_FILE=$AUTHORINO_CA_PATH" "REQUESTS_CA_BUNDLE=$AUTHORINO_CA_PATH" \
  >"$EVIDENCE_DIR/authorino-env-set.txt"
k rollout status deployment/authorino -n "$AUTHORINO_NAMESPACE" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/authorino-env-rollout.txt"
k get authorino authorino -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-after.json"
k get deployment authorino -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-deployment-after.json"
k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-ca-configmap-after.json"
jq -e --arg run "$RUN_ID" \
  '.metadata.labels["praxis.kind/run"]==$run and (.data["ca.crt"]|type=="string" and length>0)' \
  "$EVIDENCE_DIR/authorino-ca-configmap-after.json" >/dev/null
jq -e --arg cm "$AUTHORINO_CA_CONFIGMAP" --arg path "$AUTHORINO_CA_PATH" \
  'any(.spec.volumes.items[]?;.mountPath=="/etc/ssl/certs" and any(.configMaps[]?;.==$cm) and any(.items[]?;.key=="ca.crt" and .path==($path|split("/")|last)))' \
  "$EVIDENCE_DIR/authorino-after.json" >/dev/null
jq -e --arg path "$AUTHORINO_CA_PATH" \
  'any(.spec.template.spec.containers[]?;.env|any(.[]?;.name=="SSL_CERT_FILE" and .value==$path) and any(.[]?;.name=="REQUESTS_CA_BUNDLE" and .value==$path)) and
   any(.spec.template.spec.containers[]?;.volumeMounts|any(.[]?;.mountPath=="/etc/ssl/certs" and .name=="maas-api-serving-ca"))' \
  "$EVIDENCE_DIR/authorino-deployment-after.json" >/dev/null
pod="$(k get pods -n "$AUTHORINO_NAMESPACE" -l authorino-resource=authorino,control-plane=controller-manager -o json | jq -er '[.items[]|select(.metadata.deletionTimestamp==null and .status.phase=="Running" and any(.status.conditions[]?;.type=="Ready" and .status=="True"))][0].metadata.name')"
k exec "$pod" -n "$AUTHORINO_NAMESPACE" -- test -s "$AUTHORINO_CA_PATH" >"$EVIDENCE_DIR/authorino-mounted-ca-check.txt"
k get "certificate/$API_CERT" -n "$NAMESPACE" -o json | jq '{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,ownerReferences:.metadata.ownerReferences,ready:.status.conditions,secretName:.spec.secretName,issuerRef:.spec.issuerRef,dnsNames:.spec.dnsNames}' >"$EVIDENCE_DIR/api-certificate-summary.json"
authorino_uid="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/authorino-before.json")"
cm_uid="$(jq -r '.metadata.uid' "$EVIDENCE_DIR/authorino-ca-configmap-after.json")"
jq -n --arg run "$RUN_ID" --arg authorinoUid "$authorino_uid" --arg cmUid "$cm_uid" \
  --arg fingerprint "$(openssl x509 -in "$(dirname "$STATE_FILE")/maas-api-root-ca.crt" -noout -fingerprint -sha256 | sed 's/^.*=//')" \
  '{mode:"install",status:"passed",run:$run,authorinoUID:$authorinoUid,configMapUID:$cmUid,apiCAFingerprintSHA256:$fingerprint,controllerManagedCertificateReused:true,tlsVerificationEnabled:true,privateKeyBytesRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
find "$EVIDENCE_DIR" -maxdepth 1 -type f -exec chmod 0600 {} +
log 'Verified the MaaS controller-owned API certificate and issuer, installed run-scoped CA trust aliases, and kept TLS verification enabled'
