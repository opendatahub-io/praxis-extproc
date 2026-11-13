#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Configure the task GridSite to probe the task-owned Gateway API address.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
MODE="${GRID_SITE_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
NETWORK_NAME="${GRID_NETWORK_NAME:-grid-llmd-e2e}"
SITE_NAME="${GRID_SITE_NAME:-grid-llmd-e2e}"
SITE_ADDRESS="${GRID_SITE_GATEWAY_ADDRESS:-}"
SERVER_NAME="${GRID_SITE_SERVER_NAME:-}"
OPERATOR_NAMESPACE="${GRID_OPERATOR_NAMESPACE:-praxis-grid-operator-system}"
IDENTITY_SECRET="${GRID_IDENTITY_SECRET_NAME:-grid-site-identity}"
CA_SECRET="${GRID_CA_SECRET_NAME:-grid-ca}"
SOURCE_REVISION="${GRID_OPERATOR_SOURCE_REVISION:-}"
EVIDENCE_DIR="${GRID_SITE_EVIDENCE_DIR:-}"

oc() {
  if [[ -z "$KUBE_CONTEXT" ]]; then KUBE_CONTEXT="$(command oc config current-context)"; fi
  command oc --context="$KUBE_CONTEXT" "$@"
}

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Task GridSite endpoint plan (plan mode makes no cluster calls):
  1. Verify the current API context, task namespace, GridNetwork, GridSite ownership, and exact TLS Secret references.
  2. Verify the current-run site certificate chains to the current-run Grid CA and matches the supplied server name.
  3. Compute the certificate's canonical DER SHA-256 fingerprint without writing the certificate/key to evidence.
  4. Patch only the task GridSite spec.gatewayEndpoint and spec.trust canonicalFingerprints fields.
  5. Wait for current-generation GridSite Active/TlsVerified and capture bounded status/events/logs.

The patch preserves GridNetwork reference, region, zone, and all other GridSite fields. No inference traffic is sent.
Modes: plan, patch, verify, collect. API TLS bypass requires the explicit ephemeral-dev opt-in.
PLAN
  exit 0
fi

for bin in oc jq openssl base64 sha256sum awk; do
  command -v "$bin" >/dev/null || { printf '%s is required\n' "$bin" >&2; exit 2; }
