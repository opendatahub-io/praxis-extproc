#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Wait for and verify the self-signed identity created by the Grid operator.
set -Eeuo pipefail
umask 077

MODE="${GRID_IDENTITY_MODE:-apply}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
NAMESPACE="${GRID_OPERATOR_NAMESPACE:-}"
RELEASE_NAME="${GRID_OPERATOR_RELEASE_NAME:-grid-operator}"
DEPLOYMENT_NAME="${GRID_OPERATOR_DEPLOYMENT_NAME:-grid-operator}"
NETWORK_NAME="${GRID_NETWORK_NAME:-}"
SITE_NAME="${GRID_SITE_NAME:-}"
SERVER_NAME="${GRID_GATEWAY_HOSTNAME:-}"
TIMEOUT="${GRID_IDENTITY_TIMEOUT:-300}"
EVIDENCE_DIR="${GRID_IDENTITY_EVIDENCE_DIR:-}"
CURRENT_PHASE="input-validation"
RESULT_WRITTEN=0

for bin in oc jq openssl base64 cmp python3; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
[[ "$MODE" == apply || "$MODE" == verify ]] || { printf 'GRID_IDENTITY_MODE must be apply or verify\n' >&2; exit 2; }
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" && -n "$NAMESPACE" && -n "$NETWORK_NAME" && -n "$SITE_NAME" && -n "$SERVER_NAME" ]] || {
  printf 'expected API, context, run label, operator namespace, GridNetwork, site name, and Gateway hostname are required\n' >&2
  exit 2
}
[[ "$SITE_NAME" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ && ${#SITE_NAME} -le 63 ]] || { printf 'GRID_SITE_NAME must be a lowercase DNS label up to 63 characters\n' >&2; exit 2; }
[[ "$SERVER_NAME" == "$SITE_NAME.grid.internal" ]] || {
  printf 'the Grid operator self-signed identity uses the site DNS name %s.grid.internal\n' "$SITE_NAME" >&2
  exit 2
}
[[ "$TIMEOUT" =~ ^[1-9][0-9]{0,3}$ ]] || { printf 'GRID_IDENTITY_TIMEOUT must be 1..9999 seconds\n' >&2; exit 2; }
[[ -n "$EVIDENCE_DIR" ]] || { printf 'GRID_IDENTITY_EVIDENCE_DIR is required\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
tmpdir="$(mktemp -d)"
chmod 0700 "$tmpdir"

oc() { command oc --kubeconfig="${KUBECONFIG:?KUBECONFIG must be set}" --context="$KUBE_CONTEXT" "$@"; }
log() { printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$*" | tee -a "$EVIDENCE_DIR/RUN-LOG.md"; }
die() {
  printf '%s\n' "$*" >"$EVIDENCE_DIR/failure-reason.txt"
  printf '%s\n' "$*" >&2
  exit 1
}
on_exit() {
  local rc=$?
  if (( rc != 0 )) && [[ "$RESULT_WRITTEN" != 1 ]]; then
    jq -n --arg phase "$CURRENT_PHASE" --arg reason "see failure-reason.txt and diagnostics" --argjson exitCode "$rc" \
      '{status:"fail",phase:$phase,exitCode:$exitCode,reason:$reason}' >"$EVIDENCE_DIR/RESULT.json" 2>/dev/null || true
    if command -v oc >/dev/null && [[ -n "${KUBECONFIG:-}" ]]; then
      oc get gridnetwork "$NETWORK_NAME" -o json 2>/dev/null \
        | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,annotations:.metadata.annotations},spec:{tls:.spec.tls},status:.status}' \
        >"$EVIDENCE_DIR/gridnetwork-failure-diagnostics.json" 2>/dev/null || true
      oc get gridsite "$SITE_NAME" -o json 2>/dev/null \
        | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,annotations:.metadata.annotations},spec:.spec,status:.status}' \
        >"$EVIDENCE_DIR/gridsite-failure-diagnostics.json" 2>/dev/null || true
      for secret_name in grid-ca grid-site-identity; do
        oc -n "$NAMESPACE" get secret "$secret_name" -o json 2>/dev/null \
          | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},type:.type,keyNames:(.data|keys)}' \
          >"$EVIDENCE_DIR/${secret_name}-metadata.json" 2>/dev/null || true
      done
    fi
    chmod 0600 "$EVIDENCE_DIR"/* 2>/dev/null || true
  fi
  if [[ -n "${tmpdir:-}" && -d "$tmpdir" ]]; then
    python3 -c 'import pathlib,sys; p=pathlib.Path(sys.argv[1]); [x.unlink() for x in p.iterdir() if x.is_file()]; p.rmdir()' "$tmpdir" 2>/dev/null || true
  fi
}
trap on_exit EXIT
trap 'rc=$?; printf "command failed during phase %s (exit %s)\n" "$CURRENT_PHASE" "$rc" >"$EVIDENCE_DIR/failure-reason.txt"; exit "$rc"' ERR

CURRENT_PHASE="cluster-and-resource-ownership"
server="$(oc whoami --show-server)"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || die "API server mismatch"
ready="$(oc get --raw=/readyz)"
[[ "$ready" == *ok* ]] || die "API /readyz did not return ok"
oc get namespace "$NAMESPACE" -o json >"$tmpdir/namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"] == $run' "$tmpdir/namespace.json" >/dev/null \
  || die "operator namespace is not owned by this run; refusing to adopt it"
oc -n "$NAMESPACE" get deployment "$DEPLOYMENT_NAME" -o json >"$EVIDENCE_DIR/operator-deployment.json"
jq -e --arg run "$RUN_LABEL" --arg release "$RELEASE_NAME" '
  .metadata.labels["praxis.fast/qualification-run"]==$run and
  .metadata.annotations["meta.helm.sh/release-name"]==$release and
  .status.observedGeneration >= .metadata.generation and
  (.status.availableReplicas // 0) >= 1
' "$EVIDENCE_DIR/operator-deployment.json" >/dev/null \
  || die "the current run-owned Grid operator Helm release must be deployed and Ready before identity verification"
oc get gridnetwork "$NETWORK_NAME" -o json >"$tmpdir/gridnetwork.json"
oc get gridsite "$SITE_NAME" -o json >"$tmpdir/gridsite.json"
for object in "$tmpdir/gridnetwork.json" "$tmpdir/gridsite.json"; do
  jq -e --arg run "$RUN_LABEL" --arg release "$RELEASE_NAME" --arg ns "$NAMESPACE" '
    .metadata.labels["praxis.fast/qualification-run"]==$run and
    .metadata.annotations["meta.helm.sh/release-name"]==$release and
    .metadata.annotations["meta.helm.sh/release-namespace"]==$ns
  ' "$object" >/dev/null || die "GridNetwork and GridSite must be owned by the current run Helm release"
done
jq -e --arg network "$NETWORK_NAME" '.spec.gridNetworkRef==$network' "$tmpdir/gridsite.json" >/dev/null \
  || die "GridSite does not reference the task GridNetwork"
jq -e --arg ns "$NAMESPACE" '
  .spec.tls.caSecretRef.namespace==$ns and .spec.tls.caSecretRef.name=="grid-ca" and
  .spec.tls.siteSecretRef.namespace==$ns and .spec.tls.siteSecretRef.name=="grid-site-identity"
' "$tmpdir/gridnetwork.json" >/dev/null || die "GridNetwork TLS references do not match the expected task-local identity Secrets"
network_created="$(jq -r '.metadata.creationTimestamp' "$tmpdir/gridnetwork.json")"
network_created_epoch="$(date -u -d "$network_created" +%s)"

CURRENT_PHASE="operator-identity-convergence"
if [[ "$MODE" == apply ]]; then
  deadline=$((SECONDS + TIMEOUT))
  while ! oc -n "$NAMESPACE" get secret grid-ca -o json >"$tmpdir/ca-secret.json" 2>/dev/null \
      || ! oc -n "$NAMESPACE" get secret grid-site-identity -o json >"$tmpdir/site-secret.json" 2>/dev/null; do
    (( SECONDS < deadline )) || die "Grid operator did not create both identity Secrets within ${TIMEOUT}s"
    sleep 3
  done
else
  oc -n "$NAMESPACE" get secret grid-ca -o json >"$tmpdir/ca-secret.json" \
    || die "operator-created grid-ca Secret is missing"
  oc -n "$NAMESPACE" get secret grid-site-identity -o json >"$tmpdir/site-secret.json" \
    || die "operator-created grid-site-identity Secret is missing"
fi
jq -e '(.type=="Opaque") and ((.data|keys|sort)==["ca.crt","ca.key"])' "$tmpdir/ca-secret.json" >/dev/null \
  || die "Grid operator CA Secret must contain exactly ca.crt and ca.key"
jq -e '(.type=="Opaque") and ((.data|keys|sort)==["tls.crt","tls.key"])' "$tmpdir/site-secret.json" >/dev/null \
  || die "Grid operator site identity Secret must contain exactly tls.crt and tls.key"
ca_created="$(jq -r '.metadata.creationTimestamp' "$tmpdir/ca-secret.json")"
site_created="$(jq -r '.metadata.creationTimestamp' "$tmpdir/site-secret.json")"
ca_created_epoch="$(date -u -d "$ca_created" +%s)"
site_created_epoch="$(date -u -d "$site_created" +%s)"
(( ca_created_epoch >= network_created_epoch && site_created_epoch >= network_created_epoch )) \
  || die "identity Secret predates the task GridNetwork; refusing stale identity material"

# Read secret bytes only into the mode-0700 temporary directory. Evidence stores only public certificate metadata.
for pair in "ca-secret.json:ca.crt:ca.crt" "ca-secret.json:ca.key:ca.key" \
            "site-secret.json:tls.crt:site.crt" "site-secret.json:tls.key:site.key"; do
  IFS=: read -r source key target <<<"$pair"
  jq -r --arg key "$key" '.data[$key]' "$tmpdir/$source" | base64 -d >"$tmpdir/$target"
  chmod 0600 "$tmpdir/$target"
done

CURRENT_PHASE="identity-cryptographic-validation"
openssl x509 -in "$tmpdir/ca.crt" -noout >/dev/null || die "operator CA certificate is invalid"
openssl pkey -in "$tmpdir/ca.key" -noout >/dev/null 2>&1 || die "operator CA private key is invalid"
openssl x509 -in "$tmpdir/site.crt" -noout >/dev/null || die "operator site certificate is invalid"
openssl pkey -in "$tmpdir/site.key" -noout >/dev/null 2>&1 || die "operator site private key is invalid"
openssl x509 -in "$tmpdir/ca.crt" -pubkey -noout >"$tmpdir/ca-cert.pub"
openssl pkey -in "$tmpdir/ca.key" -pubout >"$tmpdir/ca-key.pub" 2>/dev/null
cmp -s "$tmpdir/ca-cert.pub" "$tmpdir/ca-key.pub" || die "operator CA certificate and private key do not match"
openssl x509 -in "$tmpdir/site.crt" -pubkey -noout >"$tmpdir/site-cert.pub"
openssl pkey -in "$tmpdir/site.key" -pubout >"$tmpdir/site-key.pub" 2>/dev/null
cmp -s "$tmpdir/site-cert.pub" "$tmpdir/site-key.pub" || die "operator site certificate and private key do not match"
openssl verify -CAfile "$tmpdir/ca.crt" "$tmpdir/ca.crt" >"$EVIDENCE_DIR/grid-ca-self-verification.txt"
openssl verify -verify_hostname "$SERVER_NAME" -CAfile "$tmpdir/ca.crt" "$tmpdir/site.crt" >"$EVIDENCE_DIR/certificate-chain.txt"
openssl x509 -in "$tmpdir/site.crt" -noout -subject -issuer -dates -fingerprint -sha256 -ext subjectAltName \
  >"$EVIDENCE_DIR/site-certificate-public-metadata.txt"
grep -Fq "DNS:$SERVER_NAME" "$EVIDENCE_DIR/site-certificate-public-metadata.txt" \
  || die "operator site certificate does not include the expected DNS SAN"
grep -Fq "URI:spiffe://grid.internal/site/$SITE_NAME" "$EVIDENCE_DIR/site-certificate-public-metadata.txt" \
  || die "operator site certificate does not include the expected SPIFFE URI"
grep -Fq "O = $SITE_NAME" "$EVIDENCE_DIR/site-certificate-public-metadata.txt" \
  || die "operator site certificate organization does not match the task site"

CURRENT_PHASE="grid-status-and-evidence"
oc get gridnetwork "$NETWORK_NAME" -o json \
  | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,creationTimestamp:.metadata.creationTimestamp},spec:{tls:.spec.tls},status:{observedGeneration:.status.observedGeneration,phase:.status.phase,conditions:.status.conditions}}' \
  >"$EVIDENCE_DIR/gridnetwork-identity-status.json"
oc get gridsite "$SITE_NAME" -o json \
  | jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,creationTimestamp:.metadata.creationTimestamp},spec:.spec,status:.status}' \
  >"$EVIDENCE_DIR/gridsite-identity-status.json"
jq -e --argjson generation "$(jq -r '.metadata.generation' "$tmpdir/gridnetwork.json")" '
  .status.observedGeneration >= $generation and
  any(.status.conditions[]?; .type=="IdentityValid" and .status=="True")
' "$EVIDENCE_DIR/gridnetwork-identity-status.json" >/dev/null \
  || die "GridNetwork has not accepted the current operator-generated identity"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},type:.type,keyNames:(.data|keys)}' \
  "$tmpdir/site-secret.json" >"$EVIDENCE_DIR/site-identity-secret-metadata.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,creationTimestamp:.metadata.creationTimestamp,labels:.metadata.labels,ownerReferences:.metadata.ownerReferences},type:.type,keyNames:(.data|keys)}' \
  "$tmpdir/ca-secret.json" >"$EVIDENCE_DIR/grid-ca-secret-metadata.json"
chmod 0600 "$EVIDENCE_DIR"/*
log "Verified the Grid operator-generated CA and site identity for site=$SITE_NAME hostname=$SERVER_NAME; no identity Secret was modified"
jq -n --arg apiServer "$EXPECTED_API_SERVER" --arg namespace "$NAMESPACE" --arg network "$NETWORK_NAME" \
  --arg site "$SITE_NAME" --arg serverName "$SERVER_NAME" --arg evidenceDirectory "$EVIDENCE_DIR" \
  --arg caUid "$(jq -r '.metadata.uid' "$tmpdir/ca-secret.json")" \
  --arg siteUid "$(jq -r '.metadata.uid' "$tmpdir/site-secret.json")" \
  --arg caCreated "$ca_created" --arg siteCreated "$site_created" \
  '{status:"pass",identityOwner:"Grid operator",apiServer:$apiServer,namespace:$namespace,network:$network,siteName:$site,serverName:$serverName,caSecretUid:$caUid,siteSecretUid:$siteUid,caCreated:$caCreated,siteCreated:$siteCreated,evidenceDirectory:$evidenceDirectory}' \
  >"$EVIDENCE_DIR/RESULT.json"
RESULT_WRITTEN=1
