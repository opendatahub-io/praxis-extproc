#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Provide the Kind-only cert-manager CA issuer expected by MaaS AITenant APIs.
set -Eeuo pipefail
umask 077

MODE="${KIND_MAAS_TENANT_CA_MODE:-plan}"
CONTEXT="${KUBE_CONTEXT:-}"
EXPECTED_SERVER="${EXPECTED_API_SERVER:-}"
RUN_ID="${E2E_RUN_ID:-${GRID_RUN_LABEL:-}}"
RUN_LABEL="${GRID_RUN_LABEL:-$RUN_ID}"
CLUSTER="${KIND_CLUSTER_NAME:-}"
NAMESPACE="${KIND_MAAS_TENANT_CA_NAMESPACE:-cert-manager}"
TIMEOUT="${KIND_MAAS_TENANT_CA_TIMEOUT:-5m}"
STATE_FILE="${KIND_MAAS_TENANT_CA_STATE_FILE:-}"
EVIDENCE_DIR="${KIND_MAAS_TENANT_CA_EVIDENCE_DIR:-}"

suffix="$(printf '%s' "$RUN_ID" | sha256sum | cut -c1-8)"
SELF_SIGNED_ISSUER="grid-llmd-$suffix-selfsigned"
ROOT_CERT="grid-llmd-$suffix-root"
ROOT_SECRET="$ROOT_CERT"
MAAS_ISSUER="rhai-ca-issuer"
AUTHORINO_NAMESPACE="kuadrant-system"
AUTHORINO_CA_CONFIGMAP="authorino-maas-api-ca"
AUTHORINO_DEPLOYMENT="authorino"
AUTHORINO_CA_PATH="/etc/ssl/certs/maas-api-serving-ca.crt"

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Kind MaaS tenant CA plan (no cluster calls):
  1. Verify the run-owned Kind cluster, API server, cert-manager CRDs, and controller readiness.
  2. Create a run-labeled self-signed ClusterIssuer and CA Certificate in cert-manager.
  3. Create the run-required ClusterIssuer/rhai-ca-issuer from that CA Secret.
  4. Add its public CA certificate to the run-owned Authorino trust bundle without replacing the existing MaaS API CA; keep TLS verification enabled.
  5. Wait for current certificate, issuer, and Authorino rollout readiness; record UIDs and public certificate fingerprints only.
  6. Cleanup restores the original Authorino trust bundle after all run-owned AITenants and leaf Certificates have naturally disappeared.
Private key bytes remain in the cert-manager Secret and are never written to evidence or local files.
PLAN
  exit 0
fi

[[ "$MODE" =~ ^(prepare-issuer|install|trust|verify|cleanup)$ ]] || { printf 'KIND_MAAS_TENANT_CA_MODE must be plan, prepare-issuer, install, trust, verify, or cleanup\n' >&2; exit 2; }
for bin in kubectl kind jq sha256sum openssl; do command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }; done
[[ -n "$CONTEXT" && -n "$EXPECTED_SERVER" && -n "$RUN_ID" && -n "$RUN_LABEL" && -n "$CLUSTER" && -n "$STATE_FILE" && -n "$EVIDENCE_DIR" ]] || {
  printf 'KUBE_CONTEXT, EXPECTED_API_SERVER, E2E_RUN_ID, GRID_RUN_LABEL, KIND_CLUSTER_NAME, STATE_FILE, and EVIDENCE_DIR are required\n' >&2
  exit 2
}
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,80}$ && "$RUN_LABEL" == "$RUN_ID" ]] || { printf 'run ID and ownership label must match a safe run identifier\n' >&2; exit 2; }
[[ "$TIMEOUT" =~ ^[1-9][0-9]*[smh]$ ]] || { printf 'KIND_MAAS_TENANT_CA_TIMEOUT must be an integer followed by s, m, or h\n' >&2; exit 2; }
case "${TIMEOUT: -1}" in s) TIMEOUT_SECONDS="${TIMEOUT%?}" ;; m) TIMEOUT_SECONDS="$(( ${TIMEOUT%?} * 60 ))" ;; h) TIMEOUT_SECONDS="$(( ${TIMEOUT%?} * 3600 ))" ;; esac
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR" "$(dirname "$STATE_FILE")"
chmod 0700 "$EVIDENCE_DIR" "$(dirname "$STATE_FILE")"