done
[[ "$MODE" == patch || "$MODE" == verify || "$MODE" == collect ]] || {
  printf 'unsupported GRID_SITE_MODE: %s\n' "$MODE" >&2
  exit 2
}
[[ -n "$EXPECTED_API_SERVER" && -n "$RUN_LABEL" && -n "$SOURCE_REVISION" && -n "$SITE_ADDRESS" && -n "$SERVER_NAME" ]] || {
  printf 'EXPECTED_API_SERVER, GRID_RUN_LABEL, GRID_OPERATOR_SOURCE_REVISION, GRID_SITE_GATEWAY_ADDRESS, and GRID_SITE_SERVER_NAME are required\n' >&2
  exit 2
}
[[ "$SITE_ADDRESS" =~ ^([^[:space:]:]+|\[[0-9a-fA-F:]+\]):[1-9][0-9]{0,4}$ ]] || {
  printf 'GRID_SITE_GATEWAY_ADDRESS must be host:port or [ipv6]:port\n' >&2
  exit 2
}
[[ "$SERVER_NAME" =~ ^[A-Za-z0-9]([A-Za-z0-9.-]*[A-Za-z0-9])?$ ]] || {
  printf 'GRID_SITE_SERVER_NAME must be a DNS hostname\n' >&2
  exit 2
}
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="$REPO_ROOT/evidence/grid-site-$(date -u +%Y%m%dT%H%M%SZ)"
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
    printf '# GridSite endpoint attempt bug capture\n\n'
    printf -- '- First failed phase: `%s`\n' "${CURRENT_PHASE:-initialization}"
    printf -- '- Failure detail: `%s`\n' "${CURRENT_FAILURE:-see bounded phase evidence}"
    printf -- '- Time: `%s`\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf -- '- API server: `%s`\n' "$EXPECTED_API_SERVER"
    printf -- '- Grid source revision: `%s`\n' "$SOURCE_REVISION"
    printf -- '- Grid resources: `%s/%s / %s`\n' "$NAMESPACE" "$NETWORK_NAME" "$SITE_NAME"
    printf -- '- Gateway address/server name: `%s` / `%s`\n' "$SITE_ADDRESS" "$SERVER_NAME"
    printf -- '- Evidence directory: `%s`\n' "$EVIDENCE_DIR"
    printf '\nNo Secret values, certificate bytes, private keys, or inference traffic were captured/sent.\n'
  } >"$EVIDENCE_DIR/BUG-CAPTURE.md"
  jq -n --arg mode "$MODE" --arg status blocked --arg phase "${CURRENT_PHASE:-initialization}" \
    --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$code" \
    '{mode:$mode,status:$status,phase:$phase,exitCode:$exitCode,evidenceDirectory:$evidence,inferenceTrafficSent:false}' \
    >"$EVIDENCE_DIR/RESULT.json" 2>/dev/null || true
  log "GridSite endpoint $MODE stopped with exit=$code at phase=${CURRENT_PHASE:-initialization}"
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
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
if [[ "$insecure" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
    CURRENT_FAILURE='insecure kubeconfig used without ephemeral-dev opt-in'
    printf 'insecure kubeconfig rejected without explicit dev opt-in\n' >&2
    exit 1
  }
  API_TLS_MODE=explicit-ephemeral-dev-unverified
else
  API_TLS_MODE=kubeconfig-verified
fi

CURRENT_PHASE=ownership-and-schema-preflight
oc get namespace "$NAMESPACE" -o json >"$EVIDENCE_DIR/namespace.json"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' \
  "$EVIDENCE_DIR/namespace.json" >/dev/null || {
  CURRENT_FAILURE='Grid resource namespace is not owned by this run'
  printf 'Grid resource namespace ownership mismatch\n' >&2
  exit 1
}
oc get gridnetwork "$NETWORK_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gridnetwork-before.json"
jq -e --arg run "$RUN_LABEL" --arg ns "$OPERATOR_NAMESPACE" --arg identity "$IDENTITY_SECRET" --arg ca "$CA_SECRET" \
  '.metadata.labels["praxis.fast/qualification-run"]==$run and
   .spec.tls.siteSecretRef.namespace==$ns and .spec.tls.siteSecretRef.name==$identity and
   .spec.tls.caSecretRef.namespace==$ns and .spec.tls.caSecretRef.name==$ca' \
  "$EVIDENCE_DIR/gridnetwork-before.json" >/dev/null || {
  CURRENT_FAILURE='GridNetwork task ownership or TLS Secret references do not match expected run values'
  printf 'GridNetwork ownership/TLS reference gate failed\n' >&2
  exit 1
}
oc get gridsite "$SITE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gridsite-before.json"
jq -e --arg run "$RUN_LABEL" --arg network "$NETWORK_NAME" \
  '.metadata.labels["praxis.fast/qualification-run"]==$run and .spec.gridNetworkRef==$network' \
  "$EVIDENCE_DIR/gridsite-before.json" >/dev/null || {
  CURRENT_FAILURE='GridSite is not task-owned or references a different GridNetwork'
  printf 'GridSite ownership/network gate failed\n' >&2
  exit 1
}
oc get crd gridsites.grid.praxis.fast -o json >"$EVIDENCE_DIR/gridsites-crd.json"
jq -e 'any(.spec.versions[]?;.served==true and .schema.openAPIV3Schema.properties.spec.properties.gatewayEndpoint.properties.tls.properties.serverName.type=="string" and .schema.openAPIV3Schema.properties.spec.properties.trust.properties.canonicalFingerprints.type=="array")' \
  "$EVIDENCE_DIR/gridsites-crd.json" >/dev/null || {
  CURRENT_FAILURE='installed GridSite CRD lacks gatewayEndpoint TLS serverName or canonicalFingerprints fields'
  printf 'installed GridSite CRD does not support the required current endpoint/trust fields\n' >&2
  exit 1
}

