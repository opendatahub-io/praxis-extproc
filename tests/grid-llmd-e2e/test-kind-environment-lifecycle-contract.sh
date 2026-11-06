#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CREATE="$SCRIPT_DIR/create-kind-environment.sh"
DESTROY="$SCRIPT_DIR/destroy-kind-environment.sh"
create_source="$(<"$CREATE")"
destroy_source="$(<"$DESTROY")"

for required in 'KIND_CLUSTER_NAME' 'KIND_REGISTRY_CONTAINER' 'KIND_REGISTRY_NETWORK' 'KUBECONFIG_PATH' 'KIND_STATE_DIR' 'KIND_ENV_EVIDENCE_DIR'; do
  rg -Fq "$required" "$CREATE" || { printf 'create helper is missing required input %s\n' "$required" >&2; exit 1; }
done
rg -Fq 'refusing to adopt it' <<<"$create_source" || { printf 'create helper does not reject existing run names\n' >&2; exit 1; }
rg -Fq 'praxis.fast/qualification-run' <<<"$create_source" || { printf 'create helper does not label owned resources\n' >&2; exit 1; }
rg -Fq 'configure-kind-registry.sh' <<<"$create_source" || { printf 'create helper does not use the existing registry configuration gate\n' >&2; exit 1; }
rg -Fq 'status.phase=Pending' <<<"$create_source" || { printf 'create helper does not verify Pending Pods\n' >&2; exit 1; }
rg -Fq 'KIND_MIN_FREE_DISK_GIB' <<<"$create_source" || { printf 'create helper does not gate disk headroom\n' >&2; exit 1; }
rg -Fq 'KIND_MIN_AVAILABLE_MEMORY_GIB' <<<"$create_source" || { printf 'create helper does not gate memory headroom\n' >&2; exit 1; }
rg -Fq 'validate-kind-registry-subnet.py' <<<"$create_source" || { printf 'create helper does not verify an explicit registry subnet against local routes\n' >&2; exit 1; }
rg -Fq 'mapfile -t prior_network_ids' <<<"$create_source" || { printf 'create helper does not safely snapshot Docker networks before subnet validation\n' >&2; exit 1; }
rg -Fq -- '--subnet "$REGISTRY_NETWORK_SUBNET"' <<<"$create_source" || { printf 'create helper does not allocate the validated registry subnet\n' >&2; exit 1; }
rg -Fq 'registry container ownership label mismatch' <<<"$destroy_source" || { printf 'destroy helper does not enforce container ownership\n' >&2; exit 1; }
rg -Fq 'registry network ownership label mismatch' <<<"$destroy_source" || { printf 'destroy helper does not enforce network ownership\n' >&2; exit 1; }
rg -Fq 'registry-network-name.txt' <<<"$destroy_source" || { printf 'destroy helper does not verify the network name recorded during creation\n' >&2; exit 1; }
rg -Fq 'run-owned Docker networks remain after cleanup' <<<"$destroy_source" || { printf 'destroy helper does not reject leftover run-owned Docker networks\n' >&2; exit 1; }
rg -Fq 'registry data mount is not an anonymous Docker volume' <<<"$destroy_source" || { printf 'destroy helper does not validate the run registry data volume\n' >&2; exit 1; }
rg -Fq 'docker volume rm "$registry_volume"' <<<"$destroy_source" || { printf 'destroy helper does not remove the exact registry data volume\n' >&2; exit 1; }
rg -Fq 'registry data volume remains after deletion' <<<"$destroy_source" || { printf 'destroy helper does not verify registry data volume cleanup\n' >&2; exit 1; }
rg -Fq 'Kind node run-ownership label mismatch' <<<"$destroy_source" || { printf 'destroy helper does not enforce Kind node run ownership\n' >&2; exit 1; }
rg -Fq 'kind delete cluster --name "$CLUSTER_NAME"' <<<"$destroy_source" || { printf 'destroy helper does not delete only the requested cluster\n' >&2; exit 1; }
if rg -n '\$EVIDENCE/' <<<"$destroy_source"; then
  printf 'destroy helper must write all evidence under its configured EVIDENCE_DIR\n' >&2
  exit 1
fi
rg -Fq 'KIND_DESTROY_EVIDENCE_DIR' <<<"$destroy_source" || { printf 'destroy helper does not consume the configured evidence directory\n' >&2; exit 1; }
rg -Fq 'KIND_ENV_EVIDENCE_DIR' <<<"$destroy_source" || { printf 'destroy helper does not consume the run bootstrap evidence directory\n' >&2; exit 1; }
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -m 0700 "$tmp/state" "$tmp/bootstrap"
: >"$tmp/state/kubeconfig"
printf 'test-run\n' >"$tmp/bootstrap/run-id.txt"
printf 'kind-test-run\n' >"$tmp/bootstrap/cluster-name.txt"
printf 'kind-test-registry\n' >"$tmp/bootstrap/registry-name.txt"
printf 'kind-test-run-network\n' >"$tmp/bootstrap/registry-network-name.txt"
if env \
  E2E_RUN_ID=test-run \
  KIND_CLUSTER_NAME=kind-test-run \
  KIND_REGISTRY_CONTAINER=kind-test-registry \
  KIND_REGISTRY_NETWORK=wrong-network \
  KIND_STATE_DIR="$tmp/state" \
  KUBECONFIG="$tmp/state/kubeconfig" \
  KIND_ENV_EVIDENCE_DIR="$tmp/bootstrap" \
  KIND_DESTROY_EVIDENCE_DIR="$tmp/destroy-evidence" \
  bash "$DESTROY" >"$tmp/mismatched-network.log" 2>&1; then
  printf 'destroy helper accepted a network name that differs from bootstrap evidence\n' >&2
  exit 1
fi
rg -Fq 'configured registry network does not match the network recorded at creation' "$tmp/mismatched-network.log" \
  || { printf 'destroy helper did not report a mismatched registry network name\n' >&2; exit 1; }
if rg -n 'docker (system )?prune|kind delete clusters' "$CREATE" "$DESTROY"; then
  printf 'lifecycle helpers must not prune or delete unrelated resources\n' >&2
  exit 1
fi
bash -n "$CREATE"
bash -n "$DESTROY"
python3 -m py_compile "$SCRIPT_DIR/validate-kind-registry-subnet.py" "$SCRIPT_DIR/test_validate_kind_registry_subnet.py"
python3 "$SCRIPT_DIR/test_validate_kind_registry_subnet.py"
printf 'Kind lifecycle ownership and bounded-readiness contracts passed\n'
