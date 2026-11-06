#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Configure a run-owned Kind node to pull from its run-owned local registry.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CLUSTER_NAME="${KIND_CLUSTER_NAME:-}"
REGISTRY_NAME="${KIND_REGISTRY_CONTAINER:-}"
NETWORK_NAME="${KIND_REGISTRY_NETWORK:-}"
REGISTRY_PORT="${KIND_REGISTRY_PORT:-}"
RUN_ID="${E2E_RUN_ID:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
KUBECONFIG_PATH="${KUBECONFIG:-}"
EVIDENCE_DIR="${KIND_REGISTRY_EVIDENCE_DIR:-}"

for name in CLUSTER_NAME REGISTRY_NAME NETWORK_NAME REGISTRY_PORT RUN_ID KUBE_CONTEXT KUBECONFIG_PATH EVIDENCE_DIR; do
  [[ -n "${!name}" ]] || { printf '%s is required\n' "$name" >&2; exit 2; }
done
[[ "$RUN_ID" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{2,60}$ ]] || { printf 'E2E_RUN_ID is not a safe label value\n' >&2; exit 2; }
[[ "$REGISTRY_PORT" =~ ^[0-9]+$ ]] || { printf 'KIND_REGISTRY_PORT must be numeric\n' >&2; exit 2; }
[[ -f "$KUBECONFIG_PATH" && "$KUBECONFIG_PATH" != "$EVIDENCE_DIR"/* ]] || { printf 'private KUBECONFIG must exist outside evidence\n' >&2; exit 2; }
for binary in docker kubectl python3 curl rg; do
  command -v "$binary" >/dev/null 2>&1 || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

NODE_NAME="${CLUSTER_NAME}-control-plane"
NODE_OWNER="$(docker inspect "$NODE_NAME" --format '{{index .Config.Labels "io.x-k8s.kind.cluster"}}')"
REGISTRY_OWNER="$(docker inspect "$REGISTRY_NAME" --format '{{index .Config.Labels "praxis.fast/qualification-run"}}')"
NETWORK_OWNER="$(docker network inspect "$NETWORK_NAME" --format '{{index .Labels "praxis.fast/qualification-run"}}')"
[[ "$NODE_OWNER" == "$CLUSTER_NAME" ]] || { printf 'Kind node ownership label mismatch\n' >&2; exit 2; }
[[ "$REGISTRY_OWNER" == "$RUN_ID" ]] || { printf 'registry container ownership label mismatch\n' >&2; exit 2; }
[[ "$NETWORK_OWNER" == "$RUN_ID" ]] || { printf 'registry network ownership label mismatch\n' >&2; exit 2; }
network_members="$(docker network inspect "$NETWORK_NAME" --format '{{range .Containers}}{{println .Name}}{{end}}')"
rg -qxF "$NODE_NAME" <<<"$network_members" || { printf 'Kind node is not attached to the run registry network\n' >&2; exit 2; }
rg -qxF "$REGISTRY_NAME" <<<"$network_members" || { printf 'registry is not attached to the run registry network\n' >&2; exit 2; }
curl --fail --silent --show-error "http://127.0.0.1:${REGISTRY_PORT}/v2/" >"$EVIDENCE_DIR/registry-health.json"

HOST_DIR="/etc/containerd/certs.d/${REGISTRY_NAME}:5000"
printf '[host."http://%s:5000"]\n  capabilities = ["pull", "resolve"]\n' "$REGISTRY_NAME" >"$EVIDENCE_DIR/registry-hosts.toml"
docker exec "$NODE_NAME" mkdir -p "$HOST_DIR"
docker cp "$EVIDENCE_DIR/registry-hosts.toml" "$NODE_NAME:$HOST_DIR/hosts.toml" >/dev/null
docker cp "$NODE_NAME:/etc/containerd/config.toml" "$EVIDENCE_DIR/containerd-config.before.toml" >/dev/null
python3 "$SCRIPT_DIR/configure-kind-registry-config.py" \
  "$EVIDENCE_DIR/containerd-config.before.toml" \
  "$EVIDENCE_DIR/containerd-config.after.toml" \
  >"$EVIDENCE_DIR/containerd-config-transform.txt"
if ! cmp -s "$EVIDENCE_DIR/containerd-config.before.toml" "$EVIDENCE_DIR/containerd-config.after.toml"; then
  docker cp "$EVIDENCE_DIR/containerd-config.after.toml" "$NODE_NAME:/etc/containerd/config.toml" >/dev/null
  docker exec "$NODE_NAME" systemctl restart containerd
fi
docker exec "$NODE_NAME" systemctl is-active --quiet containerd
KUBECONFIG="$KUBECONFIG_PATH" kubectl --context "$KUBE_CONTEXT" wait \
  --for=condition=Ready "node/$NODE_NAME" --timeout=120s >"$EVIDENCE_DIR/node-ready.txt"
KUBECONFIG="$KUBECONFIG_PATH" kubectl --context "$KUBE_CONTEXT" get --raw=/readyz >"$EVIDENCE_DIR/readyz.txt"
grep -qx 'ok' "$EVIDENCE_DIR/readyz.txt" || { printf 'Kubernetes API /readyz failed\n' >&2; exit 1; }

printf 'cluster=%s\nnode=%s\nregistry=%s\nnetwork=%s\nregistry_port=%s\nregistry_owner=%s\n' \
  "$CLUSTER_NAME" "$NODE_NAME" "$REGISTRY_NAME" "$NETWORK_NAME" "$REGISTRY_PORT" "$RUN_ID" \
  >"$EVIDENCE_DIR/RESULT.txt"
printf 'run-owned Kind registry configuration and API readiness verified\n'
