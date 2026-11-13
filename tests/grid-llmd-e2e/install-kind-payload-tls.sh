#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
RUN_ID=""
NAMESPACE="maas-system"
ISSUER="rhai-ca-issuer"
CA_SECRET_NAME=""
CA_SECRET_UID=""
CA_FILE=""
CERTIFICATES_FILE=""
EVIDENCE_DIR=""
TIMEOUT=180

usage() {
  cat <<'USAGE'
Usage: install-kind-payload-tls.sh --context CTX --kubeconfig FILE \
  --expected-api-server URL --run-id ID --ca-secret NAME --ca-secret-uid UID \
  --ca-file PUBLIC-CA.pem --certificates-file YAML --evidence-dir NEW-DIR

Verifies the run-owned MaaS issuer and public CA, installs six run-owned
cert-manager Certificates (the shared payload Services and both tenant pool
Service pairs) plus the CA-only Istio credential Secret, and proves every leaf
certificate/Secret pair before the AIGC Deployments exist.
Secret values and private keys are never written to evidence.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="${2:?}"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="${2:?}"; shift 2 ;;
    --run-id) RUN_ID="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --issuer) ISSUER="${2:?}"; shift 2 ;;
    --ca-secret) CA_SECRET_NAME="${2:?}"; shift 2 ;;
    --ca-secret-uid) CA_SECRET_UID="${2:?}"; shift 2 ;;
    --ca-file) CA_FILE="${2:?}"; shift 2 ;;
    --certificates-file) CERTIFICATES_FILE="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="${2:?}"; shift 2 ;;
    --timeout-seconds) TIMEOUT="${2:?}"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { printf 'install-kind-payload-tls: %s\n' "$*" >&2; exit 2; }
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" && -n "$RUN_ID" ]] || fail 'explicit context, private kubeconfig, API server, and run ID are required'
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || fail 'run ID is not a valid ownership label'
[[ -f "$CA_FILE" && -s "$CA_FILE" && -f "$CERTIFICATES_FILE" && -s "$CERTIFICATES_FILE" ]] || fail 'CA and certificate manifest files must exist and be nonempty'
[[ -n "$CA_SECRET_NAME" && -n "$CA_SECRET_UID" && -n "$EVIDENCE_DIR" ]] || fail 'CA Secret identity and a new evidence directory are required'
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ ]] || fail 'timeout must be a positive number of seconds'
[[ ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory already exists; refusing overwrite'
[[ "$(stat -c '%a' "$KUBECONFIG_FILE")" == 600 || "$(stat -c '%a' "$KUBECONFIG_FILE")" == 400 ]] || fail 'kubeconfig must have mode 0600 or 0400'
for binary in kubectl jq python3 openssl base64 sha256sum; do command -v "$binary" >/dev/null || fail "$binary is required"; done

mkdir -p "$(dirname "$EVIDENCE_DIR")"
mkdir -m 0700 "$EVIDENCE_DIR"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kind-payload-tls.XXXXXXXX")"
chmod 0700 "$WORK_DIR"
on_exit() {
  local rc=$?
  trap - EXIT
  if ((rc != 0)); then
    capture_failure
    jq -n --arg run "$RUN_ID" --arg evidence "$EVIDENCE_DIR" --argjson exitCode "$rc" \
      '{status:"FAILED",run:$run,exitCode:$exitCode,evidenceDirectory:$evidence,secretValuesRecorded:false,privateKeysRecorded:false}' \
      >"$EVIDENCE_DIR/RESULT.json"
  fi
  [[ ! -d "$WORK_DIR" ]] || rm -r -- "$WORK_DIR"
  exit "$rc"
}
trap on_exit EXIT

k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
capture_failure() {
  set +e
  k get clusterissuer "$ISSUER" -o json 2>/dev/null \
    | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},status:{conditions:.status.conditions}}' \
    >"$EVIDENCE_DIR/issuer-on-failure.json"
  k get certificates.cert-manager.io -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{secretName:.spec.secretName,dnsNames:.spec.dnsNames,issuerRef:.spec.issuerRef},status:{conditions:.status.conditions}}]' \
    >"$EVIDENCE_DIR/certificates-on-failure.json"
  k get secrets -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|select(.metadata.name|startswith("payload-"))|{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,annotations:.metadata.annotations},type:.type,dataKeys:((.data//{})|keys)}]' \
    >"$EVIDENCE_DIR/secrets-on-failure.json"
  k get deployments -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|select(.metadata.name=="payload-pre-processing" or .metadata.name=="payload-processing")|{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,template:{labels:.spec.template.metadata.labels,volumes:[.spec.template.spec.volumes[]?|{name,secretName:.secret.secretName}],containers:[.spec.template.spec.containers[]?|{name,image,ports,resources,volumeMounts}]}},status:.status}]' \
    >"$EVIDENCE_DIR/deployments-on-failure.json"
  k get pods -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|select(.metadata.labels.app=="payload-pre-processing" or .metadata.labels.app=="payload-processing")|{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,deletionTimestamp:.metadata.deletionTimestamp},spec:{nodeName:.spec.nodeName,containers:[.spec.containers[]?|{name,image,volumeMounts}]},status:{phase:.status.phase,conditions:.status.conditions,containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount,state}]}}]' \
    >"$EVIDENCE_DIR/pods-on-failure.json"
  k get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json 2>/dev/null \
    | jq '[.items[-100:][]|{reason,type,message:.message,lastTimestamp:.lastTimestamp,involvedObject:.involvedObject}]' \
    >"$EVIDENCE_DIR/events-on-failure.json"
  set -e
}

