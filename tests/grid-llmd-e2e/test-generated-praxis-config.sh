#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
validator="$SCRIPT_DIR/validate-generated-praxis-config.py"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

cat >"$tmpdir/endpoints.json" <<'JSON'
[
  {"cluster":"pool-a","address":"pool-a.gateway.svc:8080"},
  {"cluster":"pool-b","address":"pool-b.gateway.svc:8080"}
]
JSON
cat >"$tmpdir/config.yaml" <<'YAML'
listeners:
  - name: gateway
    route:
      overlayFile: /etc/praxis/routing/routing-overlay.json
      reload:
        enabled: true
    clusters:
      - name: pool-a
        address: pool-a.gateway.svc:8080
      - name: pool-b
        address: pool-b.gateway.svc:8080
YAML

python3 "$validator" "$tmpdir/endpoints.json" <"$tmpdir/config.yaml" >"$tmpdir/result.json"
jq -e '.status=="valid" and .overlayReloadEnabled and (.expectedEndpoints|length)==2 and .credentialLikeFields==[]' "$tmpdir/result.json" >/dev/null

if sed 's/enabled: true/enabled: false/' "$tmpdir/config.yaml" | python3 "$validator" "$tmpdir/endpoints.json" >/dev/null 2>&1; then
  printf 'validator accepted a disabled overlay reload\n' >&2
  exit 1
fi
if sed 's/pool-b.gateway.svc:8080/other.gateway.svc:8080/' "$tmpdir/config.yaml" | python3 "$validator" "$tmpdir/endpoints.json" >/dev/null 2>&1; then
  printf 'validator accepted a missing expected pool endpoint\n' >&2
  exit 1
fi
if sed '/address: pool-a.gateway.svc:8080/a\        api_key: should-never-be-written' "$tmpdir/config.yaml" | python3 "$validator" "$tmpdir/endpoints.json" >/dev/null 2>&1; then
  printf 'validator accepted a credential-like config field\n' >&2
  exit 1
fi

printf 'Grid-generated Praxis config structure, pool endpoint, overlay reload, and credential-field checks: PASS\n'