k() { command kubectl --context="$CONTEXT" --request-timeout=20s "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
capture_failure() {
  k get clusterissuer "$MAAS_ISSUER" "$SELF_SIGNED_ISSUER" -o yaml >"$EVIDENCE_DIR/clusterissuers-on-failure.yaml" 2>&1 || true
  k get certificate "$ROOT_CERT" -n "$NAMESPACE" -o yaml >"$EVIDENCE_DIR/root-certificate-on-failure.yaml" 2>&1 || true
  k get certificates.cert-manager.io -A -o json | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,issuerRef:.spec.issuerRef,conditions:.status.conditions}]' >"$EVIDENCE_DIR/certificates-on-failure.json" 2>&1 || true
  k get events -A --sort-by=.lastTimestamp -o json | jq '[.items[-100:][]|{namespace:.metadata.namespace,reason:.reason,message:.message,involvedObject:.involvedObject,lastTimestamp:.lastTimestamp}]' >"$EVIDENCE_DIR/recent-events-on-failure.json" 2>&1 || true
  k get deployment -n cert-manager -o json | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,images:[.spec.template.spec.containers[].image]}]' >"$EVIDENCE_DIR/cert-manager-deployments-on-failure.json" 2>&1 || true
}
on_error() {
  local code="$1"
  trap - ERR
  capture_failure
  jq -n --arg mode "$MODE" --arg run "$RUN_ID" --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:"blocked",run:$run,exitCode:$exitCode,evidenceDirectory:$evidence,privateKeyBytesRecorded:false}' >"$EVIDENCE_DIR/RESULT.json"
  log "MaaS Kind tenant CA mode $MODE stopped with exit=$code"
  exit "$code"
}
trap 'on_error $?' ERR

authorino_trust_snapshot() {
  local out="$1" cm_json="$EVIDENCE_DIR/authorino-ca-configmap-live.json" cr_json="$EVIDENCE_DIR/authorino-resource-live.json" deploy_json="$EVIDENCE_DIR/authorino-deployment-live.json"
  k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o json >"$cm_json"
  jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.kind/run"]==$run and (.data["ca.crt"]|type=="string" and length>0)' "$cm_json" >/dev/null || {
    printf 'Authorino MaaS CA ConfigMap is not owned by this Kind run or has no CA bundle\n' >&2; return 1;
  }
  k get authorino authorino -n "$AUTHORINO_NAMESPACE" -o json >"$cr_json"
  jq -e --arg cm "$AUTHORINO_CA_CONFIGMAP" --arg path "$AUTHORINO_CA_PATH" '
    any(.spec.volumes.items[]?; .mountPath=="/etc/ssl/certs" and any(.configMaps[]?;.==$cm) and
      any(.items[]?;.key=="ca.crt" and .path==($path|split("/")|last)))
  ' "$cr_json" >/dev/null || { printf 'Authorino CR does not mount the run-owned MaaS CA bundle at the verified system trust path\n' >&2; return 1; }
  k get deployment "$AUTHORINO_DEPLOYMENT" -n "$AUTHORINO_NAMESPACE" -o json >"$deploy_json"
  jq -e --arg path "$AUTHORINO_CA_PATH" '
    any(.spec.template.spec.containers[]?;.env|any(.[]?;.name=="SSL_CERT_FILE" and .value==$path)) and
    any(.spec.template.spec.containers[]?;.volumeMounts|any(.[]?;.mountPath=="/etc/ssl/certs" and .name=="maas-api-serving-ca"))
  ' "$deploy_json" >/dev/null || { printf 'Authorino Deployment does not use the verified CA bundle for TLS callbacks\n' >&2; return 1; }
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,resourceVersion:.metadata.resourceVersion,labels:.metadata.labels},data:{"ca.crt":.data["ca.crt"]}}' \
    "$cm_json" >"$out"
  chmod 0600 "$out" "$cm_json" "$cr_json" "$deploy_json"
}