actual_server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$actual_server" == "$EXPECTED_API_SERVER" ]] || fail 'selected context API server differs from expected API server'
[[ "$(k get --raw=/readyz)" == *ok* ]] || fail 'Kubernetes API is not ready'
namespace_json="$(k get namespace "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run and .metadata.labels["app.kubernetes.io/managed-by"]=="grid-llmd-kind"' \
  <<<"$namespace_json" >/dev/null || fail "namespace $NAMESPACE is not owned by this run"

issuer_json="$WORK_DIR/issuer.json"
k get clusterissuer "$ISSUER" -o json >"$issuer_json"
jq -e '.metadata.generation as $g | any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g) and (.spec.ca.secretName|type=="string" and length>0)' \
  "$issuer_json" >/dev/null || fail "ClusterIssuer/$ISSUER is not Ready with a CA Secret"
issuer_ca_secret="$(jq -er '.spec.ca.secretName' "$issuer_json")"
[[ "$issuer_ca_secret" == "$CA_SECRET_NAME" ]] || fail "ClusterIssuer/$ISSUER references an unexpected CA Secret"
issuer_ca_json="$WORK_DIR/issuer-ca.json"
k get secret "$CA_SECRET_NAME" -n cert-manager -o json >"$issuer_ca_json"
jq -e --arg uid "$CA_SECRET_UID" '.metadata.uid==$uid and (.data["tls.crt"]|type=="string" and length>0)' \
  "$issuer_ca_json" >/dev/null || fail 'CA Secret UID or certificate data does not match the recorded run identity'
jq -r '.data["tls.crt"]' "$issuer_ca_json" | base64 -d >"$WORK_DIR/issuer-ca.pem"
chmod 0600 "$WORK_DIR/issuer-ca.pem"
ca_fingerprint="$(openssl x509 -in "$CA_FILE" -noout -fingerprint -sha256 | sed 's/^[^=]*=//;s/://g')"
issuer_ca_fingerprint="$(openssl x509 -in "$WORK_DIR/issuer-ca.pem" -noout -fingerprint -sha256 | sed 's/^[^=]*=//;s/://g')"
[[ "$ca_fingerprint" == "$issuer_ca_fingerprint" ]] || fail 'renderer CA fingerprint differs from the active ClusterIssuer root certificate'

# Validate the manifest contract before applying any resource.
python3 - "$CERTIFICATES_FILE" "$RUN_ID" "$NAMESPACE" "$ISSUER" "$WORK_DIR/expected-certificates.txt" <<'PY'
import sys
from pathlib import Path
import yaml

path, run_id, namespace, issuer, names_path = sys.argv[1:]
docs = [x for x in yaml.safe_load_all(Path(path).read_text()) if isinstance(x, dict)]
services = ["payload-pre-processing", "payload-processing"]
services += [f"{service}-{pool}" for pool in ("pool-a", "pool-b") for service in ("payload-pre-processing", "payload-processing")]
expected = {f"{service}-tls": f"{service}.{namespace}.svc.cluster.local" for service in services}
if len(docs) != len(expected):
    raise SystemExit(f"certificate manifest must contain exactly {len(expected)} shared and tenant-pool Certificates")