CURRENT_PHASE=certificate-verification
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT
identity_json="$(oc get secret "$IDENTITY_SECRET" -n "$OPERATOR_NAMESPACE" -o json)"
ca_json="$(oc get secret "$CA_SECRET" -n "$OPERATOR_NAMESPACE" -o json)"
jq -e '(.data|keys|sort)==["tls.crt","tls.key"]' <<<"$identity_json" >/dev/null || {
  CURRENT_FAILURE='task identity Secret does not contain the expected TLS key pair'
  printf 'site identity Secret key names do not match tls.crt/tls.key\n' >&2
  exit 1
}
jq -e '.data|has("ca.crt")' <<<"$ca_json" >/dev/null || {
  CURRENT_FAILURE='task Grid CA Secret has no ca.crt key'
  printf 'Grid CA Secret lacks ca.crt\n' >&2
  exit 1
}
printf '%s' "$(jq -r '.data["tls.crt"]' <<<"$identity_json")" | base64 -d >"$tmpdir/site.crt"
printf '%s' "$(jq -r '.data["ca.crt"]' <<<"$ca_json")" | base64 -d >"$tmpdir/grid-ca.crt"
openssl verify -CAfile "$tmpdir/grid-ca.crt" "$tmpdir/site.crt" >"$EVIDENCE_DIR/certificate-chain.txt"
openssl x509 -in "$tmpdir/site.crt" -noout -subject -issuer -dates -fingerprint -sha256 -ext subjectAltName \
  >"$EVIDENCE_DIR/certificate-metadata.txt"
grep -Fq "DNS:$SERVER_NAME" "$EVIDENCE_DIR/certificate-metadata.txt" || {
  CURRENT_FAILURE='site certificate SAN does not match GRID_SITE_SERVER_NAME'
  printf 'site identity SAN does not match the requested GridSite TLS serverName\n' >&2
  exit 1
}
fingerprint="$(openssl x509 -in "$tmpdir/site.crt" -outform DER | sha256sum | awk '{print $1}')"
cert_pub_sha="$(openssl x509 -in "$tmpdir/site.crt" -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum | awk '{print $1}')"
key_pub_sha="$(printf '%s' "$(jq -r '.data["tls.key"]' <<<"$identity_json")" | base64 -d | openssl pkey -pubout -outform DER | sha256sum | awk '{print $1}')"
[[ "$cert_pub_sha" == "$key_pub_sha" ]] || {
  CURRENT_FAILURE='site identity certificate/private-key pair mismatch'
  printf 'site identity certificate/key pair validation failed\n' >&2
  exit 1
}
[[ "$fingerprint" =~ ^[0-9a-f]{64}$ ]] || {
  CURRENT_FAILURE='computed canonical certificate fingerprint was malformed'
  printf 'canonical certificate fingerprint validation failed\n' >&2
  exit 1
}
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,annotations:.metadata.annotations},type,keyNames:(.data|keys)}' \
  <<<"$identity_json" >"$EVIDENCE_DIR/site-identity-metadata.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,labels:.metadata.labels,annotations:.metadata.annotations},type,keyNames:(.data|keys)}' \
  <<<"$ca_json" >"$EVIDENCE_DIR/grid-ca-metadata.json"
unset identity_json ca_json
rm -rf "$tmpdir"

cat >"$EVIDENCE_DIR/INVOCATION.md" <<EOF
# Task GridSite endpoint invocation

- Started: $(date -u +%Y-%m-%dT%H:%M:%SZ)
- Mode: $MODE
- Context: $context
- User: $user
- API server: $server
- API readiness: $ready
- API TLS mode: $API_TLS_MODE
- Run label: $RUN_LABEL
- ODH Grid operator source: $SOURCE_REVISION
- GridNetwork/GridSite: $NAMESPACE/$NETWORK_NAME / $SITE_NAME
- Gateway endpoint: $SITE_ADDRESS
- TLS server name: $SERVER_NAME
- Canonical leaf certificate SHA-256: $fingerprint

The operation only changes the task-owned GridSite gatewayEndpoint and
canonicalFingerprints trust policy. Secret and certificate bytes are not
recorded. No inference traffic is sent.
EOF
printf 'context=%s\nuser=%s\nserver=%s\nreadyz=%s\napi_tls_mode=%s\n' \
  "$context" "$user" "$server" "$ready" "$API_TLS_MODE" >"$EVIDENCE_DIR/api-access-summary.txt"

CURRENT_PHASE=task-gridsite-reconcile
desired_patch="$(jq -cn --arg address "$SITE_ADDRESS" --arg serverName "$SERVER_NAME" --arg fingerprint "$fingerprint" \
  '{spec:{gatewayEndpoint:{address:$address,tls:{mode:"Mutual",serverName:$serverName}},trust:{canonicalFingerprints:[$fingerprint]}}}')"
if [[ "$MODE" == patch ]]; then
  jq -n --argjson patch "$desired_patch" '$patch' >"$EVIDENCE_DIR/gridsite-patch.json"
  oc patch gridsite "$SITE_NAME" -n "$NAMESPACE" --type=merge --patch-file "$EVIDENCE_DIR/gridsite-patch.json" \
    >"$EVIDENCE_DIR/gridsite-patch-result.txt"
