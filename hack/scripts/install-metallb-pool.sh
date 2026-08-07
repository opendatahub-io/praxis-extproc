#!/usr/bin/env bash
# Configure MetalLB IPAddressPool from the Kind docker network.
# Usage: install-metallb-pool.sh <kube-context>
set -euo pipefail

CTX="${1:?usage: install-metallb-pool.sh <kube-context>}"

if kubectl --context "$CTX" get ipaddresspool e2e-pool -n metallb-system &>/dev/null; then
  echo "MetalLB pool already configured"
  exit 0
fi

KIND_SUBNET=$(docker network inspect kind \
  -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' \
  | tr ' ' '\n' | grep '\.' | head -1)
[[ -n "$KIND_SUBNET" ]] || { echo "cannot determine Kind subnet" >&2; exit 1; }
LB_BASE=$(echo "$KIND_SUBNET" | cut -d'.' -f1-3)

for _ in $(seq 1 6); do
  if kubectl --context "$CTX" apply -f - <<EOF 2>/dev/null
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: e2e-pool
  namespace: metallb-system
spec:
  addresses:
  - ${LB_BASE}.200-${LB_BASE}.250
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata:
  name: e2e-l2
  namespace: metallb-system
EOF
  then
    echo "MetalLB pool ${LB_BASE}.200-250"
    exit 0
  fi
  echo "MetalLB webhook not ready, retrying..."
  sleep 10
done
echo "failed to configure MetalLB pool" >&2
exit 1
