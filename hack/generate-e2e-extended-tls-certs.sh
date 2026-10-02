#!/usr/bin/env bash
# Generate dev TLS material for deploy/overlays/e2e-extended/tls/* (Idle/TLS tranche).
# Not invoked by CI; run manually before wiring Secrets into tls overlays.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TLS_ROOT="${ROOT}/deploy/overlays/e2e-extended/tls"
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

mkdir -p "${TLS_ROOT}/positive/certs"

openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout "${WORKDIR}/ca.key" -out "${WORKDIR}/ca.crt" \
  -days 3650 -subj "/CN=e2e-extended-ca"

openssl req -newkey rsa:2048 -nodes \
  -keyout "${TLS_ROOT}/positive/certs/server.key" \
  -out "${WORKDIR}/server.csr" \
  -subj "/CN=payload-processing.istio-system.svc.cluster.local"

openssl x509 -req -in "${WORKDIR}/server.csr" \
  -CA "${WORKDIR}/ca.crt" -CAkey "${WORKDIR}/ca.key" -CAcreateserial \
  -out "${TLS_ROOT}/positive/certs/server.crt" -days 825 \
  -extfile <(printf "subjectAltName=DNS:payload-processing.istio-system.svc.cluster.local,DNS:payload-pre-processing.istio-system.svc.cluster.local")

cp "${WORKDIR}/ca.crt" "${TLS_ROOT}/positive/certs/ca.crt"

echo "wrote ${TLS_ROOT}/positive/certs/{server.key,server.crt,ca.crt}"
echo "Wire these into tls/positive kustomization Secrets when replacing placeholder overlays."