elif [[ "$MODE" == verify ]]; then
  oc get gridsite "$SITE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gridsite.json"
  jq -e --arg address "$SITE_ADDRESS" --arg serverName "$SERVER_NAME" --arg fingerprint "$fingerprint" \
    '.spec.gatewayEndpoint.address==$address and .spec.gatewayEndpoint.tls.mode=="Mutual" and .spec.gatewayEndpoint.tls.serverName==$serverName and .spec.trust.canonicalFingerprints==[$fingerprint]' \
    "$EVIDENCE_DIR/gridsite.json" >/dev/null || {
    CURRENT_FAILURE='GridSite endpoint/trust spec differs from the requested task endpoint'
    printf 'live GridSite endpoint/trust does not match the request\n' >&2
    exit 1
  }
fi

CURRENT_PHASE=gridsite-tls-readiness
deadline=$((SECONDS + ${GRID_SITE_READINESS_TIMEOUT_SECONDS:-300}))
while ((SECONDS < deadline)); do
  oc get gridsite "$SITE_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gridsite.json"
  if jq -e '.metadata.generation as $g | .status.observedGeneration==$g and .status.phase=="Active" and .status.reason=="TlsVerified"' \
    "$EVIDENCE_DIR/gridsite.json" >/dev/null; then
    break
  fi
  sleep 5
done
oc get gridnetwork "$NETWORK_NAME" -n "$NAMESPACE" -o json >"$EVIDENCE_DIR/gridnetwork.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:{tls:.spec.tls},status:{phase:.status.phase,routingMapStatus:.status.routingMapStatus,praxisConfigStatus:.status.praxisConfigStatus}}' \
  "$EVIDENCE_DIR/gridnetwork.json" >"$EVIDENCE_DIR/gridnetwork-summary.json"
jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,generation:.metadata.generation,labels:.metadata.labels},spec:.spec,status:.status}' \
  "$EVIDENCE_DIR/gridsite.json" >"$EVIDENCE_DIR/gridsite-summary.json"
oc get events -n "$NAMESPACE" --sort-by=.lastTimestamp >"$EVIDENCE_DIR/events.txt" 2>&1 || true
oc logs "deployment/${GRID_OPERATOR_DEPLOYMENT_NAME:-grid-operator}" -n "$OPERATOR_NAMESPACE" --all-containers=true --tail=200 \
  >"$EVIDENCE_DIR/operator-logs.txt" 2>&1 || true

jq -e '.metadata.generation as $g | .status.observedGeneration==$g and .status.phase=="Active" and .status.reason=="TlsVerified"' \
  "$EVIDENCE_DIR/gridsite.json" >/dev/null || {
  CURRENT_FAILURE="GridSite did not reach current-generation Active/TlsVerified; observed phase=$(jq -r '.status.phase // "unknown"' "$EVIDENCE_DIR/gridsite.json") reason=$(jq -r '.status.reason // "unknown"' "$EVIDENCE_DIR/gridsite.json")"
  printf 'GridSite did not reach current-generation Active/TlsVerified\n' >&2
  exit 1
}

jq -n --arg mode "$MODE" --arg status ready --arg evidence "$EVIDENCE_DIR" \
  --arg address "$SITE_ADDRESS" --arg serverName "$SERVER_NAME" --arg fingerprint "$fingerprint" \
  '{mode:$mode,status:$status,evidenceDirectory:$evidence,gatewayAddress:$address,serverName:$serverName,certificateFingerprint:$fingerprint,inferenceTrafficSent:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
cat >"$EVIDENCE_DIR/source-lock.md" <<EOF
# GridSite endpoint source lock

- ODH Grid operator source revision: $SOURCE_REVISION
- Namespace/GridNetwork/GridSite: $NAMESPACE/$NETWORK_NAME/$SITE_NAME
- Gateway endpoint: $SITE_ADDRESS
- TLS server name: $SERVER_NAME
- Trust policy: canonical X.509 DER SHA-256 fingerprint: $fingerprint
- Current status: Active/TlsVerified at observed generation $(jq -r '.status.observedGeneration' "$EVIDENCE_DIR/gridsite.json")
- No secret value, certificate/key contents, or inference traffic was recorded/sent.
EOF
log "GridSite endpoint $MODE complete: current-generation Active/TlsVerified at $SITE_ADDRESS; no inference traffic sent"
printf 'Evidence: %s\n' "$EVIDENCE_DIR"
