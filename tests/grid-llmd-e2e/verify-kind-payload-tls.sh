#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Verify the complete Kind AIGC payload certificate, Service, and Gateway TLS path.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTEXT=""; KCFG=""; API=""; RUN=""; CA_FILE=""; CA_SECRET_UID=""; ISSUER_CA_SECRET=""; ISSUER_CA_SECRET_UID=""; EVIDENCE=""; TIMEOUT=300; NAMESPACE=maas-system; POOL=""
while (($#)); do
  case "$1" in
    --context) CONTEXT="${2:?}"; shift 2 ;;
    --kubeconfig) KCFG="${2:?}"; shift 2 ;;
    --expected-api-server) API="${2:?}"; shift 2 ;;
    --run-id) RUN="${2:?}"; shift 2 ;;
    --ca-file) CA_FILE="${2:?}"; shift 2 ;;
    --ca-secret-uid) CA_SECRET_UID="${2:?}"; shift 2 ;;
    --issuer-ca-secret) ISSUER_CA_SECRET="${2:?}"; shift 2 ;;
    --issuer-ca-secret-uid) ISSUER_CA_SECRET_UID="${2:?}"; shift 2 ;;
    --namespace) NAMESPACE="${2:?}"; shift 2 ;;
    --pool) POOL="${2:?}"; shift 2 ;;
    --evidence-dir) EVIDENCE="${2:?}"; shift 2 ;;
    --timeout-seconds) TIMEOUT="${2:?}"; shift 2 ;;
    --help|-h)
      echo 'Usage: verify-kind-payload-tls.sh --context CTX --kubeconfig FILE --expected-api-server URL --run-id ID --ca-file PUBLIC-CA.pem --ca-secret-uid GATEWAY-TRUST-SECRET-UID --issuer-ca-secret NAME --issuer-ca-secret-uid UID --evidence-dir NEW-DIR [--namespace NS] [--pool pool-a|pool-b] [--timeout-seconds N]'
      exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[[ -z "$POOL" || "$POOL" == pool-a || "$POOL" == pool-b ]] || { echo '--pool must be pool-a or pool-b' >&2; exit 2; }
SERVICE_PRE="payload-pre-processing${POOL:+-$POOL}"
SERVICE_PROCESSING="payload-processing${POOL:+-$POOL}"
SERVICES=("$SERVICE_PRE" "$SERVICE_PROCESSING")
GATEWAY_NAME="${POOL:+grid-llmd-$POOL}"
[[ -n "$GATEWAY_NAME" ]] || GATEWAY_NAME=maas-default-gateway
ENVOY_FILTER="payload-processing${POOL:+-$POOL}"

fail() {
  local reason="$*"
  printf 'verify-kind-payload-tls: %s\n' "$reason" >&2
  if declare -F capture_failure >/dev/null && [[ -d "${EVIDENCE:-}" ]]; then
    capture_failure
    jq -n --arg run "$RUN" --arg reason "$reason" --arg evidence "$EVIDENCE" \
      '{status:"FAILED",run:$run,reason:$reason,evidenceDirectory:$evidence,secretValuesRecorded:false,privateKeysRecorded:false}' \
      >"$EVIDENCE/RESULT.json"
  fi
  exit 2
}
[[ -n "$CONTEXT" && -f "$KCFG" && -n "$API" && -n "$RUN" && -s "$CA_FILE" && -n "$CA_SECRET_UID" && -n "$ISSUER_CA_SECRET" && -n "$ISSUER_CA_SECRET_UID" && -n "$EVIDENCE" ]] \
  || fail 'explicit cluster identity, run ID, issuer CA, and new evidence directory are required'
