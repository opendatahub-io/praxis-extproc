#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Create an isolated one-node Kind cluster and run-owned local registry.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUN_ID="${E2E_RUN_ID:-}"
CLUSTER_NAME="${KIND_CLUSTER_NAME:-}"
REGISTRY_NAME="${KIND_REGISTRY_CONTAINER:-}"
NETWORK_NAME="${KIND_REGISTRY_NETWORK:-}"
KUBECONFIG_PATH="${KUBECONFIG:-}"
STATE_DIR="${KIND_STATE_DIR:-}"
EVIDENCE_DIR="${KIND_ENV_EVIDENCE_DIR:-}"
EVIDENCE="$EVIDENCE_DIR"
REGISTRY_IMAGE="${KIND_REGISTRY_IMAGE:-registry:2.8.3}"
NODE_IMAGE="${KIND_NODE_IMAGE:-kindest/node:v1.34.3}"
REGISTRY_NETWORK_SUBNET="${KIND_REGISTRY_NETWORK_SUBNET:-10.250.0.0/24}"
REGISTRY_NETWORK_GATEWAY="${KIND_REGISTRY_NETWORK_GATEWAY:-10.250.0.1}"
CAPACITY_PROFILE="${KIND_CAPACITY_PROFILE:-full}"
RUN_LABEL_KEY="praxis.fast/qualification-run"
EVIDENCE_CREATED=false

fail() {
  if [[ "$EVIDENCE_CREATED" == true ]]; then printf 'failure=%s\n' "$*" >"$EVIDENCE_DIR/blocked.txt"; fi
  printf 'Kind environment creation blocked: %s\n' "$*" >&2
  exit 2
}
for name in RUN_ID CLUSTER_NAME REGISTRY_NAME NETWORK_NAME KUBECONFIG_PATH STATE_DIR EVIDENCE_DIR; do
  [[ -n "${!name}" ]] || fail "$name is required"