install_authorino_trust() {
  local existing_json="$EVIDENCE_DIR/authorino-ca-before.json" bundle_before bundle_after root_pem root_fingerprint cm_uid labels manifest deadline
  local bundle_path
  bundle_path="$(dirname "$STATE_FILE")/authorino-maas-api-ca.original.pem"
  authorino_trust_snapshot "$existing_json"
  cm_uid="$(jq -r '.metadata.uid' "$existing_json")"
  bundle_before="$(jq -r '.data["ca.crt"]' "$existing_json")"
  root_pem="$(k get secret "$ROOT_SECRET" -n "$NAMESPACE" -o jsonpath='{.data.tls\.crt}' | base64 -d)"
  [[ -n "$root_pem" ]] || { printf 'run CA Secret has no public certificate\n' >&2; return 1; }
  root_fingerprint="$(printf '%s\n' "$root_pem" | openssl x509 -noout -fingerprint -sha256 | sed 's/^.*=//')"
  mkdir -p "$(dirname "$STATE_FILE")"
  if [[ -f "$STATE_FILE" ]] && jq -e '.authorinoTrust.originalBundlePath' "$STATE_FILE" >/dev/null 2>&1; then
    bundle_path="$(jq -r '.authorinoTrust.originalBundlePath' "$STATE_FILE")"
    [[ -s "$bundle_path" ]] || { printf 'recorded original Authorino CA bundle is missing\n' >&2; return 1; }
  elif [[ ! -s "$bundle_path" ]]; then
    # Snapshot the exact bundle even when the API CA helper has already placed
    # this same run-owned root in it. The resulting trust operation is then an
    # idempotent no-op, and cleanup can restore this exact baseline.
    printf '%s\n' "$bundle_before" >"$bundle_path"
    chmod 0600 "$bundle_path"
  fi
  if [[ "$bundle_before" != *"$root_pem"* ]]; then
    bundle_after="$(mktemp "$(dirname "$STATE_FILE")/.authorino-ca-bundle.XXXXXX")"
    cat "$bundle_path" >"$bundle_after"
    printf '\n%s\n' "$root_pem" >>"$bundle_after"
  else
    bundle_after="$(mktemp "$(dirname "$STATE_FILE")/.authorino-ca-bundle.XXXXXX")"
    printf '%s\n' "$bundle_before" >"$bundle_after"
  fi
  chmod 0600 "$bundle_after"
  jq -r '.metadata.labels' "$existing_json" >"$EVIDENCE_DIR/authorino-ca-labels.json"
  labels="$(cat "$EVIDENCE_DIR/authorino-ca-labels.json")"
  manifest="$EVIDENCE_DIR/authorino-ca-configmap-apply.json"
  k create configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" --from-file="ca.crt=$bundle_after" --dry-run=client -o json \
    | jq --argjson labels "$labels" '.metadata.labels=$labels' >"$manifest"
  k apply -f "$manifest" >"$EVIDENCE_DIR/authorino-ca-configmap-apply.txt"
  k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-ca-after.json"
  jq -e --arg uid "$cm_uid" --arg run "$RUN_LABEL" --arg root "$root_pem" '
    .metadata.uid==$uid and .metadata.labels["praxis.kind/run"]==$run and (.data["ca.crt"]|contains($root))
  ' "$EVIDENCE_DIR/authorino-ca-after.json" >/dev/null || { printf 'updated Authorino CA bundle lost ownership, UID, or run root certificate\n' >&2; return 1; }
  k rollout restart deployment/"$AUTHORINO_DEPLOYMENT" -n "$AUTHORINO_NAMESPACE" >"$EVIDENCE_DIR/authorino-rollout-restart.txt"
  k rollout status deployment/"$AUTHORINO_DEPLOYMENT" -n "$AUTHORINO_NAMESPACE" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/authorino-rollout-status.txt"
  authorino_trust_snapshot "$EVIDENCE_DIR/authorino-ca-configmap-after-rollout.json"
  jq -n --arg name "$AUTHORINO_CA_CONFIGMAP" --arg uid "$cm_uid" --arg path "$bundle_path" \
    --arg originalSha "$(sha256sum "$bundle_path" | awk '{print $1}')" \
    --arg combinedSha "$(sha256sum "$bundle_after" | awk '{print $1}')" --arg rootFingerprint "$root_fingerprint" \
    '{configMap:{name:$name,uid:$uid},originalBundlePath:$path,originalBundleSHA256:$originalSha,combinedBundleSHA256:$combinedSha,tenantRootFingerprintSHA256:$rootFingerprint,tlsVerificationEnabled:true}' \
    >"$EVIDENCE_DIR/authorino-trust-state.json"
  chmod 0600 "$EVIDENCE_DIR/authorino-trust-state.json"
  rm -f "$bundle_after"
  if [[ "$bundle_before" == *"$root_pem"* ]]; then
    log "Verified the run tenant CA was already present in Authorino's MaaS API trust bundle and preserved it; TLS verification remains enabled"
  else
    log "Added the run tenant CA to Authorino's existing MaaS API trust bundle and verified its rollout; TLS verification remains enabled"
  fi
}

