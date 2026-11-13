#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR" <<'PY'
from pathlib import Path
import sys

scripts = Path(sys.argv[1])
lifecycle = (scripts / "run-kind-lifecycle.sh").read_text()
renderer = (scripts / "render_kind_aigc_manifest_bundle.py").read_text()
installer = (scripts / "install-kind-payload-tls.sh").read_text()
verifier = (scripts / "verify-kind-payload-tls.sh").read_text()
checker = (scripts / "verify_kind_payload_tls_config.py").read_text()

render = lifecycle.index('python3 "$SCRIPT_DIR/render_kind_aigc_manifest_bundle.py"')
issue = lifecycle.index('run_phase payload-certificates --')
bundle_apply = lifecycle.index('apply -f "$AIGC_BUNDLE"')
tenant_apply = lifecycle.index('apply -f "$RUN_DIR/platform/default-aitenant.yaml"')
connectivity = lifecycle.index('run_phase payload-tls-connectivity --')
platform_ready = lifecycle.index('run_phase maas-platform-ready --')
assert render < issue < bundle_apply, "payload Certificates must be issued before the AIGC payload Deployments are applied"
assert tenant_apply < connectivity < platform_ready, "live payload TLS verification must precede aggregate platform and serving-pool gates"
focused_tests = lifecycle.index('phase_start payload-tls-renderer-tests')
cluster_create = lifecycle.index('run_phase kind-cluster-and-registry')
image_build = lifecycle.index('run_phase kind-images-build')
assert lifecycle.index('phase_end 0', focused_tests) < cluster_create < image_build, "locked-source renderer checks must complete before cluster/image work"
assert lifecycle.index('test-render-kind-aigc-source.sh', focused_tests) < cluster_create, "the exact resolved AIGC source must pass the renderer before cluster provisioning"

for required in (
    "payload-pre-processing-tls",
    "payload-processing-tls",
    "payload-extproc-serving-ca",
    ".svc.cluster.local",
    "issuerRef",
    "secretTemplate",
):
    assert required in renderer or required in installer, f"missing payload TLS contract term: {required}"
assert 'TENANT_POOLS = ("pool-a", "pool-b")' in renderer, "renderer must cover both task pool identities"
assert 'f"{service_name}-{pool}-tls"' in renderer, "renderer must name tenant Certificates from the live Service convention"

pool_a_auth = lifecycle.index("run_phase pool-a-authorization")
pool_a_tls = lifecycle.index("run_phase pool-a-payload-tls")
pool_a_probe = lifecycle.index("phase_start pool-a-setup-receipt")
pool_b_auth = lifecycle.index("run_phase pool-b-authorization")
pool_b_tls = lifecycle.index("run_phase pool-b-payload-tls")
pool_b_probe = lifecycle.index("phase_start pool-b-setup-receipt")
assert pool_a_auth < pool_a_tls < pool_a_probe, "Pool A tenant TLS must pass before its setup receipt"
assert pool_b_auth < pool_b_tls < pool_b_probe, "Pool B tenant TLS must pass before its setup receipt"

for required in (
    "-verify_hostname",
    "-servername",
    "-verify_return_error",
    "pilot-agent request GET config_dump",
    "'/clusters?format=json'",
    "destinationrules.networking.istio.io",
    "endpointslices",
    "pilot-agent request GET config_dump",
):
    assert required in verifier, f"live TLS verifier is missing {required}"

for required in (
    "subjectAltNames",
    "credentialName",
    "match_typed_subject_alt_names",
    "trusted_ca",
    "xds_dump",
):
    assert required in checker, f"live configuration checker is missing {required}"
assert "--pool" in verifier and "grid-llmd-$POOL" in verifier, "live TLS verifier must inspect pool-scoped Service and Gateway contracts"

assert 'jq -r \'.data["tls.key"]\' "$secret_json" >"$EVIDENCE_DIR/' not in installer
print("Kind payload TLS lifecycle ordering and live DNS/SNI/issuer gates are wired")
PY
