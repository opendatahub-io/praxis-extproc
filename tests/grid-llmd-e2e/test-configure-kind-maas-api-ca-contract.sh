#!/usr/bin/env bash
# shellcheck disable=SC2016 # assertions intentionally match literal source fragments.
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
helper="$SCRIPT_DIR/configure-kind-maas-api-ca.sh"
[[ -x "$helper" ]] || { printf 'Kind MaaS API CA helper must be executable\n' >&2; exit 1; }

# Reuse the MaaS controller-owned serving Certificate and its run tenant CA.
grep -Fq 'API_CERT="maas-api-serving-cert"' "$helper"
grep -Fq 'API_ISSUER="rhai-ca-issuer"' "$helper"
grep -Fq 'API_ROOT_NAMESPACE="cert-manager"' "$helper"
grep -Fq 'MaaS controller-owned API Certificate' "$helper"
grep -Fq '.kind=="Config" and .name=="default"' "$helper"
grep -Fq 'controllerManaged:true' "$helper"
grep -Fq 'api-certificate-key-public-key-sha256.txt' "$helper"
grep -Fq -- '-verify_hostname maas-api.maas-system.svc ' "$helper"
grep -Fq -- '-verify_hostname maas-api.maas-system.svc.cluster.local ' "$helper"
if grep -Fq -- '-verify_hostname maas-api ' "$helper"; then
  printf 'MaaS TLS verification must use a certificate-covered service FQDN, not the short Service name\n' >&2
  exit 1
fi
if grep -Fq 'name: $API_CERT' "$helper" || grep -Fq 'secretName: $API_SECRET' "$helper"; then
  printf 'Kind MaaS API helper must reuse the controller-owned Certificate and Secret, not create a competing Certificate\n' >&2
  exit 1
fi

# MaaS main's API-key cleanup CronJobs refer to an OpenShift service-CA
# ConfigMap. Kind supplies a run-owned alias containing the controller CA.
grep -Fq 'MAAS_OPENSHIFT_CA_CONFIGMAP="openshift-service-ca.crt"' "$helper"
grep -Fq 'MAAS_OPENSHIFT_CA_KEY="service-ca.crt"' "$helper"
grep -Fq 'MaaS main' "$helper"
grep -Fq 'maas-api-root-ca.crt' "$helper"
grep -Fq -- '--from-file="$MAAS_OPENSHIFT_CA_KEY=' "$helper"
grep -Fq '"praxis.kind/run":$run' "$helper"
grep -Fq 'actual_service_ca_sha' "$helper"
grep -Fq '.maasServiceCA.configMapUID' "$helper"
grep -Fq 'serviceCAConfigMapPreservedUntilClusterDestroy' "$helper"
grep -Fq 'refusing to adopt it' "$helper"
grep -Fq 'dataKeys:(.data|keys)' "$helper"
grep -Fq 'existing_service_ca_sha="$(jq -j' "$helper"
grep -Fq 'actual_service_ca_sha="$(jq -j' "$helper"
grep -Fq 'Recovered a partially applied service CA alias' "$helper"
grep -Fq 'stale-run-certificate-delete.txt' "$helper"
grep -Fq 'refusing to delete an unowned or unexpected MaaS API certificate' "$helper"

# Evidence stores sanitized metadata, not the ConfigMap's certificate bytes.
if grep -Eq 'k get configmap .* -o json >"\$EVIDENCE_DIR/maas-service-ca-configmap.*json' "$helper"; then
  printf 'MaaS service CA helper must not save raw ConfigMap certificate material to evidence\n' >&2
  exit 1
fi
printf 'Kind MaaS API CA contract passed\n'