restore_authorino_trust() {
  local state="$1" cm_uid bundle_path labels manifest restored_sha
  jq -e '.authorinoTrust.configMap.name=="authorino-maas-api-ca" and (.authorinoTrust.originalBundlePath|type=="string")' <<<"$state" >/dev/null || {
    printf 'run state does not contain the Authorino CA trust snapshot\n' >&2; return 1;
  }
  cm_uid="$(jq -r '.authorinoTrust.configMap.uid' <<<"$state")"
  bundle_path="$(jq -r '.authorinoTrust.originalBundlePath' <<<"$state")"
  [[ -s "$bundle_path" ]] || { printf 'original Authorino CA bundle is missing; refusing restore\n' >&2; return 1; }
  authorino_trust_snapshot "$EVIDENCE_DIR/authorino-ca-before-restore.json"
  jq -e --arg uid "$cm_uid" '.metadata.uid==$uid' "$EVIDENCE_DIR/authorino-ca-before-restore.json" >/dev/null || {
    printf 'Authorino CA ConfigMap UID changed; refusing restore\n' >&2; return 1;
  }
  jq -j '.data["ca.crt"]' "$EVIDENCE_DIR/authorino-ca-before-restore.json" >"$EVIDENCE_DIR/authorino-ca-current.pem"
  if [[ "$(sha256sum "$EVIDENCE_DIR/authorino-ca-current.pem" | awk '{print $1}')" != "$(jq -r '.authorinoTrust.combinedBundleSHA256' <<<"$state")" ]]; then
    printf 'Authorino CA bundle differs from the recorded run bundle; refusing to overwrite intervening changes\n' >&2; return 1;
  fi
  labels="$(jq -c '.metadata.labels' "$EVIDENCE_DIR/authorino-ca-before-restore.json")"
  manifest="$EVIDENCE_DIR/authorino-ca-configmap-restore.json"
  k create configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" --from-file="ca.crt=$bundle_path" --dry-run=client -o json \
    | jq --argjson labels "$labels" '.metadata.labels=$labels' >"$manifest"
  k apply -f "$manifest" >"$EVIDENCE_DIR/authorino-ca-configmap-restore.txt"
  k get configmap "$AUTHORINO_CA_CONFIGMAP" -n "$AUTHORINO_NAMESPACE" -o json >"$EVIDENCE_DIR/authorino-ca-restored.json"
  jq -j '.data["ca.crt"]' "$EVIDENCE_DIR/authorino-ca-restored.json" >"$EVIDENCE_DIR/authorino-ca-restored.pem"
  restored_sha="$(sha256sum "$EVIDENCE_DIR/authorino-ca-restored.pem" | awk '{print $1}')"
  jq -e --arg uid "$cm_uid" '.metadata.uid==$uid' "$EVIDENCE_DIR/authorino-ca-restored.json" >/dev/null && \
    [[ "$restored_sha" == "$(sha256sum "$bundle_path" | awk '{print $1}')" ]] || {
    printf 'Authorino CA trust bundle did not restore to its original UID and contents\n' >&2; return 1;
  }
  k rollout restart deployment/"$AUTHORINO_DEPLOYMENT" -n "$AUTHORINO_NAMESPACE" >"$EVIDENCE_DIR/authorino-rollout-restore.txt"
  k rollout status deployment/"$AUTHORINO_DEPLOYMENT" -n "$AUTHORINO_NAMESPACE" --timeout="$TIMEOUT" >"$EVIDENCE_DIR/authorino-rollout-restore-status.txt"
  log 'Restored the original Authorino MaaS API trust bundle and verified rollout before removing the run tenant CA'
}