done
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || fail 'E2E_RUN_ID is not a safe Kubernetes label value'
[[ "$CLUSTER_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ ]] || fail 'KIND_CLUSTER_NAME is not a safe DNS label'
[[ "$REGISTRY_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ ]] || fail 'KIND_REGISTRY_CONTAINER is not a safe Docker name'
[[ "$NETWORK_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ ]] || fail 'KIND_REGISTRY_NETWORK is not a safe Docker name'
[[ "$KUBECONFIG_PATH" != "$EVIDENCE_DIR"/* ]] || fail 'private kubeconfig must be outside evidence'
[[ "$KUBECONFIG_PATH" == "$STATE_DIR"/* ]] || fail 'private kubeconfig must be inside the run state directory'
[[ "$STATE_DIR" != "$EVIDENCE_DIR" && "$STATE_DIR" != "$EVIDENCE_DIR"/* ]] || fail 'private run state must be outside evidence'
[[ ! -e "$STATE_DIR" ]] || fail 'run state directory already exists; refusing to adopt it'
[[ ! -e "$KUBECONFIG_PATH" ]] || fail 'refusing to overwrite an existing kubeconfig'
[[ ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory already exists'
for binary in docker kind kubectl jq curl timeout python3 rg grep seq df free nproc ip; do
  command -v "$binary" >/dev/null 2>&1 || fail "$binary is required"
done
if kind get clusters 2>/dev/null | rg -qx "$CLUSTER_NAME"; then
  fail "Kind cluster name already exists; refusing to adopt it: $CLUSTER_NAME"
fi
if docker container inspect "$REGISTRY_NAME" >/dev/null 2>&1; then
  fail "registry container name already exists; refusing to adopt it: $REGISTRY_NAME"
fi
if docker network inspect "$NETWORK_NAME" >/dev/null 2>&1; then
  fail "registry network name already exists; refusing to adopt it: $NETWORK_NAME"
fi

mkdir -p "$EVIDENCE_DIR" "$STATE_DIR" "$(dirname "$KUBECONFIG_PATH")"
chmod 0700 "$EVIDENCE_DIR" "$STATE_DIR" "$(dirname "$KUBECONFIG_PATH")"
EVIDENCE_CREATED=true
on_error() {
  local status=$?
  printf 'exit_status=%s line=%s command=%s\n' "$status" "$LINENO" "$BASH_COMMAND" >"$EVIDENCE_DIR/unexpected-exit.txt"
  printf 'Run-owned resources, if created, were left in place for diagnosis; use destroy-kind-environment.sh after app cleanup.\n' >&2
  exit "$status"
}
trap on_error ERR

df -B1 --output=avail / | tail -n 1 | tr -d ' ' >"$EVIDENCE/host-disk-available-bytes.txt"
free -b >"$EVIDENCE/host-memory.txt"
nproc >"$EVIDENCE/host-cpu-count.txt"
docker info --format '{{json .}}' | jq '{serverVersion:.ServerVersion,operatingSystem:.OperatingSystem,architecture:.Architecture,ncpu:.NCPU,memory:.MemTotal,dockerRootDir:.DockerRootDir}' \
  >"$EVIDENCE/container-engine-capacity.json"
case "$CAPACITY_PROFILE" in
  full)
    MIN_FREE_DISK_GIB="${KIND_MIN_FREE_DISK_GIB:-6}"
    MIN_AVAILABLE_MEMORY_GIB="${KIND_MIN_AVAILABLE_MEMORY_GIB:-8}"
    REQUIRED_CPU_COUNT="${KIND_MIN_CPU_COUNT:-8}"
    EXACT_CPU_COUNT=""
    ;;
  gha-standard-4cpu)
    # This is a separate experiment profile. The GitHub-hosted runner VM is the
    # whole-run cgroup; its visible CPUs, Docker daemon CPUs, and memory must agree.
    MIN_FREE_DISK_GIB="${KIND_MIN_FREE_DISK_GIB:-8}"
    MIN_AVAILABLE_MEMORY_GIB="${KIND_MIN_AVAILABLE_MEMORY_GIB:-10}"
    REQUIRED_CPU_COUNT=4
    EXACT_CPU_COUNT=4
    ;;
  *) fail 'KIND_CAPACITY_PROFILE must be full or gha-standard-4cpu' ;;
esac
docker_cpu_count="$(jq -er '.ncpu' "$EVIDENCE/container-engine-capacity.json")"
python3 - "$EVIDENCE/host-disk-available-bytes.txt" "$EVIDENCE/host-memory.txt" "$EVIDENCE/host-cpu-count.txt" \
  "$MIN_FREE_DISK_GIB" "$MIN_AVAILABLE_MEMORY_GIB" "$REQUIRED_CPU_COUNT" "$EXACT_CPU_COUNT" "$docker_cpu_count" <<'PY'
import re, sys
from pathlib import Path
disk_bytes = int(Path(sys.argv[1]).read_text().strip())
memory = Path(sys.argv[2]).read_text()
match = re.search(r"^Mem:\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s+(\d+)\s*$", memory, re.MULTILINE)
if not match:
    raise SystemExit("could not read host memory availability")
available_bytes = int(match.group(6))
cpu_count = int(Path(sys.argv[3]).read_text().strip())
minimum_disk = float(sys.argv[4]) * 1024**3
minimum_memory = float(sys.argv[5]) * 1024**3
minimum_cpus = int(sys.argv[6])
exact_cpus = int(sys.argv[7]) if sys.argv[7] else None
docker_cpus = int(sys.argv[8])
if disk_bytes < minimum_disk:
    raise SystemExit(f"insufficient disk headroom: {disk_bytes} bytes available")
if available_bytes < minimum_memory:
    raise SystemExit(f"insufficient memory headroom: {available_bytes} bytes available")
if exact_cpus is not None and cpu_count != exact_cpus:
    raise SystemExit(f"{exact_cpus}-CPU profile requires exactly {exact_cpus} visible CPUs; found {cpu_count}")
if cpu_count < minimum_cpus:
    raise SystemExit(f"capacity profile requires at least {minimum_cpus} visible CPUs; found {cpu_count}")
if exact_cpus is not None and docker_cpus != exact_cpus:
    raise SystemExit(f"{exact_cpus}-CPU profile requires the container engine to report the same {exact_cpus} CPUs; found {docker_cpus}")
PY

printf '%s\n' "$RUN_ID" >"$EVIDENCE_DIR/run-id.txt"
printf '%s\n' "$CAPACITY_PROFILE" >"$EVIDENCE_DIR/capacity-profile.txt"
printf '%s\n' "$CLUSTER_NAME" >"$EVIDENCE_DIR/cluster-name.txt"
printf '%s\n' "$REGISTRY_NAME" >"$EVIDENCE_DIR/registry-name.txt"
printf '%s\n' "$NETWORK_NAME" >"$EVIDENCE_DIR/registry-network-name.txt"
printf '%s\n' "$REGISTRY_IMAGE" >"$EVIDENCE_DIR/registry-image-ref.txt"
printf '%s\n' "$NODE_IMAGE" >"$EVIDENCE_DIR/node-image-ref.txt"
printf '%s\n' "$REGISTRY_NETWORK_SUBNET" >"$EVIDENCE_DIR/registry-network-subnet.txt"
printf '%s\n' "$REGISTRY_NETWORK_GATEWAY" >"$EVIDENCE_DIR/registry-network-gateway.txt"

mapfile -t prior_network_ids < <(docker network ls -q)
if ((${#prior_network_ids[@]})); then
  docker network inspect "${prior_network_ids[@]}" >"$EVIDENCE_DIR/docker-networks-before.json"
else
  printf '[]\n' >"$EVIDENCE_DIR/docker-networks-before.json"
fi
ip -j route show >"$EVIDENCE_DIR/host-routes-before.json"
python3 "$SCRIPT_DIR/validate-kind-registry-subnet.py" \
  --subnet "$REGISTRY_NETWORK_SUBNET" \
  --gateway "$REGISTRY_NETWORK_GATEWAY" \
  --docker-networks "$EVIDENCE_DIR/docker-networks-before.json" \
  --host-routes "$EVIDENCE_DIR/host-routes-before.json" \
  >"$EVIDENCE_DIR/registry-network-subnet-validation.json"

if ! docker image inspect "$REGISTRY_IMAGE" >/dev/null 2>&1; then
  timeout 300s docker pull "$REGISTRY_IMAGE" >"$EVIDENCE_DIR/registry-image-pull.txt" 2>&1
fi
docker image inspect "$REGISTRY_IMAGE" --format '{{json .}}' \
  | jq '{id:.Id,repoDigests:.RepoDigests,architecture:.Architecture,os:.Os}' >"$EVIDENCE_DIR/registry-image-identity.json"
docker network create --driver bridge --subnet "$REGISTRY_NETWORK_SUBNET" --gateway "$REGISTRY_NETWORK_GATEWAY" \
  --label "$RUN_LABEL_KEY=$RUN_ID" \
  --label "local-env.opendatahub.io/kind-run=$CLUSTER_NAME" \
  "$NETWORK_NAME" >"$EVIDENCE_DIR/registry-network-id.txt"
docker run --detach --restart unless-stopped --name "$REGISTRY_NAME" \
  --network "$NETWORK_NAME" --network-alias "$REGISTRY_NAME" \
  --publish 127.0.0.1::5000 \
  --label "$RUN_LABEL_KEY=$RUN_ID" \
  --label "local-env.opendatahub.io/kind-run=$CLUSTER_NAME" \
  "$REGISTRY_IMAGE" >"$EVIDENCE_DIR/registry-container-id.txt"
REGISTRY_PORT="$(docker container inspect "$REGISTRY_NAME" --format '{{(index (index .NetworkSettings.Ports "5000/tcp") 0).HostPort}}')"
[[ "$REGISTRY_PORT" =~ ^[0-9]+$ ]] || fail 'could not resolve run-owned registry host port'
printf '%s\n' "$REGISTRY_PORT" >"$EVIDENCE_DIR/registry-port.txt"
registry_ready=false
for attempt in $(seq 1 60); do
  if curl --fail --silent "http://127.0.0.1:${REGISTRY_PORT}/v2/" >"$EVIDENCE_DIR/registry-health.json"; then
    registry_ready=true
    printf '%s\n' "$attempt" >"$EVIDENCE_DIR/registry-ready-attempt.txt"
    break
  fi
  sleep 1
done
[[ "$registry_ready" == true ]] || fail 'run-owned local registry did not become healthy within 60 seconds'

cat >"$EVIDENCE_DIR/kind-config.yaml" <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
- role: control-plane
  labels:
    praxis.fast/qualification-run: $RUN_ID
EOF
chmod 0600 "$EVIDENCE_DIR/kind-config.yaml"
printf 'kind create cluster --name %s --image %s --config %s --kubeconfig %s --wait 180s\n' \
  "$CLUSTER_NAME" "$NODE_IMAGE" "$EVIDENCE_DIR/kind-config.yaml" "$KUBECONFIG_PATH" \
  >"$EVIDENCE_DIR/kind-create-command.txt"
timeout 300s kind create cluster --name "$CLUSTER_NAME" --image "$NODE_IMAGE" \
  --config "$EVIDENCE_DIR/kind-config.yaml" --kubeconfig "$KUBECONFIG_PATH" --wait 180s \
  >"$EVIDENCE_DIR/kind-create.log" 2>&1
chmod 0600 "$KUBECONFIG_PATH"
KUBE_CONTEXT="kind-$CLUSTER_NAME"
NODE_NAME="${CLUSTER_NAME}-control-plane"
NODE_OWNER="$(docker inspect "$NODE_NAME" --format '{{index .Config.Labels "io.x-k8s.kind.cluster"}}')"
[[ "$NODE_OWNER" == "$CLUSTER_NAME" ]] || fail 'created Kind node ownership label mismatch'
docker image inspect "$NODE_IMAGE" --format '{{json .}}' \
  | jq '{id:.Id,repoDigests:.RepoDigests,architecture:.Architecture,os:.Os}' >"$EVIDENCE/node-image-identity.json"
docker network connect --gw-priority=-1 "$NETWORK_NAME" "$NODE_NAME" >"$EVIDENCE/registry-network-connect.txt"

KIND_CLUSTER_NAME="$CLUSTER_NAME" \
KIND_REGISTRY_CONTAINER="$REGISTRY_NAME" \
KIND_REGISTRY_NETWORK="$NETWORK_NAME" \
KIND_REGISTRY_PORT="$REGISTRY_PORT" \
E2E_RUN_ID="$RUN_ID" \
KUBE_CONTEXT="$KUBE_CONTEXT" \
KUBECONFIG="$KUBECONFIG_PATH" \
KIND_REGISTRY_EVIDENCE_DIR="$EVIDENCE_DIR/registry-configure" \
bash "$SCRIPT_DIR/configure-kind-registry.sh"

k() { KUBECONFIG="$KUBECONFIG_PATH" kubectl --context "$KUBE_CONTEXT" "$@"; }
k get --raw=/version >"$EVIDENCE/api-version.json"
k get --raw=/readyz >"$EVIDENCE/api-readyz.txt"
grep -qx 'ok' "$EVIDENCE/api-readyz.txt" || fail 'Kubernetes API /readyz is not ok'
k wait --for=condition=Ready "node/$NODE_NAME" --timeout=120s >"$EVIDENCE/node-ready.txt"
k get nodes -o json | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels,conditions:.status.conditions,capacity:.status.capacity,allocatable:.status.allocatable}]' \
  >"$EVIDENCE/nodes.json"
k get storageclass -o json | jq '[.items[]|{name:.metadata.name,provisioner:.provisioner,isDefault:(.metadata.annotations["storageclass.kubernetes.io/is-default-class"]//"false")}]' \
  >"$EVIDENCE/storageclasses.json"
jq -e 'any(.[];.isDefault=="true")' "$EVIDENCE/storageclasses.json" >/dev/null || fail 'fresh Kind cluster has no default StorageClass'
pending_zero=false
for attempt in $(seq 1 60); do
  k get pods -A --field-selector=status.phase=Pending -o json \
    | jq '[.items[]|{namespace:.metadata.namespace,name:.metadata.name,ownerReferences:.metadata.ownerReferences}]' \
      >"$EVIDENCE/pending-pods.json"
  pending_count="$(jq 'length' "$EVIDENCE/pending-pods.json")"
  printf '%s\t%s\n' "$attempt" "$pending_count" >>"$EVIDENCE/pending-pod-poll.tsv"
  if [[ "$pending_count" == 0 ]]; then pending_zero=true; break; fi
  sleep 2
done
[[ "$pending_zero" == true ]] || fail 'fresh Kind cluster has Pending Pods after the bounded bootstrap wait'
k get namespaces -o json | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid}]' >"$EVIDENCE/namespaces.json"
printf '{\n  "status": "PASS",\n  "run_id": "%s",\n  "cluster": "%s",\n  "context": "%s",\n  "api_server": "%s",\n  "registry": "%s",\n  "registry_port": %s,\n  "registry_network": "%s",\n  "pending_pods": 0\n}\n' \
  "$RUN_ID" "$CLUSTER_NAME" "$KUBE_CONTEXT" "$(k config view --minify -o jsonpath='{.clusters[0].cluster.server}')" \
  "$REGISTRY_NAME" "$REGISTRY_PORT" "$NETWORK_NAME" >"$EVIDENCE/RESULT.json"
printf 'run-owned Kind cluster and registry are Ready; API /readyz=ok, zero Pending Pods\n'