seen = set()
for doc in docs:
    meta, spec = doc.get("metadata", {}), doc.get("spec", {})
    name = meta.get("name")
    if doc.get("apiVersion") != "cert-manager.io/v1" or doc.get("kind") != "Certificate":
        raise SystemExit("manifest contains a non-Certificate resource")
    if name not in expected or name in seen or meta.get("namespace") != namespace:
        raise SystemExit("Certificate name or namespace is outside the expected payload set")
    seen.add(name)
    if meta.get("labels", {}).get("praxis.kind/run") != run_id:
        raise SystemExit(f"Certificate/{name} lacks the current run ownership label")
    if spec.get("secretName") != name or spec.get("dnsNames") != [expected[name]]:
        raise SystemExit(f"Certificate/{name} has a Secret or DNS SAN contract mismatch")
    if spec.get("issuerRef") != {"name": issuer, "kind": "ClusterIssuer", "group": "cert-manager.io"}:
        raise SystemExit(f"Certificate/{name} does not use the expected ClusterIssuer")
    if spec.get("secretTemplate", {}).get("labels", {}).get("praxis.kind/run") != run_id:
        raise SystemExit(f"Certificate/{name} does not propagate run ownership to its generated Secret")
if seen != set(expected):
    raise SystemExit(f"certificate manifest is missing expected names: {sorted(set(expected) - seen)}")
Path(names_path).write_text("".join(name + "\n" for name in expected))
PY
mapfile -t certificate_names <"$WORK_DIR/expected-certificates.txt"

certificate_resources=()
for cert_name in "${certificate_names[@]}"; do
  certificate_resources+=("certificate/$cert_name")
  if k get certificate "$cert_name" -n "$NAMESPACE" -o name >"$WORK_DIR/preexisting-$cert_name.txt" 2>/dev/null; then
    fail "refusing to adopt pre-existing Certificate/$cert_name"
  fi
done
if k get secret payload-extproc-serving-ca -n "$NAMESPACE" -o json >"$WORK_DIR/preexisting-trust-secret.json" 2>/dev/null; then
  jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run' "$WORK_DIR/preexisting-trust-secret.json" >/dev/null \
    || fail 'refusing to adopt a pre-existing payload-extproc-serving-ca Secret'
  fail 'run-owned payload-extproc-serving-ca Secret already exists; expected a fresh namespace'
fi

trust_manifest="$WORK_DIR/payload-extproc-serving-ca.yaml"
kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" create secret generic payload-extproc-serving-ca \
  --namespace "$NAMESPACE" --from-file="ca.crt=$CA_FILE" --dry-run=client -o yaml \
  | python3 -c 'import sys,yaml; x=yaml.safe_load(sys.stdin); x.setdefault("metadata",{}).setdefault("labels",{}).update({"app.kubernetes.io/managed-by":"grid-llmd-kind","app.kubernetes.io/part-of":"grid-llmd-e2e","praxis.kind/run":sys.argv[1]}); print(yaml.safe_dump(x,sort_keys=False))' "$RUN_ID" \
  >"$trust_manifest"
chmod 0600 "$trust_manifest"
k apply --dry-run=server -f "$trust_manifest" >"$EVIDENCE_DIR/trust-secret-server-dry-run.txt"
k apply --dry-run=server -f "$CERTIFICATES_FILE" >"$EVIDENCE_DIR/certificates-server-dry-run.txt"
k apply -f "$trust_manifest" >"$EVIDENCE_DIR/trust-secret-apply.txt"
k apply -f "$CERTIFICATES_FILE" >"$EVIDENCE_DIR/certificates-apply.txt"
k wait --for=condition=Ready "${certificate_resources[@]}" -n "$NAMESPACE" --timeout="${TIMEOUT}s" \
  >"$EVIDENCE_DIR/certificates-ready-wait.txt"