server="$(k config view --minify -o jsonpath='{.clusters[0].cluster.server}')"
[[ "$server" == "$EXPECTED_SERVER" ]] || { printf 'Kubernetes API identity mismatch\n' >&2; exit 1; }
[[ "$(k get --raw=/readyz)" == *ok* ]] || { printf 'Kubernetes API is not ready\n' >&2; exit 1; }
kind get clusters | grep -Fxq "$CLUSTER" || { printf 'the run-owned Kind cluster is absent\n' >&2; exit 1; }
k get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/cert-manager-namespace.json"
k get crd certificates.cert-manager.io clusterissuers.cert-manager.io -o name >"$EVIDENCE_DIR/cert-manager-crds.txt"
k get deployment cert-manager cert-manager-webhook cert-manager-cainjector -n "$NAMESPACE" -o json \
  | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,observedGeneration:.status.observedGeneration,readyReplicas:.status.readyReplicas,images:[.spec.template.spec.containers[].image]}]' \
  >"$EVIDENCE_DIR/cert-manager-deployments.json"
jq -e 'length==3 and all(.[];.observedGeneration==.generation and (.readyReplicas//0)>=1)' "$EVIDENCE_DIR/cert-manager-deployments.json" >/dev/null || {
  printf 'cert-manager controllers are not all current and Ready\n' >&2; exit 1;
}

