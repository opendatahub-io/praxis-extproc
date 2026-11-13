#!/usr/bin/env bash
# shellcheck disable=SC2016 # assertions intentionally match literal source fragments.
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
helper="$SCRIPT_DIR/create-kind-grid-identity.sh"
[[ -x "$helper" ]] || { printf 'Kind Grid identity helper must be executable\n' >&2; exit 1; }

# The Grid operator owns these identity Secrets. The Kind helper may wait for and
# validate them, but must never replace them with cert-manager or kubectl apply.
grep -Fq 'Wait for and verify the self-signed identity created by the Grid operator' "$helper"
grep -Fq 'GRID_IDENTITY_MODE:-apply' "$helper"
grep -Fq 'operator-created grid-ca Secret is missing' "$helper"
grep -Fq 'Grid operator did not create both identity Secrets' "$helper"
grep -Fq '(.data|keys|sort)==["ca.crt","ca.key"]' "$helper"
grep -Fq '(.data|keys|sort)==["tls.crt","tls.key"]' "$helper"
grep -Fq 'cmp -s "$tmpdir/ca-cert.pub" "$tmpdir/ca-key.pub"' "$helper"
grep -Fq 'cmp -s "$tmpdir/site-cert.pub" "$tmpdir/site-key.pub"' "$helper"
grep -Fq 'openssl verify -verify_hostname "$SERVER_NAME" -CAfile' "$helper"
grep -Fq 'spiffe://grid.internal/site/$SITE_NAME' "$helper"
grep -Fq 'GridNetwork TLS references do not match' "$helper"
grep -Fq 'identity Secret predates the task GridNetwork' "$helper"
grep -Fq 'no identity Secret was modified' "$helper"
grep -Fq 'gridnetwork-failure-diagnostics.json' "$helper"
grep -Fq 'keyNames:(.data|keys)' "$helper"

if grep -Eq 'apiVersion: cert-manager.io/v1|oc apply -f -|secretName: grid-site-identity-issued|grid-ca-secret-apply' "$helper"; then
  printf 'identity helper must not create or replace Grid identity Secrets\n' >&2
  exit 1
fi
if grep -Eq 'oc .*get secret .* -o (yaml|json) >.*EVIDENCE_DIR|\.data.*\|base64 -d >.*EVIDENCE_DIR|private.?key.*\.txt' "$helper"; then
  printf 'identity helper has a suspect secret-evidence path\n' >&2
  exit 1
fi
printf 'Kind Grid identity contract passed\n'
