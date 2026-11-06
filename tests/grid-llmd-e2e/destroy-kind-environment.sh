#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Remove only the exact Kind cluster, registry and network owned by one run.
set -Eeuo pipefail
umask 077

RUN_ID="${E2E_RUN_ID:-}"
CLUSTER_NAME="${KIND_CLUSTER_NAME:-}"
REGISTRY_NAME="${KIND_REGISTRY_CONTAINER:-}"
NETWORK_NAME="${KIND_REGISTRY_NETWORK:-}"
KUBECONFIG_PATH="${KUBECONFIG:-}"
STATE_DIR="${KIND_STATE_DIR:-}"
EVIDENCE_DIR="${KIND_DESTROY_EVIDENCE_DIR:-}"
BOOTSTRAP_EVIDENCE_DIR="${KIND_ENV_EVIDENCE_DIR:-}"
RUN_LABEL_KEY="praxis.fast/qualification-run"
fail() { printf 'Kind environment cleanup blocked: %s\n' "$*" >&2; exit 2; }
for name in RUN_ID CLUSTER_NAME REGISTRY_NAME NETWORK_NAME KUBECONFIG_PATH STATE_DIR EVIDENCE_DIR BOOTSTRAP_EVIDENCE_DIR; do
  [[ -n "${!name}" ]] || fail "$name is required"
