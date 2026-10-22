#!/usr/bin/env bash
# Generate certs (if needed) and apply deploy/overlays/e2e-extended/tls/<scenario>/.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCENARIO="${1:-positive}"
CTX="${E2E_CONTEXT:-kind-praxis-e2e}"
NS=istio-system
OVERLAY="${ROOT}/deploy/overlays/e2e-extended/tls/${SCENARIO}"

if [[ ! -d "$OVERLAY" ]]; then
  echo "unknown TLS scenario overlay: ${OVERLAY}" >&2
  exit 1
fi

POSITIVE_CRT="${ROOT}/deploy/overlays/e2e-extended/tls/positive/certs/tls.crt"
need_certs=0
if [[ ! -f "${OVERLAY}/certs/tls.crt" || ! -f "${POSITIVE_CRT}" ]]; then
  need_certs=1
elif ! openssl x509 -in "${POSITIVE_CRT}" -noout -checkend 0 >/dev/null 2>&1; then
  # Leaf expired (or unreadable): regenerate the whole TLS inventory.
  need_certs=1
fi
if [[ "${need_certs}" -eq 1 ]]; then
  bash "${ROOT}/hack/generate-e2e-extended-tls-certs.sh"
fi

# Secret — delete/recreate so mounts always see new PEM bytes.
kubectl --context "$CTX" -n "$NS" delete secret e2e-extended-tls-server --ignore-not-found >/dev/null
kubectl --context "$CTX" -n "$NS" create secret generic e2e-extended-tls-server \
  --from-file=tls.crt="${OVERLAY}/certs/tls.crt" \
  --from-file=tls.key="${OVERLAY}/certs/tls.key" \
  --from-file=ca.crt="${OVERLAY}/certs/ca.crt"

kubectl --context "$CTX" -n "$NS" apply -f "${OVERLAY}/plugins-pre.yaml"
kubectl --context "$CTX" -n "$NS" apply -f "${OVERLAY}/plugins-post.yaml"

# Patch baseline EnvoyFilter clusters (REPLACE via second EnvoyFilter is ignored by istiod).
CA_FILE="${OVERLAY}/certs/ca.crt"
# Server negatives: Envoy must trust the *positive* CA while pods present a bad leaf.
if [[ "$SCENARIO" == server-* ]]; then
  CA_FILE="${ROOT}/deploy/overlays/e2e-extended/tls/positive/certs/ca.crt"
fi
PATCH_ARGS=(--context "$CTX" --namespace "$NS" --ca-file "$CA_FILE")
if [[ -f "${OVERLAY}/certs/client.crt" && -f "${OVERLAY}/certs/client.key" && "$SCENARIO" == client-* && "$SCENARIO" != client-missing ]]; then
  PATCH_ARGS+=(--client-crt "${OVERLAY}/certs/client.crt" --client-key "${OVERLAY}/certs/client.key")
fi
# client-missing: validated server trust, no client identity
# positive / server-*: server TLS only (no client cert)
python3 "${ROOT}/hack/patch-e2e-extended-upstream-tls.py" "${PATCH_ARGS[@]}"

# Strategic-merge patch mounts — does not wipe plugins-config-volume.
kubectl --context "$CTX" -n "$NS" patch deployment payload-pre-processing --type strategic \
  -p '{"spec":{"template":{"spec":{"containers":[{"name":"payload-pre-processing","volumeMounts":[{"name":"tls-certs","mountPath":"/etc/praxis-tls","readOnly":true}]}],"volumes":[{"name":"tls-certs","secret":{"secretName":"e2e-extended-tls-server"}}]}}}}'
kubectl --context "$CTX" -n "$NS" patch deployment payload-processing --type strategic \
  -p '{"spec":{"template":{"spec":{"containers":[{"name":"payload-processing","volumeMounts":[{"name":"tls-certs","mountPath":"/etc/praxis-tls","readOnly":true}]}],"volumes":[{"name":"tls-certs","secret":{"secretName":"e2e-extended-tls-server"}}]}}}}'

kubectl --context "$CTX" -n "$NS" rollout restart deployment/payload-pre-processing deployment/payload-processing
kubectl --context "$CTX" -n "$NS" rollout status deployment/payload-pre-processing --timeout=180s
kubectl --context "$CTX" -n "$NS" rollout status deployment/payload-processing --timeout=180s
sleep 3
echo "applied TLS scenario=${SCENARIO}"
