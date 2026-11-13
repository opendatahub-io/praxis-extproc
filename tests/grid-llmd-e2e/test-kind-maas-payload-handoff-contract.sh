#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
helper="$SCRIPT_DIR/handoff-kind-maas-payload-processing.sh"
grep -Fq 'app.kubernetes.io/managed-by"]=="grid-llmd-kind"' "$helper"
grep -Fq '.metadata.uid==$uid' "$helper"
grep -Fq 'app.kubernetes.io/component"]=="payload-processing"' "$helper"
grep -Fq 'ownerRefs' "$helper"
grep -Fq 'Resources with another manager are retained' "$helper"
if grep -Eq 'remove-finalizer|finalizers\[\]|jsonpath=.*finalizers' "$helper"; then
  printf 'payload handoff helper must not bypass finalizers\n' >&2
  exit 1
fi
printf 'Kind MaaS payload handoff contract passed\n'