done
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || fail 'E2E_RUN_ID is not a safe Kubernetes label value'
[[ "$CLUSTER_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ ]] || fail 'KIND_CLUSTER_NAME is not a safe DNS label'
[[ "$REGISTRY_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ ]] || fail 'KIND_REGISTRY_CONTAINER is not a safe Docker name'
[[ "$NETWORK_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ ]] || fail 'KIND_REGISTRY_NETWORK is not a safe Docker name'
[[ "$KUBECONFIG_PATH" == "$STATE_DIR"/* ]] || fail 'private run kubeconfig path must be under KIND_STATE_DIR'
[[ "$STATE_DIR" != "$EVIDENCE_DIR" && "$STATE_DIR" != "$EVIDENCE_DIR"/* ]] || fail 'private run state must be outside evidence'
[[ -d "$BOOTSTRAP_EVIDENCE_DIR" ]] || fail 'KIND_ENV_EVIDENCE_DIR must identify the completed run bootstrap evidence'
[[ "$(<"$BOOTSTRAP_EVIDENCE_DIR/run-id.txt")" == "$RUN_ID" ]] || fail 'bootstrap evidence run ID mismatch'
[[ "$(<"$BOOTSTRAP_EVIDENCE_DIR/cluster-name.txt")" == "$CLUSTER_NAME" ]] || fail 'bootstrap evidence cluster name mismatch'
[[ "$(<"$BOOTSTRAP_EVIDENCE_DIR/registry-name.txt")" == "$REGISTRY_NAME" ]] || fail 'bootstrap evidence registry name mismatch'
[[ "$(<"$BOOTSTRAP_EVIDENCE_DIR/registry-network-name.txt")" == "$NETWORK_NAME" ]] || fail 'configured registry network does not match the network recorded at creation'
[[ ! -e "$EVIDENCE_DIR" ]] || fail 'evidence directory already exists'
for binary in docker kind jq; do command -v "$binary" >/dev/null 2>&1 || fail "$binary is required"; done
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

cluster_exists=false
if kind get clusters 2>/dev/null | rg -qx "$CLUSTER_NAME"; then cluster_exists=true; fi
if [[ "$cluster_exists" == true ]]; then
  [[ -f "$KUBECONFIG_PATH" ]] || fail 'private run kubeconfig is missing while the owned Kind cluster still exists'
  node_name="${CLUSTER_NAME}-control-plane"
  node_owner="$(docker inspect "$node_name" --format '{{index .Config.Labels "io.x-k8s.kind.cluster"}}' 2>/dev/null || true)"
  [[ "$node_owner" == "$CLUSTER_NAME" ]] || fail 'Kind control-plane cluster ownership label mismatch'
  KUBECONFIG="$KUBECONFIG_PATH" kubectl --context "kind-$CLUSTER_NAME" get "node/$node_name" -o json >"$EVIDENCE_DIR/kind-node-precheck.json"
  node_run="$(jq -er --arg key "$RUN_LABEL_KEY" '.metadata.labels[$key]' "$EVIDENCE_DIR/kind-node-precheck.json")"
  [[ "$node_run" == "$RUN_ID" ]] || fail 'Kind node run-ownership label mismatch'
fi

registry_exists=false
registry_volume=""
if docker container inspect "$REGISTRY_NAME" >/dev/null 2>&1; then
  registry_exists=true
  registry_owner="$(docker container inspect "$REGISTRY_NAME" --format "{{index .Config.Labels \"$RUN_LABEL_KEY\"}}")"
  [[ "$registry_owner" == "$RUN_ID" ]] || fail 'registry container ownership label mismatch'
  docker container inspect "$REGISTRY_NAME" --format '{{json .Mounts}}' >"$EVIDENCE_DIR/registry-mounts-pre-delete.json"
  registry_volume="$(jq -er '[.[]|select(.Destination=="/var/lib/registry" and .Type=="volume")|.Name] | if length==1 then .[0] else error("expected one registry data volume") end' "$EVIDENCE_DIR/registry-mounts-pre-delete.json")"
  [[ "$registry_volume" =~ ^[a-f0-9]{64}$ ]] || fail 'registry data mount is not an anonymous Docker volume; refusing ambiguous cleanup'
  docker volume inspect "$registry_volume" >"$EVIDENCE_DIR/registry-volume-pre-delete.json"
fi
network_exists=false
if docker network inspect "$NETWORK_NAME" >/dev/null 2>&1; then
  network_exists=true
  network_owner="$(docker network inspect "$NETWORK_NAME" --format "{{index .Labels \"$RUN_LABEL_KEY\"}}")"
  [[ "$network_owner" == "$RUN_ID" ]] || fail 'registry network ownership label mismatch'
fi
snapshot_run_owned_networks() {
  local output="$1"
  local -a network_ids=()
  mapfile -t network_ids < <(docker network ls -q)
  if ((${#network_ids[@]})); then
    docker network inspect "${network_ids[@]}" \
      | jq --arg key "$RUN_LABEL_KEY" --arg run "$RUN_ID" \
          '[.[]|select(.Labels[$key]==$run)|{name:.Name,id:.Id,labels:.Labels,containerCount:((.Containers//{})|length)}]' \
      >"$output"
  else
    printf '[]\n' >"$output"
  fi
}
snapshot_run_owned_networks "$EVIDENCE_DIR/run-owned-networks-before.json"
if [[ "$cluster_exists" == true ]]; then
  jq -e --arg name "$NETWORK_NAME" 'length==1 and .[0].name==$name' \
    "$EVIDENCE_DIR/run-owned-networks-before.json" >/dev/null \
    || fail 'configured registry network is not the sole Docker network owned by this run'
else
  jq -e --arg name "$NETWORK_NAME" 'length==0 or (length==1 and .[0].name==$name)' \
    "$EVIDENCE_DIR/run-owned-networks-before.json" >/dev/null \
    || fail 'configured registry network does not match the run-owned Docker network inventory'
fi
if [[ "$cluster_exists" == true ]]; then
  KUBECONFIG="$KUBECONFIG_PATH" kubectl --context "kind-$CLUSTER_NAME" get --raw=/readyz \
    >"$EVIDENCE_DIR/pre-delete-readyz.txt" 2>&1 || true
  KUBECONFIG="$KUBECONFIG_PATH" kubectl --context "kind-$CLUSTER_NAME" get namespaces -o json \
    | jq '[.items[]|{name:.metadata.name,uid:.metadata.uid,labels:.metadata.labels}]' >"$EVIDENCE_DIR/pre-delete-namespaces.json" || true
fi
printf '{"cluster":"%s","cluster_owned":%s,"registry":"%s","registry_owned":%s,"network":"%s","network_owned":%s}\n' \
  "$CLUSTER_NAME" "$cluster_exists" "$REGISTRY_NAME" "$registry_exists" "$NETWORK_NAME" "$network_exists" \
  >"$EVIDENCE_DIR/ownership-precheck.json"

if [[ "$cluster_exists" == true ]]; then
  timeout 180s kind delete cluster --name "$CLUSTER_NAME" >"$EVIDENCE_DIR/kind-delete.txt" 2>&1
fi
if [[ "$registry_exists" == true ]]; then docker rm -f "$REGISTRY_NAME" >"$EVIDENCE_DIR/registry-delete.txt"; fi
if [[ -n "$registry_volume" ]]; then
  attached_containers="$(docker ps -aq --filter "volume=$registry_volume")"
  [[ -z "$attached_containers" ]] || fail 'registry anonymous volume is still attached to a container'
  docker volume rm "$registry_volume" >"$EVIDENCE_DIR/registry-volume-delete.txt"
fi
if [[ "$network_exists" == true ]]; then
  network_members="$(docker network inspect "$NETWORK_NAME" --format '{{range .Containers}}{{println .Name}}{{end}}')"
  [[ -z "$network_members" ]] || fail 'run registry network still has attached containers after cluster/registry deletion'
  docker network rm "$NETWORK_NAME" >"$EVIDENCE_DIR/registry-network-delete.txt"
fi

if kind get clusters 2>/dev/null | rg -qx "$CLUSTER_NAME"; then fail 'Kind cluster remains after deletion'; fi
if docker container inspect "$REGISTRY_NAME" >/dev/null 2>&1; then fail 'registry container remains after deletion'; fi
if docker network inspect "$NETWORK_NAME" >/dev/null 2>&1; then fail 'registry network remains after deletion'; fi
if [[ -n "$registry_volume" ]] && docker volume inspect "$registry_volume" >/dev/null 2>&1; then fail 'run registry data volume remains after deletion'; fi
snapshot_run_owned_networks "$EVIDENCE_DIR/run-owned-networks-after.json"
jq -e 'length==0' "$EVIDENCE_DIR/run-owned-networks-after.json" >/dev/null \
  || fail 'run-owned Docker networks remain after cleanup'
if [[ -f "$KUBECONFIG_PATH" ]]; then
  rm -f "$KUBECONFIG_PATH"
  printf 'removed\n' >"$EVIDENCE_DIR/private-kubeconfig-cleanup.txt"
else
  printf 'already-absent\n' >"$EVIDENCE_DIR/private-kubeconfig-cleanup.txt"
fi
kind get clusters >"$EVIDENCE_DIR/kind-clusters-after.txt"
printf '{"status":"PASS","cluster_removed":true,"registry_removed":true,"network_removed":true,"run_owned_networks_remaining":0,"docker_caches_pruned":false}\n' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'run-owned Kind cluster and registry resources removed; shared Docker caches were left intact\n'
