#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
HOOK="$ROOT/tests/grid-llmd-e2e/gateway-revision-hook.sh"

bash -n "$HOOK"
for required in \
  'routing-overlay.json' \
  '/etc/praxis/praxis.yaml' \
  '/etc/praxis/routing/routing-overlay.json' \
  '.revision.value' \
  'mountReconciliationStatus' \
  'appliedRevision' \
  'content_digest' \
  'grid-consumer-config-' \
  'routeRetryLimit' \
  'retryAttempts' \
  'upstreamRequests' \
  '/clusters?format=json' \
  'rq_total' \
  'requestCounterPresent' \
  'counterSource:"envoy_clusters_per_host_rq_total"' \
  'gateway.networking.k8s.io/gateway-name' \
  'praxis.fast/qualification-run'; do
  if ! rg -Fq "$required" "$HOOK"; then
    printf 'gateway revision hook missing contract: %s\n' "$required" >&2
    exit 1
  fi
done
if rg -n 'GRID_SERVING_CONFIG|serving-config.json' "$HOOK"; then
  printf 'Gateway revision hook must validate the operator Praxis config and directly mounted Grid overlay\n' >&2
  exit 1
fi
if rg -n 'oc (apply|create|delete|patch|scale|replace|set)' "$HOOK"; then
  printf 'gateway revision hook must remain read-only\n' >&2
  exit 1
fi
if rg -n 'kubectl (apply|create|delete|patch|scale|replace|set)' "$HOOK"; then
  printf 'gateway revision hook must remain read-only\n' >&2
  exit 1
fi

printf 'Gateway revision hook read-only ownership, overlay, and upstream-attempt counter contract passed\n'