for cert_name in "${certificate_names[@]}"; do
  cert_json="$WORK_DIR/$cert_name-certificate.json"
  secret_json="$WORK_DIR/$cert_name-secret.json"
  k get certificate "$cert_name" -n "$NAMESPACE" -o json >"$cert_json"
  k get secret "$cert_name" -n "$NAMESPACE" -o json >"$secret_json"
  fqdn="${cert_name%-tls}.$NAMESPACE.svc.cluster.local"
  cert_generation="$(jq -er '.metadata.generation' "$cert_json")"
  jq -e --arg run "$RUN_ID" --arg fqdn "$fqdn" --arg issuer "$ISSUER" --argjson generation "$cert_generation" \
    '.metadata.labels["praxis.kind/run"]==$run and
     any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$generation) and
     .spec.secretName==.metadata.name and .spec.dnsNames==[$fqdn] and .spec.issuerRef.name==$issuer' \
    "$cert_json" >/dev/null || fail "Certificate/$cert_name is not Ready at the current generation with the exact Service DNS SAN"
  jq -e --arg run "$RUN_ID" '(.metadata.labels["praxis.kind/run"]==$run) and .type=="kubernetes.io/tls" and
    (.data["tls.crt"]|type=="string" and length>0) and (.data["tls.key"]|type=="string" and length>0)' \
    "$secret_json" >/dev/null || fail "Secret/$cert_name is missing run-owned TLS key material"
  jq -r '.data["tls.crt"]' "$secret_json" | base64 -d >"$WORK_DIR/$cert_name.crt"
  jq -r '.data["tls.key"]' "$secret_json" | base64 -d >"$WORK_DIR/$cert_name.key"
  chmod 0600 "$WORK_DIR/$cert_name.crt" "$WORK_DIR/$cert_name.key"
  openssl verify -CAfile "$CA_FILE" -purpose sslserver -verify_hostname "$fqdn" "$WORK_DIR/$cert_name.crt" \
    >"$EVIDENCE_DIR/$cert_name-chain-and-hostname-verify.txt"
  cert_public_key="$(openssl x509 -in "$WORK_DIR/$cert_name.crt" -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum | awk '{print $1}')"
  key_public_key="$(openssl pkey -in "$WORK_DIR/$cert_name.key" -pubout -outform DER | sha256sum | awk '{print $1}')"
  [[ "$cert_public_key" == "$key_public_key" ]] || fail "Secret/$cert_name certificate and private key do not match"
  openssl x509 -in "$WORK_DIR/$cert_name.crt" -noout -issuer -subject -serial -fingerprint -sha256 -ext subjectAltName \
    >"$EVIDENCE_DIR/$cert_name-certificate-summary.txt"
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{secretName:.spec.secretName,dnsNames:.spec.dnsNames,issuerRef:.spec.issuerRef},status:{observedGeneration:.status.observedGeneration,conditions:.status.conditions}}' \
    "$cert_json" >"$EVIDENCE_DIR/$cert_name-certificate.json"
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,resourceVersion:.metadata.resourceVersion,labels:.metadata.labels},type:.type,dataKeys:((.data//{})|keys),keyMaterialRecorded:false}' \
    "$secret_json" >"$EVIDENCE_DIR/$cert_name-secret-summary.json"
  printf '{"service":"%s","dnsName":"%s","certificateReady":true,"secretAvailable":true,"certificateKeyMatch":true,"issuerChainVerified":true}\n' \
    "${cert_name%-tls}" "$fqdn" >"$EVIDENCE_DIR/$cert_name-result.json"
done

trust_json="$WORK_DIR/trust-secret.json"
k get secret payload-extproc-serving-ca -n "$NAMESPACE" -o json >"$trust_json"
jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run and .type=="Opaque" and (.data["ca.crt"]|type=="string" and length>0) and ((.data//{}|keys)==["ca.crt"])' \
  "$trust_json" >/dev/null || fail 'Istio trust Secret is not run-owned or does not contain only ca.crt'
jq -r '.data["ca.crt"]' "$trust_json" | base64 -d >"$WORK_DIR/trust-ca.pem"
trust_ca_fingerprint="$(openssl x509 -in "$WORK_DIR/trust-ca.pem" -noout -fingerprint -sha256 | sed 's/^[^=]*=//;s/://g')"
[[ "$ca_fingerprint" == "$trust_ca_fingerprint" ]] || fail 'Istio trust Secret CA fingerprint differs from the ClusterIssuer root certificate'
# Hash the public trust value without retaining it in the evidence document.
trust_sha256="$(sha256sum "$WORK_DIR/trust-ca.pem" | awk '{print $1}')"
jq --arg digest "$trust_sha256" '{metadata:.metadata,type:.type,dataKeys:.dataKeys,caCertificateSha256:$digest,privateKeyRecorded:false}' \
  < <(jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels},type:.type,dataKeys:((.data//{})|keys)}' "$trust_json") \
  >"$EVIDENCE_DIR/trust-secret-summary.json"
k get clusterissuer "$ISSUER" -o json \
  | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},spec:{caSecretName:.spec.ca.secretName},status:{conditions:.status.conditions}}' \
  >"$EVIDENCE_DIR/issuer-summary.json"
jq -n --arg run "$RUN_ID" --arg namespace "$NAMESPACE" --arg issuer "$ISSUER" --arg caSha256 "$ca_fingerprint" \
  --argjson certificates "$(printf '%s\n' "${certificate_names[@]}" | jq -R . | jq -s .)" \
  --arg apiServer "$actual_server" \
  '{status:"PASS",runId:$run,namespace:$namespace,issuer:$issuer,apiServer:$apiServer,caCertificateFingerprintSha256:$caSha256,
    certificates:$certificates,certificateSecretsReady:true,certificateKeyPairsVerified:true,
    serviceDNSNamesVerified:true,issuerChainVerified:true,istioTrustSecretReady:true,secretValuesRecorded:false,privateKeysRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'Issued six run-owned shared and pool payload Certificates; verified SANs, matching TLS keys, issuer chain, and Gateway CA trust Secret\n'