[[ "$TIMEOUT" =~ ^[1-9][0-9]*$ && ! -e "$EVIDENCE" ]] || fail 'timeout must be positive and evidence directory new'
[[ "$(stat -c '%a' "$KCFG")" == 600 || "$(stat -c '%a' "$KCFG")" == 400 ]] || fail 'kubeconfig must be mode 0600 or 0400'
for binary in kubectl jq python3 openssl timeout sha256sum; do command -v "$binary" >/dev/null || fail "$binary is required"; done
mkdir -p "$(dirname "$EVIDENCE")"; mkdir -m 0700 "$EVIDENCE"
WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/kind-payload-tls-verify.XXXXXXXX")"
chmod 0700 "$WORK_DIR"
PORT_FORWARD_PID=""
cleanup() {
  if [[ -n "$PORT_FORWARD_PID" ]]; then
    kill "$PORT_FORWARD_PID" 2>/dev/null || true
    wait "$PORT_FORWARD_PID" 2>/dev/null || true
    PORT_FORWARD_PID=""
  fi
  [[ ! -d "$WORK_DIR" ]] || rm -r -- "$WORK_DIR"
}
capture_failure() {
  set +e
  k get certificates.cert-manager.io -n "$NAMESPACE" -o json 2>/dev/null \
    | jq --arg pre "$SERVICE_PRE-tls" --arg processing "$SERVICE_PROCESSING-tls" '[.items[]|select(.metadata.name==$pre or .metadata.name==$processing)|{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{secretName:.spec.secretName,dnsNames:.spec.dnsNames,issuerRef:.spec.issuerRef},status:{conditions:.status.conditions}}]' \
    >"$EVIDENCE/certificates-on-failure.json"
  k get secrets -n "$NAMESPACE" -o json 2>/dev/null \
    | jq --arg pre "$SERVICE_PRE-tls" --arg processing "$SERVICE_PROCESSING-tls" '[.items[]|select(.metadata.name==$pre or .metadata.name==$processing or .metadata.name=="payload-extproc-serving-ca")|{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels},type:.type,dataKeys:((.data//{})|keys)}]' \
    >"$EVIDENCE/secrets-on-failure.json"
  k get deployments -n "$NAMESPACE" -o json 2>/dev/null \
    | jq --arg pre "$SERVICE_PRE" --arg processing "$SERVICE_PROCESSING" '[.items[]|select(.metadata.name==$pre or .metadata.name==$processing)|{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,template:{labels:.spec.template.metadata.labels,volumes:.spec.template.spec.volumes,containers:[.spec.template.spec.containers[]?|{name,image,volumeMounts,resources}]}},status:.status}]' \
    >"$EVIDENCE/deployments-on-failure.json"
  k get pods -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|select(.metadata.name|startswith("payload-"))|{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,deletionTimestamp:.metadata.deletionTimestamp},spec:{nodeName:.spec.nodeName},status:{phase:.status.phase,conditions:.status.conditions,containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount,state}]}}]' \
    >"$EVIDENCE/pods-on-failure.json"
  k get services "${SERVICES[@]}" -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid},spec:{clusterIP:.spec.clusterIP,selector:.spec.selector,ports:.spec.ports}}]' \
    >"$EVIDENCE/services-on-failure.json"
  k get endpointslices -n "$NAMESPACE" -o json 2>/dev/null \
    | jq --arg pre "$SERVICE_PRE" --arg processing "$SERVICE_PROCESSING" '[.items[]|select(.metadata.labels["kubernetes.io/service-name"]==$pre or .metadata.labels["kubernetes.io/service-name"]==$processing)|{metadata:{name:.metadata.name,labels:.metadata.labels},ports:.ports,endpoints:[.endpoints[]?|{addresses,conditions,targetRef}]}]' \
    >"$EVIDENCE/endpointslices-on-failure.json"
  k get destinationrules.networking.istio.io "${SERVICES[@]}" -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},spec:.spec}]' >"$EVIDENCE/destination-rules-on-failure.json"
  k get envoyfilter.networking.istio.io "$ENVOY_FILTER" -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},workloadSelector:.spec.workloadSelector,configPatches:[.spec.configPatches[]?|select(.applyTo=="CLUSTER")|{applyTo,patch:{operation:.patch.operation,value:{name:.patch.value.name,load_assignment:.patch.value.load_assignment,transport_socket:.patch.value.transport_socket}}}]}' \
    >"$EVIDENCE/envoy-filter-on-failure.json"
  k get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json 2>/dev/null \
    | jq '[.items[-100:][]|{type,reason,message,involvedObject,lastTimestamp}]' >"$EVIDENCE/events-on-failure.json"
  if [[ -n "$GATEWAY_POD" ]]; then
    k exec "$GATEWAY_POD" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET '/clusters?format=json' 2>/dev/null \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); print(json.dumps([c for c in d.get("cluster_statuses",[]) if "payload-pre-processing-extproc" in c.get("name","") or "payload-processing-extproc" in c.get("name","")],indent=2))' \
      >"$EVIDENCE/payload-cluster-runtime-on-failure.json"
  fi
  set -e
}
on_error() {
  local code="$1" line="$2"
  trap - ERR
  capture_failure
  jq -n --arg run "$RUN" --arg reason "command failed with exit $code at line $line" --arg evidence "$EVIDENCE" \
    '{status:"FAILED",run:$run,reason:$reason,evidenceDirectory:$evidence,secretValuesRecorded:false,privateKeysRecorded:false}' \
    >"$EVIDENCE/RESULT.json"
  exit "$code"
}
trap cleanup EXIT
trap 'code=$?; on_error "$code" "$LINENO"' ERR
k() { command kubectl --kubeconfig="$KCFG" --context="$CONTEXT" --request-timeout=20s "$@"; }
GATEWAY_POD=""

