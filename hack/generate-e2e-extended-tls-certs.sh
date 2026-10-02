#!/usr/bin/env bash
# Generate TLS material + EnvoyFilter patches for deploy/overlays/e2e-extended/tls/*.
# Invoked by make e2e-setup-extended / apply-e2e-extended-tls.sh (not default CI).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TLS_ROOT="${ROOT}/deploy/overlays/e2e-extended/tls"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

PRE_SNI="payload-pre-processing.istio-system.svc.cluster.local"
POST_SNI="payload-processing.istio-system.svc.cluster.local"
SAN_BOTH="DNS:${PRE_SNI},DNS:${POST_SNI}"
CLIENT_SAN="DNS:e2e-gateway.istio-system.svc.cluster.local"

gen_ca() {
  local dir="$1" cn="$2"
  mkdir -p "$dir"
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout "${dir}/ca.key" -out "${dir}/ca.crt" \
    -days 3650 -subj "/CN=${cn}" 2>/dev/null
}

# gen_leaf OUT_DIR CN SAN CA_DIR [NOT_BEFORE NOT_AFTER]
# Dated leaves use `openssl ca -startdate/-enddate` (OpenSSL 3.0 / Ubuntu 24.04);
# `x509 -req -not_before/-not_after` requires OpenSSL 3.4+.
gen_leaf() {
  local out_dir="$1" cn="$2" san="$3" ca_dir="$4"
  local not_before="${5:-}" not_after="${6:-}"
  mkdir -p "$out_dir"
  openssl req -newkey rsa:2048 -nodes \
    -keyout "${out_dir}/tls.key" \
    -out "${WORKDIR}/leaf.csr" \
    -subj "/CN=${cn}" 2>/dev/null
  printf "subjectAltName=%s\n" "$san" >"${WORKDIR}/san.ext"
  if [[ -n "$not_before" && -n "$not_after" ]]; then
    local ca_db ca_cfg
    ca_db="$(mktemp -d "${WORKDIR}/ca-db.XXXXXX")"
    ca_cfg="${ca_db}/ca.cnf"
    mkdir -p "${ca_db}/newcerts"
    : >"${ca_db}/index.txt"
    echo 1000 >"${ca_db}/serial"
    cat >"${ca_cfg}" <<EOF
[ ca ]
default_ca = CA_default

[ CA_default ]
dir               = ${ca_db}
database          = ${ca_db}/index.txt
new_certs_dir     = ${ca_db}/newcerts
serial            = ${ca_db}/serial
default_md        = sha256
policy            = policy_anything
email_in_dn       = no
unique_subject    = no

[ policy_anything ]
commonName        = supplied

[ v3_req ]
basicConstraints  = CA:FALSE
keyUsage          = digitalSignature, keyEncipherment
extendedKeyUsage  = serverAuth, clientAuth
subjectAltName    = ${san}
EOF
    openssl ca -batch \
      -config "${ca_cfg}" \
      -cert "${ca_dir}/ca.crt" \
      -keyfile "${ca_dir}/ca.key" \
      -in "${WORKDIR}/leaf.csr" \
      -out "${out_dir}/tls.crt" \
      -startdate "${not_before}" \
      -enddate "${not_after}" \
      -extensions v3_req \
      -notext
  else
    openssl x509 -req -in "${WORKDIR}/leaf.csr" \
      -CA "${ca_dir}/ca.crt" -CAkey "${ca_dir}/ca.key" -CAcreateserial \
      -out "${out_dir}/tls.crt" -days 825 -extfile "${WORKDIR}/san.ext" 2>/dev/null
  fi
  cp "${ca_dir}/ca.crt" "${out_dir}/ca.crt"
}

indent_pem() {
  local file="$1" spaces="$2"
  sed "s/^/${spaces}/" "$file"
}

# Emit one full upstream cluster (REPLACE — MERGE cannot clear ACCEPT_UNTRUSTED).
cluster_patch() {
  local cluster_name="$1" sni="$2" svc_host="$3" ca_indented="$4" client_block="$5"
  cat <<EOF
    - applyTo: CLUSTER
      match:
        context: GATEWAY
        cluster:
          name: ${cluster_name}
      patch:
        operation: REPLACE
        value:
          name: ${cluster_name}
          type: STRICT_DNS
          connect_timeout: 5s
          typed_extension_protocol_options:
            envoy.extensions.upstreams.http.v3.HttpProtocolOptions:
              "@type": type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions
              explicit_http_config:
                http2_protocol_options: {}
          transport_socket:
            name: envoy.transport_sockets.tls
            typed_config:
              "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext
              sni: ${sni}
              common_tls_context:
                alpn_protocols: ["h2"]
${client_block}                validation_context:
                  trust_chain_verification: VERIFY_TRUST_CHAIN
                  trusted_ca:
                    inline_string: |
${ca_indented}
          load_assignment:
            cluster_name: ${cluster_name}
            endpoints:
              - lb_endpoints:
                  - endpoint:
                      address:
                        socket_address:
                          address: ${svc_host}
                          port_value: 9004
EOF
}

