#!/usr/bin/env bash
# Install Kuadrant via Helm and wire Authorino CA trust for maas-api TLS.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
parse_context "$@"
require_cmd helm
require_cmd kubectl

if ! helm list -n kuadrant-system --kube-context "$KUBE_CONTEXT" 2>/dev/null | grep -q kuadrant-operator; then
  helm repo add kuadrant https://kuadrant.io/helm-charts/ 2>/dev/null || true
  helm repo update kuadrant
  kc create namespace kuadrant-system --dry-run=client -o yaml | kc apply -f -
  helm upgrade --install kuadrant-operator kuadrant/kuadrant-operator \
    --kube-context "$KUBE_CONTEXT" \
    --namespace kuadrant-system \
    --version "$KUADRANT_VERSION" \
    --set manager.env[0].name=ISTIO_GATEWAY_CONTROLLER_NAMES \
    --set manager.env[0].value=istio.io/gateway-controller \
    --wait --timeout 180s
  ok "Kuadrant operator v${KUADRANT_VERSION} installed"
else
  ok "Kuadrant operator already installed"
fi

if ! kc get kuadrant kuadrant -n kuadrant-system &>/dev/null; then
  kc apply -f - <<EOF
apiVersion: kuadrant.io/v1beta1
kind: Kuadrant
metadata:
  name: kuadrant
  namespace: kuadrant-system
spec: {}
EOF
fi

echo "  Waiting for Authorino..."
for _i in $(seq 1 36); do
  kc get deployment authorino -n kuadrant-system &>/dev/null && break
  sleep 5
done
kc wait --for=condition=Available deployment/authorino -n kuadrant-system --timeout=180s
ok "Authorino available"

if kc get configmap maas-ca-bundle -n kuadrant-system &>/dev/null && \
   kc get authorino authorino -n kuadrant-system -o jsonpath='{.spec.volumes.items[0].name}' 2>/dev/null | grep -q maas-ca; then
  ok "Authorino CA trust already configured"
  exit 0
fi

TMP_CA=$(mktemp)
trap 'rm -f "$TMP_CA"' EXIT
kc get secret maas-root-ca -n "$MAAS_NAMESPACE" -o jsonpath='{.data.ca\.crt}' | base64 -d > "$TMP_CA"
[[ -s "$TMP_CA" ]] || die "maas-root-ca secret missing or empty in ${MAAS_NAMESPACE}"

kc create configmap maas-ca-bundle -n kuadrant-system \
  --from-file=maas-ca.crt="$TMP_CA" \
  --dry-run=client -o yaml | kc apply -f -

kc patch authorino authorino -n kuadrant-system --type=merge -p '{
  "spec":{
    "volumes":{
      "items":[{
        "name":"maas-ca",
        "mountPath":"/certs",
        "configMaps":["maas-ca-bundle"],
        "items":[{"key":"maas-ca.crt","path":"ca-bundle.crt"}]
      }]
    }
  }
}'

kc patch deployment authorino -n kuadrant-system --type=strategic -p '{
  "spec":{"template":{"spec":{"containers":[{
    "name":"authorino",
    "env":[{"name":"SSL_CERT_FILE","value":"/certs/ca-bundle.crt"}]
  }]}}}
}'

kc rollout status deployment/authorino -n kuadrant-system --timeout=180s
ok "Authorino CA trust configured for maas-api"