if [[ "$MODE" == cleanup ]]; then
  [[ -s "$STATE_FILE" ]] || { printf 'run state file is required for ownership-safe cleanup\n' >&2; exit 2; }
  k get aitenant -A -o json | jq -e --arg run "$RUN_LABEL" '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run)]|length==0' >/dev/null || {
    printf 'run-owned AITenants remain; wait for their natural deletion before removing the CA issuer\n' >&2; exit 1;
  }
  k get certificates.cert-manager.io -A -o json | jq -e --arg run "$RUN_LABEL" --arg root "$ROOT_CERT" \
    '[.items[]|select(.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.name!=$root)]|length==0' >/dev/null || {
    printf 'run-owned leaf Certificates remain; wait for natural MaaS cleanup before removing the CA issuer\n' >&2; exit 1;
  }
  state="$(cat "$STATE_FILE")"
  jq -e --arg run "$RUN_LABEL" '.runId==$run and .resources.maasIssuer.name=="rhai-ca-issuer"' <<<"$state" >/dev/null || {
    printf 'run state does not authorize this CA issuer cleanup\n' >&2; exit 1;
  }
  restore_authorino_trust "$state"
  for resource in "clusterissuer/$MAAS_ISSUER" "certificate/$ROOT_CERT" "clusterissuer/$SELF_SIGNED_ISSUER"; do
    resource_type="${resource%%/*}"
    resource_name="${resource#*/}"
    live="$EVIDENCE_DIR/$(tr '/-' '__' <<<"$resource").json"
    if [[ "$resource_type" == certificate ]]; then
      k get certificate "$resource_name" -n "$NAMESPACE" -o json >"$live" 2>/dev/null || continue
      expected_uid="$(jq -r '.resources.rootCertificate.uid' <<<"$state")"
    else
      k get clusterissuer "$resource_name" -o json >"$live" 2>/dev/null || continue
      if [[ "$resource_name" == "$MAAS_ISSUER" ]]; then
        expected_uid="$(jq -r '.resources.maasIssuer.uid' <<<"$state")"
      else
        expected_uid="$(jq -r '.resources.selfSignedIssuer.uid' <<<"$state")"
      fi
    fi
    if [[ -s "$live" ]]; then
      jq -e --arg run "$RUN_LABEL" --arg uid "$expected_uid" \
        '.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.uid==$uid' "$live" >/dev/null || {
        printf 'refusing to delete non-run-owned %s\n' "$resource" >&2; exit 1;
      }
      if [[ "$resource_type" == certificate ]]; then
        k delete certificate "$resource_name" -n "$NAMESPACE" --wait=true --timeout="$TIMEOUT" >"$EVIDENCE_DIR/$(tr '/-' '__' <<<"$resource").delete.txt"
      else
        k delete clusterissuer "$resource_name" --wait=true --timeout="$TIMEOUT" >"$EVIDENCE_DIR/$(tr '/-' '__' <<<"$resource").delete.txt"
      fi
    fi
  done
  secret_uid="$(jq -r '.resources.caSecret.uid' <<<"$state")"
  if k get secret "$ROOT_SECRET" -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},keys:((.data//{})|keys)}' \
      >"$EVIDENCE_DIR/ca-secret-before-delete.json"; then
    jq -e --arg run "$RUN_LABEL" --arg uid "$secret_uid" '.metadata.labels["praxis.fast/qualification-run"]==$run and .metadata.uid==$uid' \
      "$EVIDENCE_DIR/ca-secret-before-delete.json" >/dev/null || { printf 'CA Secret UID/ownership changed; refusing cleanup\n' >&2; exit 1; }
    k delete secret "$ROOT_SECRET" -n "$NAMESPACE" --wait=true --timeout="$TIMEOUT" >"$EVIDENCE_DIR/ca-secret.delete.txt"
  fi
  jq -n --arg run "$RUN_ID" --arg evidence "$EVIDENCE_DIR" '{mode:"cleanup",status:"cleaned",run:$run,evidenceDirectory:$evidence,privateKeyBytesRecorded:false}' >"$EVIDENCE_DIR/RESULT.json"
  log 'Run-owned MaaS Kind CA Certificate, issuer, and Secret were removed after tenant resources had disappeared'
  exit 0
fi

if [[ "$MODE" == install || "$MODE" == prepare-issuer ]]; then
  [[ ! -e "$STATE_FILE" ]] || { printf 'state file already exists; refusing a second install\n' >&2; exit 2; }
  cat >"$EVIDENCE_DIR/issuers.yaml" <<YAML
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: $SELF_SIGNED_ISSUER
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-kind
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  selfSigned: {}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: $ROOT_CERT
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-kind
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  isCA: true
  commonName: grid-llmd-$suffix-maas-kind-root
  secretName: $ROOT_SECRET
  duration: 72h
  renewBefore: 24h
  privateKey:
    algorithm: ECDSA
    size: 256
    rotationPolicy: Always
  secretTemplate:
    labels:
      app.kubernetes.io/part-of: grid-llmd-e2e
      app.kubernetes.io/managed-by: grid-llmd-kind
      praxis.fast/qualification-run: "$RUN_LABEL"
  issuerRef:
    name: $SELF_SIGNED_ISSUER
    kind: ClusterIssuer
    group: cert-manager.io
YAML
  cat >"$EVIDENCE_DIR/maas-cluster-issuer.yaml" <<YAML
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata:
  name: $MAAS_ISSUER
  labels:
    app.kubernetes.io/part-of: grid-llmd-e2e
    app.kubernetes.io/managed-by: grid-llmd-kind
    praxis.fast/qualification-run: "$RUN_LABEL"
spec:
  ca:
    secretName: $ROOT_SECRET
YAML
  for item in "clusterissuer/$SELF_SIGNED_ISSUER" "certificate/$ROOT_CERT" "clusterissuer/$MAAS_ISSUER"; do
    resource="${item%%/*}"
    name="${item#*/}"
    if [[ "$resource" == certificate ]]; then
      existing_status=0
      k get certificate "$name" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/existing-$resource-$name.json" 2>/dev/null || existing_status=$?
    else
      existing_status=0
      k get clusterissuer "$name" -o json >"$EVIDENCE_DIR/existing-$resource-$name.json" 2>/dev/null || existing_status=$?
    fi
    if (( existing_status == 0 )); then
      jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' "$EVIDENCE_DIR/existing-$resource-$name.json" >/dev/null || {
        printf 'resource %s exists without this run ownership label\n' "$item" >&2; exit 1;
      }
    fi
  done
  k apply --dry-run=server -f "$EVIDENCE_DIR/issuers.yaml" >"$EVIDENCE_DIR/certificates-server-dry-run.txt"
  k apply --dry-run=server -f "$EVIDENCE_DIR/maas-cluster-issuer.yaml" >"$EVIDENCE_DIR/maas-issuer-server-dry-run.txt"
  k apply -f "$EVIDENCE_DIR/issuers.yaml" >"$EVIDENCE_DIR/certificates-apply.txt"
  deadline=$((SECONDS + TIMEOUT_SECONDS))
  until k get clusterissuer "$SELF_SIGNED_ISSUER" -o json >"$EVIDENCE_DIR/self-signed-issuer-live.json" 2>/dev/null && \
    jq -e 'any(.status.conditions[]?;.type=="Ready" and .status=="True")' "$EVIDENCE_DIR/self-signed-issuer-live.json" >/dev/null; do
    (( SECONDS < deadline )) || { printf 'self-signed ClusterIssuer did not become Ready within %s\n' "$TIMEOUT" >&2; exit 1; }
    sleep 2
  done
  until k get certificate "$ROOT_CERT" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/root-certificate-live.json" 2>/dev/null && \
    jq -e 'any(.status.conditions[]?;.type=="Ready" and .status=="True")' "$EVIDENCE_DIR/root-certificate-live.json" >/dev/null && \
    k get secret "$ROOT_SECRET" -n "$NAMESPACE" -o json 2>/dev/null \
      | jq -e '((.data//{})|has("tls.crt") and has("tls.key"))' >/dev/null; do
    (( SECONDS < deadline )) || { printf 'run CA Certificate did not become Ready within %s\n' "$TIMEOUT" >&2; exit 1; }
    sleep 2
  done
  k apply -f "$EVIDENCE_DIR/maas-cluster-issuer.yaml" >"$EVIDENCE_DIR/maas-issuer-apply.txt"
fi

deadline=$((SECONDS + TIMEOUT_SECONDS))
until k get clusterissuer "$MAAS_ISSUER" -o json >"$EVIDENCE_DIR/maas-issuer-live.json" 2>/dev/null && \
  jq -e --arg run "$RUN_LABEL" '(.metadata.labels["praxis.fast/qualification-run"]==$run) and any(.status.conditions[]?;.type=="Ready" and .status=="True")' \
    "$EVIDENCE_DIR/maas-issuer-live.json" >/dev/null; do
  (( SECONDS < deadline )) || { printf 'MaaS ClusterIssuer did not become Ready within %s\n' "$TIMEOUT" >&2; exit 1; }
  sleep 2
done
k get certificate "$ROOT_CERT" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/root-certificate-live.json"
jq -n \
  --argjson secret "$(k get secret "$ROOT_SECRET" -n "$NAMESPACE" -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},keys:((.data//{})|keys),hasTLSCertificate:((.data//{})|has("tls.crt")),hasTLSPrivateKey:((.data//{})|has("tls.key"))} ')" \
  '$secret' >"$EVIDENCE_DIR/ca-secret-identity.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and any(.status.conditions[]?;.type=="Ready" and .status=="True")' \
  "$EVIDENCE_DIR/root-certificate-live.json" >/dev/null || { printf 'run CA Certificate is not Ready and owned by this run\n' >&2; exit 1; }
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and .hasTLSCertificate and .hasTLSPrivateKey' \
  "$EVIDENCE_DIR/ca-secret-identity.json" >/dev/null || { printf 'run CA Secret is not complete and owned by this run\n' >&2; exit 1; }
root_fingerprint="$(k get secret "$ROOT_SECRET" -n "$NAMESPACE" -o jsonpath='{.data.tls\.crt}' | base64 -d | openssl x509 -noout -fingerprint -sha256 | sed 's/^.*=//')"

k get clusterissuer "$MAAS_ISSUER" -o json | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels},spec:.spec,status:.status}' >"$EVIDENCE_DIR/maas-issuer-summary.json"
k get certificate "$ROOT_CERT" -n "$NAMESPACE" -o json | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{isCA:.spec.isCA,secretName:.spec.secretName,issuerRef:.spec.issuerRef,duration:.spec.duration},status:{conditions:.status.conditions}}' >"$EVIDENCE_DIR/root-certificate-summary.json"
if [[ "$MODE" == install || "$MODE" == trust ]]; then
  if [[ "$MODE" == trust ]]; then
    [[ -s "$STATE_FILE" ]] || { printf 'trust mode requires the existing run CA state file\n' >&2; exit 2; }
    jq -e --arg run "$RUN_LABEL" '.runId==$run and .resources.maasIssuer.name=="rhai-ca-issuer"' "$STATE_FILE" >/dev/null || {
      printf 'trust mode state does not identify this run tenant issuer\n' >&2; exit 1;
    }
  fi
  install_authorino_trust