# write_envoyfilter OUT CA_PEM WITH_CLIENT [CLIENT_CRT CLIENT_KEY]
write_envoyfilter() {
  local out_path="$1" ca_pem="$2" with_client="$3"
  local client_crt="${4:-}" client_key="${5:-}"
  local ca_indented client_block=""
  # PEM bodies must be indented deeper than their `inline_string: |` keys.
  ca_indented="$(indent_pem "$ca_pem" "                      ")"

  if [[ "$with_client" == "1" ]]; then
    local crt_indented key_indented
    crt_indented="$(indent_pem "$client_crt" "                        ")"
    key_indented="$(indent_pem "$client_key" "                        ")"
    client_block=$(cat <<EOF
                tls_certificates:
                  - certificate_chain:
                      inline_string: |
${crt_indented}
                    private_key:
                      inline_string: |
${key_indented}
EOF
)
  fi

  mkdir -p "$(dirname "$out_path")"
  {
    cat <<EOF
# GENERATED by hack/generate-e2e-extended-tls-certs.sh — do not hand-edit.
apiVersion: networking.istio.io/v1alpha3
kind: EnvoyFilter
metadata:
  name: e2e-extended-upstream-tls
  namespace: istio-system
  labels:
    app.kubernetes.io/component: e2e-extended
spec:
  # Apply after baseline praxis-extproc ADD patches so REPLACE wins.
  priority: 100
  workloadSelector:
    labels:
      gateway.networking.k8s.io/gateway-name: e2e-gateway
  configPatches:
EOF
    cluster_patch "praxis-pre-processing-grpc" "$PRE_SNI" \
      "payload-pre-processing.istio-system.svc.cluster.local" \
      "$ca_indented" "$client_block"
    cluster_patch "praxis-processing-grpc" "$POST_SNI" \
      "payload-processing.istio-system.svc.cluster.local" \
      "$ca_indented" "$client_block"
  } >"$out_path"
}

echo "Generating e2e-extended TLS material under ${TLS_ROOT}"

gen_ca "${WORKDIR}/positive-ca" "e2e-extended-ca"
gen_ca "${WORKDIR}/alt-ca" "e2e-extended-alt-ca"

POS="${TLS_ROOT}/positive/certs"
gen_leaf "$POS" "$POST_SNI" "$SAN_BOTH" "${WORKDIR}/positive-ca"
gen_leaf "${WORKDIR}/positive-client" "e2e-extended-envoy-client" \
  "$CLIENT_SAN" "${WORKDIR}/positive-ca"
cp "${WORKDIR}/positive-client/tls.crt" "${POS}/client.crt"
cp "${WORKDIR}/positive-client/tls.key" "${POS}/client.key"
# Positive path: validated server TLS only (mTLS is exercised in client-* cells).
write_envoyfilter "${TLS_ROOT}/positive/envoyfilter-upstream-tls.yaml" \
  "${POS}/ca.crt" 0

SU="${TLS_ROOT}/server-untrusted/certs"
gen_leaf "$SU" "$POST_SNI" "$SAN_BOTH" "${WORKDIR}/alt-ca"
write_envoyfilter "${TLS_ROOT}/server-untrusted/envoyfilter-upstream-tls.yaml" \
  "${WORKDIR}/positive-ca/ca.crt" 0

SE="${TLS_ROOT}/server-expired/certs"
gen_leaf "$SE" "$POST_SNI" "$SAN_BOTH" "${WORKDIR}/positive-ca" \
  "20200101000000Z" "20200102000000Z"
write_envoyfilter "${TLS_ROOT}/server-expired/envoyfilter-upstream-tls.yaml" \
  "${WORKDIR}/positive-ca/ca.crt" 0

SWS="${TLS_ROOT}/server-wrong-san/certs"
gen_leaf "$SWS" "wrong.example" "DNS:wrong.example" "${WORKDIR}/positive-ca"
write_envoyfilter "${TLS_ROOT}/server-wrong-san/envoyfilter-upstream-tls.yaml" \
  "${WORKDIR}/positive-ca/ca.crt" 0

CM="${TLS_ROOT}/client-missing/certs"
mkdir -p "$CM"
cp "${POS}/tls.crt" "${POS}/tls.key" "${POS}/ca.crt" "$CM/"
write_envoyfilter "${TLS_ROOT}/client-missing/envoyfilter-upstream-tls.yaml" \
  "${CM}/ca.crt" 0

CU="${TLS_ROOT}/client-untrusted/certs"
mkdir -p "$CU"
cp "${POS}/tls.crt" "${POS}/tls.key" "${POS}/ca.crt" "$CU/"
gen_leaf "${WORKDIR}/bad-client" "e2e-extended-bad-client" \
  "$CLIENT_SAN" "${WORKDIR}/alt-ca"
cp "${WORKDIR}/bad-client/tls.crt" "${CU}/client.crt"
cp "${WORKDIR}/bad-client/tls.key" "${CU}/client.key"
write_envoyfilter "${TLS_ROOT}/client-untrusted/envoyfilter-upstream-tls.yaml" \
  "${CU}/ca.crt" 1 "${CU}/client.crt" "${CU}/client.key"

CE="${TLS_ROOT}/client-expired/certs"
mkdir -p "$CE"
cp "${POS}/tls.crt" "${POS}/tls.key" "${POS}/ca.crt" "$CE/"
gen_leaf "${WORKDIR}/expired-client" "e2e-extended-expired-client" \
  "$CLIENT_SAN" "${WORKDIR}/positive-ca" \
  "20200101000000Z" "20200102000000Z"
cp "${WORKDIR}/expired-client/tls.crt" "${CE}/client.crt"
cp "${WORKDIR}/expired-client/tls.key" "${CE}/client.key"
write_envoyfilter "${TLS_ROOT}/client-expired/envoyfilter-upstream-tls.yaml" \
  "${CE}/ca.crt" 1 "${CE}/client.crt" "${CE}/client.key"

echo "Wrote certs + envoyfilter-upstream-tls.yaml for all TLS scenarios."