server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$API" && "$(k get --raw=/readyz)" == *ok* ]] || fail 'API identity or /readyz gate failed'
ca_fingerprint="$(openssl x509 -in "$CA_FILE" -noout -fingerprint -sha256 | sed 's/^[^=]*=//;s/://g')"
k get clusterissuer rhai-ca-issuer -o json >"$WORK_DIR/issuer.json"
jq -e --arg secret "$ISSUER_CA_SECRET" '
  .metadata.generation as $g |
  .spec.ca.secretName==$secret and any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$g)
' "$WORK_DIR/issuer.json" >/dev/null || fail 'MaaS payload ClusterIssuer is not current-generation Ready from the recorded CA Secret'
k get secret "$ISSUER_CA_SECRET" -n cert-manager -o json >"$WORK_DIR/issuer-ca-secret.json"
jq -e --arg uid "$ISSUER_CA_SECRET_UID" ' .metadata.uid==$uid and (.data["tls.crt"]|type=="string" and length>0) ' \
  "$WORK_DIR/issuer-ca-secret.json" >/dev/null || fail 'MaaS issuer CA Secret UID or public certificate changed'
jq -r '.data["tls.crt"]' "$WORK_DIR/issuer-ca-secret.json" | base64 -d >"$WORK_DIR/issuer-ca.pem"
issuer_ca_fingerprint="$(openssl x509 -in "$WORK_DIR/issuer-ca.pem" -noout -fingerprint -sha256 | sed 's/^[^=]*=//;s/://g')"
[[ "$issuer_ca_fingerprint" == "$ca_fingerprint" ]] || fail 'active ClusterIssuer root certificate differs from the rendered Gateway trust CA'
k get secret payload-extproc-serving-ca -n "$NAMESPACE" -o json >"$WORK_DIR/trust-secret.json"
jq -e --arg uid "$CA_SECRET_UID" --arg run "$RUN" \
  '.metadata.uid==$uid and .metadata.labels["praxis.kind/run"]==$run and .type=="Opaque" and ((.data//{})|keys)==["ca.crt"]' \
  "$WORK_DIR/trust-secret.json" >/dev/null || fail 'Gateway trust Secret identity, ownership, or key contract changed'
jq -r '.data["ca.crt"]' "$WORK_DIR/trust-secret.json" | base64 -d >"$WORK_DIR/trust-ca.pem"
trust_fingerprint="$(openssl x509 -in "$WORK_DIR/trust-ca.pem" -noout -fingerprint -sha256 | sed 's/^[^=]*=//;s/://g')"
[[ "$trust_fingerprint" == "$ca_fingerprint" ]] || fail 'Gateway trust Secret CA does not match the active payload issuer'