fi
if [[ "$MODE" == verify ]]; then
  [[ -s "$STATE_FILE" ]] || { printf 'verify mode requires the run state file\n' >&2; exit 2; }
  state="$(cat "$STATE_FILE")"
  expected_bundle_sha="$(jq -er '.authorinoTrust.combinedBundleSHA256' <<<"$state")" || {
    printf 'run state does not contain the Authorino CA bundle digest\n' >&2; exit 1;
  }
  authorino_trust_snapshot "$EVIDENCE_DIR/authorino-ca-verify.json"
  jq -e --arg run "$RUN_LABEL" --arg uid "$(jq -r '.authorinoTrust.configMap.uid' <<<"$state")" \
    --arg root "$(k get secret "$ROOT_SECRET" -n "$NAMESPACE" -o jsonpath='{.data.tls\.crt}' | base64 -d)" \
    '
      .metadata.labels["praxis.kind/run"]==$run and .metadata.uid==$uid and (.data["ca.crt"]|contains($root))
    ' "$EVIDENCE_DIR/authorino-ca-verify.json" >/dev/null || { printf 'Authorino does not trust the run tenant CA under the captured run-owned identity\n' >&2; exit 1; }
  jq -j '.data["ca.crt"]' "$EVIDENCE_DIR/authorino-ca-verify.json" >"$EVIDENCE_DIR/authorino-ca-verify.pem"
  observed_bundle_sha="$(sha256sum "$EVIDENCE_DIR/authorino-ca-verify.pem" | awk '{print $1}')"
  [[ "$observed_bundle_sha" == "$expected_bundle_sha" ]] || {
    printf 'Authorino CA bundle digest differs from the run state\n' >&2; exit 1;
  }
