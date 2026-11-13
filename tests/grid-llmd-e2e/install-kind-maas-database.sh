#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

CONTEXT=""
KUBECONFIG_FILE=""
EXPECTED_API_SERVER=""
NAMESPACE="maas-system"
RUN_ID=""
IMAGE=""
STATE_DIR=""
EVIDENCE_DIR=""

usage() {
  cat <<'USAGE'
Usage: install-kind-maas-database.sh --context CTX --kubeconfig PRIVATE-FILE \
  --expected-api-server URL --run-id ID --image REPO@sha256:DIGEST \
  --state-dir NEW-PRIVATE-DIR --evidence-dir NEW-DIR [--namespace NS]

Installs the run-owned ephemeral PostgreSQL prerequisite for MaaS on Kind.
Credentials are kept in private state, the database uses emptyDir, and the
helper verifies readiness and the pinned runtime image identity. It refuses
pre-existing objects instead of adopting them. Secret values are excluded
from evidence and Kubernetes apply annotations.
USAGE
}

while (($#)); do
  case "$1" in
    --context) CONTEXT="$2"; shift 2 ;;
    --kubeconfig) KUBECONFIG_FILE="$2"; shift 2 ;;
    --expected-api-server) EXPECTED_API_SERVER="$2"; shift 2 ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --run-id) RUN_ID="$2"; shift 2 ;;
    --image) IMAGE="$2"; shift 2 ;;
    --state-dir) STATE_DIR="$2"; shift 2 ;;
    --evidence-dir) EVIDENCE_DIR="$2"; shift 2 ;;
    --help|-h) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() {
  if declare -F capture_failure >/dev/null; then capture_failure "$*"; fi
  printf 'install-kind-maas-database: %s\n' "$*" >&2
  exit 2
}
[[ -n "$CONTEXT" && -f "$KUBECONFIG_FILE" && -n "$EXPECTED_API_SERVER" ]] || fail 'explicit context, private kubeconfig, and expected API server are required'
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || fail 'run ID is not a valid ownership label'
[[ "$NAMESPACE" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || fail 'namespace is invalid'
[[ "$IMAGE" =~ ^[A-Za-z0-9._:/-]+@sha256:[0-9a-f]{64}$ ]] || fail 'PostgreSQL image must use an immutable SHA-256 digest'
[[ -n "$STATE_DIR" && -n "$EVIDENCE_DIR" ]] || fail 'private state and evidence paths are required'
[[ "$STATE_DIR" != "$EVIDENCE_DIR" && "$STATE_DIR" != "$EVIDENCE_DIR"/* && "$KUBECONFIG_FILE" != "$EVIDENCE_DIR"/* ]] || fail 'private database state and kubeconfig must stay outside evidence'
[[ ! -e "$STATE_DIR" && ! -e "$EVIDENCE_DIR" ]] || fail 'state and evidence directories must be new'
for binary in kubectl jq openssl; do command -v "$binary" >/dev/null || fail "$binary is required"; done
[[ "$(stat -c '%a' "$KUBECONFIG_FILE")" == 600 || "$(stat -c '%a' "$KUBECONFIG_FILE")" == 400 ]] || fail 'kubeconfig must have mode 0600 or 0400'

mkdir -p "$(dirname "$STATE_DIR")" "$(dirname "$EVIDENCE_DIR")"
mkdir -m 0700 "$STATE_DIR" "$EVIDENCE_DIR"
chmod 0700 "$STATE_DIR" "$EVIDENCE_DIR"
k() { command kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT" "$@"; }
capture_failure() {
  local reason="$1" pod
  [[ -d "$EVIDENCE_DIR" ]] || return 0
  trap - ERR
  set +e
  if k get deployment postgres -n "$NAMESPACE" -o json 2>/dev/null \
    | jq '{metadata:{name:.metadata.name,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,images:[.spec.template.spec.containers[].image],requests:[.spec.template.spec.containers[].resources.requests]},status:.status}' \
      >"$EVIDENCE_DIR/failure-deployment.json"; then :; fi
  if k get pods -n "$NAMESPACE" -l "app=postgres,praxis.kind/run=$RUN_ID" -o json 2>/dev/null \
    | jq '[.items[]|{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,deletionTimestamp:.metadata.deletionTimestamp},spec:{nodeName:.spec.nodeName,containers:[.spec.containers[]|{name,image,resources,envFrom,volumeMounts}]},status:{phase:.status.phase,conditions:.status.conditions,containerStatuses:[.status.containerStatuses[]?|{name,ready,restartCount,state,lastState,image,imageID}]}}]' \
      >"$EVIDENCE_DIR/failure-pods.json"; then :; fi
  if [[ -s "$EVIDENCE_DIR/failure-pods.json" ]]; then
    while IFS= read -r pod; do
      [[ -n "$pod" ]] || continue
      k logs "$pod" -n "$NAMESPACE" -c postgres --previous >"$EVIDENCE_DIR/failure-postgres-previous.log" 2>&1
      k logs "$pod" -n "$NAMESPACE" -c postgres >"$EVIDENCE_DIR/failure-postgres-current.log" 2>&1
      k describe pod "$pod" -n "$NAMESPACE" >"$EVIDENCE_DIR/failure-pod-describe.txt" 2>&1
    done < <(jq -r '.[].metadata.name' "$EVIDENCE_DIR/failure-pods.json")
  fi
  k get events -n "$NAMESPACE" --sort-by=.lastTimestamp -o json 2>/dev/null \
    | jq '[.items[]|{type,reason,message,involvedObject,firstTimestamp,lastTimestamp,count}]' \
    >"$EVIDENCE_DIR/failure-events.json"
  jq -n --arg reason "$reason" --arg run "$RUN_ID" --arg namespace "$NAMESPACE" \
    '{status:"FAILED",reason:$reason,run:$run,namespace:$namespace,secretValuesRecorded:false}' \
    >"$EVIDENCE_DIR/RESULT.json"
  set -e
}
on_error() {
  local status="$1" line="$2"
  capture_failure "command failed with exit $status at helper line $line"
  exit "$status"
}
trap 'status=$?; on_error "$status" "$LINENO"' ERR
server="$(k config view --minify -o json | jq -er '.clusters[0].cluster.server')"
[[ "$server" == "$EXPECTED_API_SERVER" ]] || fail 'selected context API server differs from expected API server'
[[ "$(k get --raw=/readyz)" == *ok* ]] || fail 'Kubernetes API is not ready'
k get namespace "$NAMESPACE" -o json |
  jq -e --arg run "$RUN_ID" '.metadata.labels["praxis.kind/run"]==$run' >/dev/null || fail 'MaaS namespace is not owned by this run'
namespace_uid="$(k get namespace "$NAMESPACE" -o json | jq -er '.metadata.uid')"

for resource in deployment/postgres service/postgres secret/postgres-creds secret/maas-db-config; do
  if k get "$resource" -n "$NAMESPACE" -o name >"$STATE_DIR/preexisting-resource.txt" 2>"$STATE_DIR/preexisting-error.txt"; then
    fail "refusing to adopt pre-existing $resource"
  fi
  grep -qi 'not found' "$STATE_DIR/preexisting-error.txt" || fail "could not verify absence of $resource"
done

password_file="$STATE_DIR/postgres-password"
env_file="$STATE_DIR/postgres.env"
url_file="$STATE_DIR/db-connection-url"
openssl rand -hex 32 >"$password_file"
password="$(cat "$password_file")"
printf 'POSTGRES_USER=maas\nPOSTGRES_PASSWORD=%s\nPOSTGRES_DB=maas\n' "$password" >"$env_file"
printf 'postgresql://maas:%s@postgres.%s.svc:5432/maas?sslmode=disable\n' "$password" "$NAMESPACE" >"$url_file"
unset password
chmod 0600 "$password_file" "$env_file" "$url_file"

cat >"$EVIDENCE_DIR/postgres-resources.yaml" <<YAML
apiVersion: v1
kind: Service
metadata:
  name: postgres
  namespace: $NAMESPACE
  labels:
    app: postgres
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.kind/run: "$RUN_ID"
spec:
  type: ClusterIP
  selector:
    app: postgres
    praxis.kind/run: "$RUN_ID"
  ports:
  - name: postgres
    port: 5432
    targetPort: 5432
    protocol: TCP
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: postgres
  namespace: $NAMESPACE
  labels:
    app: postgres
    app.kubernetes.io/managed-by: grid-llmd-kind
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.kind/run: "$RUN_ID"
spec:
  replicas: 1
  selector:
    matchLabels:
      app: postgres
      praxis.kind/run: "$RUN_ID"
  template:
    metadata:
      labels:
        app: postgres
        app.kubernetes.io/managed-by: grid-llmd-kind
        app.kubernetes.io/part-of: grid-llmd-e2e
        praxis.kind/run: "$RUN_ID"
    spec:
      automountServiceAccountToken: false
      containers:
      - name: postgres
        image: "$IMAGE"
        imagePullPolicy: IfNotPresent
        envFrom:
        - secretRef:
            name: postgres-creds
        ports:
        - name: postgres
          containerPort: 5432
          protocol: TCP
        resources:
          requests:
            cpu: 100m
            memory: 256Mi
          limits:
            cpu: 500m
            memory: 512Mi
        readinessProbe:
          exec:
            command: ["pg_isready", "-U", "maas", "-d", "maas"]
          initialDelaySeconds: 3
          periodSeconds: 3
          timeoutSeconds: 2
          failureThreshold: 20
        livenessProbe:
          exec:
            command: ["pg_isready", "-U", "maas", "-d", "maas"]
          initialDelaySeconds: 15
          periodSeconds: 10
          timeoutSeconds: 2
          failureThreshold: 6
        volumeMounts:
        - name: data
          mountPath: /var/lib/postgresql/data
      volumes:
      - name: data
        emptyDir:
          sizeLimit: 2Gi
YAML
chmod 0600 "$EVIDENCE_DIR/postgres-resources.yaml"
sha256sum "$EVIDENCE_DIR/postgres-resources.yaml" >"$EVIDENCE_DIR/postgres-resources.sha256"

k apply --dry-run=server -f "$EVIDENCE_DIR/postgres-resources.yaml" >"$EVIDENCE_DIR/workload-server-dry-run.txt"
k create secret generic postgres-creds -n "$NAMESPACE" --from-env-file="$env_file" --dry-run=client -o yaml \
  | k label --local -f - app.kubernetes.io/managed-by=grid-llmd-kind app.kubernetes.io/part-of=grid-llmd-e2e "praxis.kind/run=$RUN_ID" --overwrite --dry-run=client -o yaml \
  | k create --dry-run=server -f - >/dev/null
k create secret generic maas-db-config -n "$NAMESPACE" --from-file="DB_CONNECTION_URL=$url_file" --dry-run=client -o yaml \
  | k label --local -f - app.kubernetes.io/managed-by=grid-llmd-kind app.kubernetes.io/part-of=grid-llmd-e2e "praxis.kind/run=$RUN_ID" --overwrite --dry-run=client -o yaml \
  | k create --dry-run=server -f - >/dev/null

k create secret generic postgres-creds -n "$NAMESPACE" --from-env-file="$env_file" --dry-run=client -o yaml \
  | k label --local -f - app.kubernetes.io/managed-by=grid-llmd-kind app.kubernetes.io/part-of=grid-llmd-e2e "praxis.kind/run=$RUN_ID" --overwrite --dry-run=client -o yaml \
  | k create -f - >"$EVIDENCE_DIR/postgres-secret-create.txt"
k create secret generic maas-db-config -n "$NAMESPACE" --from-file="DB_CONNECTION_URL=$url_file" --dry-run=client -o yaml \
  | k label --local -f - app.kubernetes.io/managed-by=grid-llmd-kind app.kubernetes.io/part-of=grid-llmd-e2e "praxis.kind/run=$RUN_ID" --overwrite --dry-run=client -o yaml \
  | k create -f - >"$EVIDENCE_DIR/maas-db-secret-create.txt"
k create -f "$EVIDENCE_DIR/postgres-resources.yaml" >"$EVIDENCE_DIR/workload-create.txt"
k rollout status deployment/postgres -n "$NAMESPACE" --timeout=240s >"$EVIDENCE_DIR/rollout.txt"

k get deployment postgres -n "$NAMESPACE" -o json |
  jq '{metadata:{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,generation:.metadata.generation,labels:.metadata.labels},spec:{replicas:.spec.replicas,images:[.spec.template.spec.containers[].image],requests:[.spec.template.spec.containers[].resources.requests]},status:{observedGeneration:.status.observedGeneration,replicas:.status.replicas,readyReplicas:.status.readyReplicas,availableReplicas:.status.availableReplicas}}' >"$EVIDENCE_DIR/deployment-summary.json"
jq -e --arg image "$IMAGE" '.status.observedGeneration==.metadata.generation and .status.readyReplicas==.spec.replicas and .status.availableReplicas==.spec.replicas and .spec.images==[$image]' \
  "$EVIDENCE_DIR/deployment-summary.json" >/dev/null || fail 'PostgreSQL Deployment is not current, Ready, and pinned'

pod="$(k get pods -n "$NAMESPACE" -l "app=postgres,praxis.kind/run=$RUN_ID" -o json | jq -er '[.items[]|select(.metadata.deletionTimestamp==null and .status.phase=="Running" and any(.status.conditions[]?;.type=="Ready" and .status=="True"))]|if length==1 then .[0].metadata.name else error("expected one Ready PostgreSQL Pod") end')"
k get pod "$pod" -n "$NAMESPACE" -o json |
  jq '{metadata:{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels},containerStatuses:[.status.containerStatuses[]?|{name,image,imageID,ready,restartCount}]}' >"$EVIDENCE_DIR/pod-runtime-identity.json"
jq -e --arg digest "${IMAGE##*@}" '[.containerStatuses[]|select(.name=="postgres" and .ready==true and (.imageID|endswith("@"+$digest)))]|length==1' \
  "$EVIDENCE_DIR/pod-runtime-identity.json" >/dev/null || fail 'PostgreSQL runtime imageID does not match the immutable image digest'
k exec -n "$NAMESPACE" "$pod" -- pg_isready -U maas -d maas >"$EVIDENCE_DIR/pg-isready.txt"
k get endpointslices -n "$NAMESPACE" -l kubernetes.io/service-name=postgres -o json |
  jq '{items:[.items[]|{name:.metadata.name,ports:.ports,readyEndpoints:[.endpoints[]?|select(.conditions.ready==true)|{addresses,targetRef:{kind:.targetRef.kind,name:.targetRef.name,uid:.targetRef.uid}}]}]}' >"$EVIDENCE_DIR/endpointslices.json"
jq -e '[.items[].readyEndpoints[]]|length==1' "$EVIDENCE_DIR/endpointslices.json" >/dev/null || fail 'PostgreSQL Service does not have exactly one Ready endpoint'

for secret in postgres-creds maas-db-config; do
  k get secret "$secret" -n "$NAMESPACE" -o json |
    jq --arg name "$secret" '{name:.metadata.name,namespace:.metadata.namespace,uid:.metadata.uid,labels:.metadata.labels,dataKeys:(.data|keys)}' \
    >>"$EVIDENCE_DIR/secret-identities.jsonl"
done
jq -s . "$EVIDENCE_DIR/secret-identities.jsonl" >"$EVIDENCE_DIR/secret-identities.json"
jq -e 'length==2 and (.[0].name=="postgres-creds") and (.[1].name=="maas-db-config")' \
  "$EVIDENCE_DIR/secret-identities.json" >/dev/null || fail 'expected both database Secret identities'
jq -e --arg run "$RUN_ID" 'length==2 and all(.[];.labels["praxis.kind/run"]==$run and .labels["app.kubernetes.io/managed-by"]=="grid-llmd-kind")' \
  "$EVIDENCE_DIR/secret-identities.json" >/dev/null || fail 'database Secrets do not carry the run ownership labels'
jq -n --arg run "$RUN_ID" --arg namespace "$NAMESPACE" --arg namespaceUid "$namespace_uid" --arg image "$IMAGE" \
  --arg pod "$pod" --arg podUid "$(jq -r '.metadata.uid' "$EVIDENCE_DIR/pod-runtime-identity.json")" \
  --arg imageID "$(jq -r '.containerStatuses[0].imageID' "$EVIDENCE_DIR/pod-runtime-identity.json")" \
  '{status:"PASS",run:$run,namespace:$namespace,namespaceUid:$namespaceUid,deployment:"postgres",pod:$pod,podUid:$podUid,image:$image,imageID:$imageID,pgIsReady:true,oneReadyServiceEndpoint:true,emptyDir:true,secretValuesRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'run-owned ephemeral PostgreSQL passed readiness and immutable runtime identity gates; Secret values were not recorded\n'
