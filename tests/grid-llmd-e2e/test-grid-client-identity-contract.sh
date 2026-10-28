#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$ROOT/tests/grid-llmd-e2e/deploy-grid-client-identity.sh"

bash -n "$SCRIPT"
for required in \
  'resourceNames: [$POOL_A, $POOL_B]' \
  'resources: [models]' \
  'resourceNames: ["$MODEL_RESOURCE_NAME"]' \
  'verbs: [post]' \
  'authorization_review post models' \
  'kind:"SubjectAccessReview"' \
  'verbs: [get]' \
  'oc apply --dry-run=server' \
  'tokenMinted:false' \
  'tokenRecorded:false' \
  'praxis.fast/qualification-run'; do
  if ! rg -Fq "$required" "$SCRIPT"; then
    printf 'Grid client identity contract missing: %s\n' "$required" >&2
    exit 1
  fi
done
if rg -n 'oc (delete|patch|scale|replace|set)' "$SCRIPT"; then
  printf 'Grid client identity install must not mutate unrelated resources\n' >&2
  exit 1
fi
if rg -n '^[^#]*\boc create token\b|^[^#]*GRID_LLMD_AUTH_TOKEN=|^[^#]*--auth-token' "$SCRIPT"; then
  printf 'ServiceAccount token must be minted outside the resource/evidence script\n' >&2
  exit 1
fi

printf 'Grid client identity is run-owned, least-privilege, and keeps the token out of evidence\n'