fi
if [[ "$MODE" == install || "$MODE" == prepare-issuer ]]; then
  authorino_trust='null'
  if [[ -s "$EVIDENCE_DIR/authorino-trust-state.json" ]]; then
    authorino_trust="$(cat "$EVIDENCE_DIR/authorino-trust-state.json")"
  fi
  jq -n --arg run "$RUN_ID" --arg cluster "$CLUSTER" --arg issuer "$MAAS_ISSUER" --arg rootCert "$ROOT_CERT" --arg rootSecret "$ROOT_SECRET" \
    --arg selfIssuer "$SELF_SIGNED_ISSUER" --arg fingerprint "$root_fingerprint" \
    --argjson authorinoTrust "$authorino_trust" \
    --arg issuerUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/maas-issuer-live.json")" \
    --arg selfIssuerUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/self-signed-issuer-live.json")" \
    --arg certUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/root-certificate-live.json")" \
    --arg secretUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/ca-secret-identity.json")" \
    '{schema:"praxis.kind.maas-tenant-ca/v1",runId:$run,cluster:$cluster,resources:{maasIssuer:{name:$issuer,uid:$issuerUid},selfSignedIssuer:{name:$selfIssuer,uid:$selfIssuerUid},rootCertificate:{name:$rootCert,uid:$certUid},caSecret:{name:$rootSecret,uid:$secretUid}},publicCertificateFingerprintSHA256:$fingerprint,authorinoTrust:$authorinoTrust,privateKeyBytesRecorded:false}' \
    >"$STATE_FILE.tmp"
  chmod 0600 "$STATE_FILE.tmp"
  mv "$STATE_FILE.tmp" "$STATE_FILE"
fi
if [[ "$MODE" == trust ]]; then
  jq --slurpfile trust "$EVIDENCE_DIR/authorino-trust-state.json" '.authorinoTrust=$trust[0]' "$STATE_FILE" >"$STATE_FILE.tmp"
  chmod 0600 "$STATE_FILE.tmp"
  mv "$STATE_FILE.tmp" "$STATE_FILE"
fi
jq -n --arg mode "$MODE" --arg run "$RUN_ID" --arg issuer "$MAAS_ISSUER" --arg rootCert "$ROOT_CERT" \
  --arg fingerprint "$root_fingerprint" --arg evidence "$EVIDENCE_DIR" \
  '{mode:$mode,status:(if $mode=="prepare-issuer" then "issuer-ready" else "ready" end),run:$run,maasClusterIssuer:$issuer,rootCertificate:$rootCert,publicCertificateFingerprintSHA256:$fingerprint,authorinoTrustInstalled:($mode!="prepare-issuer"),privateKeyBytesRecorded:false,evidenceDirectory:$evidence}' \
  >"$EVIDENCE_DIR/RESULT.json"
if [[ "$MODE" == "prepare-issuer" ]]; then
  log "MaaS CA issuer $MAAS_ISSUER is Ready; Authorino trust is intentionally deferred until the MaaS API CA bundle exists"
else
  log "MaaS AITenant CA issuer $MAAS_ISSUER is Ready from the run-owned cert-manager CA; Authorino TLS trust is verified and key bytes were not recorded"
fi