deadline=$((SECONDS + TIMEOUT))
ready=false
: >"$EVIDENCE/payload-readiness-polls.tsv"
attempt=0
while (( SECONDS < deadline )); do
  attempt=$((attempt + 1)); ready=true
  for service in "${SERVICES[@]}"; do
    if ! k get deployment "$service" -n "$NAMESPACE" -o json >"$WORK_DIR/$service-deployment.json" 2>/dev/null || \
       ! k get service "$service" -n "$NAMESPACE" -o json >"$WORK_DIR/$service-service.json" 2>/dev/null; then
      ready=false; continue
    fi
    selector="$(jq -r '.spec.selector|to_entries|map("\(.key)=\(.value)")|join(",")' "$WORK_DIR/$service-service.json")"
    if [[ -z "$selector" ]] || ! k get pods -n "$NAMESPACE" -l "$selector" -o json >"$WORK_DIR/$service-pods.json" 2>/dev/null || \
       ! k get endpointslices -n "$NAMESPACE" -l "kubernetes.io/service-name=$service" -o json >"$WORK_DIR/$service-endpoints.json" 2>/dev/null; then
      ready=false; continue
    fi
    if ! jq -e '
      .metadata.generation as $g |
      (.spec.replicas // 1) as $r |
      ($r>0 and .status.observedGeneration==$g and .status.updatedReplicas==$r and .status.readyReplicas==$r and .status.availableReplicas==$r and (.status.unavailableReplicas//0)==0)
    ' "$WORK_DIR/$service-deployment.json" >/dev/null || \
       ! jq -e 'any(.items[]?;.ports|any(.[]?;.port==9004)) and any(.items[]?;.endpoints|any(.[]?;.conditions.ready==true))' "$WORK_DIR/$service-endpoints.json" >/dev/null; then
      ready=false
    fi
    desired="$(jq -r '.spec.replicas // 1' "$WORK_DIR/$service-deployment.json")"
    pod_ready="$(jq '[.items[]|select(.metadata.deletionTimestamp==null and .status.phase=="Running" and any(.status.conditions[]?;.type=="Ready" and .status=="True") and all(.status.containerStatuses[]?;.ready==true))]|length' "$WORK_DIR/$service-pods.json")"
    [[ "$pod_ready" == "$desired" ]] || ready=false
  done
  printf '%s\tready=%s\n' "$attempt" "$ready" >>"$EVIDENCE/payload-readiness-polls.tsv"
  [[ "$ready" == true ]] && break
  sleep 2
done
[[ "$ready" == true ]] || fail "both payload Deployments/Services/EndpointSlices did not converge within ${TIMEOUT}s"

for service in "${SERVICES[@]}"; do
  deployment_json="$WORK_DIR/$service-deployment.json"
  service_json="$WORK_DIR/$service-service.json"
  endpoints_json="$WORK_DIR/$service-endpoints.json"
  pods_json="$WORK_DIR/$service-pods.json"
  fqdn="$service.$NAMESPACE.svc.cluster.local"
  jq -e --arg secret "$service-tls" '
    any(.spec.template.spec.volumes[]?;.name=="tls" and .secret.secretName==$secret) and
    any(.spec.template.spec.containers[]?;any(.volumeMounts[]?;.name=="tls" and .mountPath=="/etc/tls/private"))
  ' "$deployment_json" >/dev/null || fail "Deployment/$service does not mount its matching payload TLS Secret"
  jq -e --arg secret "$service-tls" '
    .metadata.annotations["service.beta.openshift.io/serving-cert-secret-name"]==$secret and
    .metadata.labels["app.kubernetes.io/managed-by"]=="ai-gateway-controller"
  ' "$service_json" >/dev/null || fail "Service/$service does not declare the expected controller-owned serving certificate Secret"
  jq -e '.spec.ports|any(.[]?;.port==9004)' "$service_json" >/dev/null || fail "Service/$service does not expose port 9004"
  jq -e --arg service "$service" '
    any(.items[]?;.ports|any(.[]?;.port==9004)) and
    any(.items[]?;.endpoints[]?|.conditions.ready==true and .targetRef.kind=="Pod")
  ' "$endpoints_json" >/dev/null || fail "Service/$service has no ready EndpointSlice Pod on port 9004"
  k get certificate "$service-tls" -n "$NAMESPACE" -o json >"$WORK_DIR/$service-certificate.json"
  k get secret "$service-tls" -n "$NAMESPACE" -o json >"$WORK_DIR/$service-tls-secret.json"
  generation="$(jq -er '.metadata.generation' "$WORK_DIR/$service-certificate.json")"
  jq -e --arg run "$RUN" --arg fqdn "$fqdn" --argjson generation "$generation" \
    '.metadata.labels["praxis.kind/run"]==$run and .spec.secretName==.metadata.name and .spec.dnsNames==[$fqdn] and
     .spec.issuerRef.name=="rhai-ca-issuer" and .spec.issuerRef.kind=="ClusterIssuer" and
     any(.status.conditions[]?;.type=="Ready" and .status=="True" and .observedGeneration==$generation)' \
    "$WORK_DIR/$service-certificate.json" >/dev/null || fail "Certificate/$service-tls is not current-generation Ready with the exact Service DNS SAN"
  jq -e --arg run "$RUN" '.metadata.labels["praxis.kind/run"]==$run and .type=="kubernetes.io/tls" and (.data["tls.crt"]|length>0) and (.data["tls.key"]|length>0)' \
    "$WORK_DIR/$service-tls-secret.json" >/dev/null || fail "Secret/$service-tls is absent or does not contain TLS key material"
  jq -r '.data["tls.crt"]' "$WORK_DIR/$service-tls-secret.json" | base64 -d >"$WORK_DIR/$service.crt"
  jq -r '.data["tls.key"]' "$WORK_DIR/$service-tls-secret.json" | base64 -d >"$WORK_DIR/$service.key"
  openssl verify -CAfile "$CA_FILE" -purpose sslserver -verify_hostname "$fqdn" "$WORK_DIR/$service.crt" >"$EVIDENCE/$service-certificate-verify.txt"
  cert_pub="$(openssl x509 -in "$WORK_DIR/$service.crt" -pubkey -noout | openssl pkey -pubin -outform DER | sha256sum | awk '{print $1}')"
  key_pub="$(openssl pkey -in "$WORK_DIR/$service.key" -pubout -outform DER | sha256sum | awk '{print $1}')"
  [[ "$cert_pub" == "$key_pub" ]] || fail "Secret/$service-tls key does not match its leaf certificate"

  local_port="$(python3 - <<'PYPORT'
import socket
s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()
PYPORT
)"
  k port-forward --address 127.0.0.1 "service/$service" "$local_port:9004" -n "$NAMESPACE" \
    >"$EVIDENCE/$service-port-forward.log" 2>&1 &
  PORT_FORWARD_PID=$!
  forward_ready=false
  forward_deadline=$((SECONDS + 20))
  while (( SECONDS < forward_deadline )); do
    if ! kill -0 "$PORT_FORWARD_PID" 2>/dev/null; then break; fi
    if grep -q "Forwarding from 127.0.0.1:$local_port -> 9004" "$EVIDENCE/$service-port-forward.log"; then forward_ready=true; break; fi
    sleep 0.2
  done
  [[ "$forward_ready" == true ]] || fail "kubectl port-forward could not attach to Service/$service"
  timeout 10s openssl s_client -connect "127.0.0.1:$local_port" -servername "$fqdn" -verify_hostname "$fqdn" \
    -verify_return_error -CAfile "$CA_FILE" -brief </dev/null >"$EVIDENCE/$service-service-tls-handshake.txt" 2>&1 \
    || fail "Service/$service TLS handshake failed DNS/SNI/issuer verification"
  kill "$PORT_FORWARD_PID" 2>/dev/null || true
  wait "$PORT_FORWARD_PID" 2>/dev/null || true
  PORT_FORWARD_PID=""
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},spec:{replicas:.spec.replicas,selector:.spec.selector,templateLabels:.spec.template.metadata.labels,volumes:[.spec.template.spec.volumes[]?|select(.name=="tls")|{name,secret:.secret.secretName}],containers:[.spec.template.spec.containers[]?|{name,image,imagePullPolicy,ports,volumeMounts,resources}]},status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,updatedReplicas:.status.updatedReplicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas,conditions:.status.conditions}}' \
    "$deployment_json" >"$EVIDENCE/$service-deployment-summary.json"
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid},spec:{clusterIP:.spec.clusterIP,selector:.spec.selector,ports:.spec.ports}}' "$service_json" >"$EVIDENCE/$service-service-summary.json"
  jq '[.items[]|{metadata:{name:.metadata.name,labels:.metadata.labels},ports:.ports,endpoints:[.endpoints[]?|{addresses,conditions,targetRef}]}]' \
    "$endpoints_json" >"$EVIDENCE/$service-endpoints-summary.json"
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{dnsNames:.spec.dnsNames,secretName:.spec.secretName,issuerRef:.spec.issuerRef},status:{conditions:.status.conditions}}' \
    "$WORK_DIR/$service-certificate.json" >"$EVIDENCE/$service-certificate-summary.json"
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels},type:.type,dataKeys:((.data//{})|keys),privateKeyRecorded:false}' \
    "$WORK_DIR/$service-tls-secret.json" >"$EVIDENCE/$service-secret-summary.json"
  jq '[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid,deletionTimestamp:.metadata.deletionTimestamp},status:{phase:.status.phase,conditions:.status.conditions,containerStatuses:[.status.containerStatuses[]?|{name,ready,restartCount,image,imageID}]}}]' \
    "$pods_json" >"$EVIDENCE/$service-pods-summary.json"
done

k get destinationrules.networking.istio.io "${SERVICES[@]}" -n "$NAMESPACE" -o json >"$EVIDENCE/destination-rules-live.json"
k get envoyfilter.networking.istio.io "$ENVOY_FILTER" -n "$NAMESPACE" -o json >"$EVIDENCE/envoy-filter-live.json"
k get pods -n "$NAMESPACE" -l "gateway.networking.k8s.io/gateway-name=$GATEWAY_NAME" -o json >"$WORK_DIR/gateway-pods.json"
mapfile -t gateway_pods < <(jq -r '[.items[]|select(.status.phase=="Running" and .metadata.deletionTimestamp==null and any(.status.conditions[]?;.type=="Ready" and .status=="True") and any(.status.containerStatuses[]?;.name=="istio-proxy" and .ready==true))|.metadata.name]|sort[]' "$WORK_DIR/gateway-pods.json")
(( ${#gateway_pods[@]} > 0 )) || fail 'MaaS Gateway has no Ready istio-proxy Pod'
jq '[.items[]|select(.status.phase=="Running" and .metadata.deletionTimestamp==null)|{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels},status:{conditions:.status.conditions,containerStatuses:[.status.containerStatuses[]?|{name,ready,image,imageID}]}}]' \
  "$WORK_DIR/gateway-pods.json" >"$EVIDENCE/gateway-pods-summary.json"

for gateway_pod in "${gateway_pods[@]}"; do
  GATEWAY_POD="$gateway_pod"
  xds_file="$WORK_DIR/$gateway_pod-config-dump.json"
  runtime_file="$WORK_DIR/$gateway_pod-clusters.json"
  k exec "$gateway_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET config_dump >"$xds_file"
  chmod 0600 "$xds_file"
  k exec "$gateway_pod" -n "$NAMESPACE" -c istio-proxy -- pilot-agent request GET '/clusters?format=json' >"$runtime_file"
  chmod 0600 "$runtime_file"
  python3 - "$runtime_file" "$EVIDENCE/$gateway_pod-runtime-dns-summary.json" "$WORK_DIR" "$NAMESPACE" "$SERVICE_PRE" "$SERVICE_PROCESSING" "$POOL" <<'PYRUNTIME'
import ipaddress, json, sys
from pathlib import Path
runtime_path, output_path, work_dir, namespace, service_pre, service_processing, pool = sys.argv[1:]
runtime=json.loads(Path(runtime_path).read_text())
summary=[]
for service in (service_pre, service_processing):
    service_doc=json.loads((Path(work_dir)/f"{service}-service.json").read_text())
    endpoints=json.loads((Path(work_dir)/f"{service}-endpoints.json").read_text())
    processor=service.removesuffix(f"-{pool}") if pool else service
    cluster_name=f"{processor}-extproc"
    matches=[x for x in runtime.get("cluster_statuses",[]) if x.get("name")==cluster_name]
    if len(matches)!=1:
        raise SystemExit(f"{service}: expected exactly one active Envoy service cluster")
    hosts=matches[0].get("host_statuses",[])
    if not hosts:
        raise SystemExit(f"{service}: Envoy has no resolved service DNS host")
    addresses=[]
    for host in hosts:
        socket=host.get("address",{}).get("socket_address",{})
        address=socket.get("address")
        port=socket.get("port_value",socket.get("portValue"))
        if not address or port!=9004:
            raise SystemExit(f"{service}: Envoy resolved an unexpected service address or port")
        try:
            ipaddress.ip_address(address)
        except ValueError:
            # If Envoy retains the DNS name, it still needs to match the configured service FQDN.
            if address != f"{service}.{namespace}.svc.cluster.local":
                raise SystemExit(f"{service}: Envoy resolved an unexpected DNS name")
        addresses.append(address)
    cluster_ip=service_doc["spec"].get("clusterIP")
    if cluster_ip and cluster_ip!="None" and cluster_ip not in addresses:
        raise SystemExit(f"{service}: active Envoy cluster does not resolve to the Kubernetes Service IP")
    summary.append({"service":service,"cluster":cluster_name,"resolvedAddresses":addresses,"serviceClusterIP":cluster_ip})
Path(output_path).write_text(json.dumps({"status":"PASS","services":summary},indent=2,sort_keys=True)+"\n")
PYRUNTIME
  checker_args=(
    --destination-rules "$EVIDENCE/destination-rules-live.json" --envoy-filter "$EVIDENCE/envoy-filter-live.json"
    --xds-dump "$xds_file" --ca-file "$CA_FILE" --namespace "$NAMESPACE"
    --output "$EVIDENCE/$gateway_pod-xds-tls-summary.json"
  )
  [[ -z "$POOL" ]] || checker_args+=(--pool "$POOL")
  python3 "$SCRIPT_DIR/verify_kind_payload_tls_config.py" "${checker_args[@]}" \
    >"$EVIDENCE/$gateway_pod-xds-check.log" 2>&1 || fail "active Gateway $gateway_pod xDS does not satisfy the DNS/SNI/SAN/issuer trust contract"
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},spec:{host:.spec.host,workloadSelector:.spec.workloadSelector,trafficPolicy:{tls:.spec.trafficPolicy.tls}}}' \
    "$EVIDENCE/destination-rules-live.json" >"$EVIDENCE/destination-rules-summary.json"
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation},workloadSelector:.spec.workloadSelector,clusters:[.spec.configPatches[]?|select(.applyTo=="CLUSTER")|{name:.patch.value.name,address:.patch.value.load_assignment.endpoints[0].lb_endpoints[0].endpoint.address.socket_address.address,sni:.patch.value.transport_socket.typed_config.sni,validation:.patch.value.transport_socket.typed_config.common_tls_context.validation_context|{trusted_ca_type:(keys|map(select(.=="trusted_ca"))),dns_san:.match_typed_subject_alt_names}}]}' \
    "$EVIDENCE/envoy-filter-live.json" >"$EVIDENCE/envoy-filter-summary.json"
done

jq -n --arg run "$RUN" --arg namespace "$NAMESPACE" --arg gateway "$GATEWAY_NAME" --arg caFingerprint "$ca_fingerprint" \
  --argjson services "$(printf '%s\n' "${SERVICES[@]}" | jq -R . | jq -s .)" \
  --argjson gatewayPods "$(jq -c '[.items[]|select(.status.phase=="Running" and any(.status.conditions[]?;.type=="Ready" and .status=="True") and any(.status.containerStatuses[]?;.name=="istio-proxy" and .ready==true))|{name:.metadata.name,uid:.metadata.uid}]' "$WORK_DIR/gateway-pods.json")" \
  '{status:"PASS",run:$run,namespace:$namespace,gateway:$gateway,gatewayPods:$gatewayPods,services:$services,
    certificatesCurrentGenerationReady:true,secretsAvailable:true,deploymentsCurrentGenerationReady:true,serviceEndpointsReady:true,
    serviceTlsDnsSniAndIssuerVerified:true,gatewayServiceDnsResolutionVerified:true,gatewayEnvoyFilterAndXdsTlsVerified:true,
    issuerCaSha256Fingerprint:$caFingerprint,secretValuesRecorded:false,privateKeysRecorded:false}' >"$EVIDENCE/RESULT.json"
printf 'both payload Services, certificates, Deployments, endpoint slices, and live Gateway TLS/DNS configuration passed\n'
