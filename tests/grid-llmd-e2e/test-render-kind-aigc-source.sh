#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Render the exact resolved AIGC source through the Kind payload TLS adapter.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_DIR="${1:?usage: test-render-kind-aigc-source.sh LOCKED-AIGC-SOURCE-DIR SUMMARY.json}"
SUMMARY="${2:?usage: test-render-kind-aigc-source.sh LOCKED-AIGC-SOURCE-DIR SUMMARY.json}"
[[ -d "$SOURCE_DIR/config/manifests/praxis-extproc/overlays/odh" ]] || {
  printf 'locked AIGC source lacks the Praxis ExtProc ODH overlay: %s\n' "$SOURCE_DIR" >&2
  exit 2
}
SOURCE_SHA="$(git -C "$SOURCE_DIR" rev-parse HEAD)"
SOURCE_BRANCH="$(git -C "$SOURCE_DIR" branch --show-current)"
SOURCE_STATUS="$(git -C "$SOURCE_DIR" status --porcelain --untracked-files=all)"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ && -z "$SOURCE_BRANCH" && -z "$SOURCE_STATUS" ]] || {
  printf 'AIGC renderer test must use a clean detached source worktree\n' >&2
  exit 2
}
[[ ! -e "$SUMMARY" ]] || { printf 'refusing to overwrite renderer summary: %s\n' "$SUMMARY" >&2; exit 2; }
for binary in kustomize openssl python3; do command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }; done

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aigc-kind-render-contract.XXXXXXXX")"
chmod 0700 "$WORK_DIR"
trap '[[ ! -d "$WORK_DIR" ]] || rm -r -- "$WORK_DIR"' EXIT
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$WORK_DIR/test-ca.key" -out "$WORK_DIR/test-ca.pem" \
  -days 1 -subj '/CN=kind-payload-render-contract-only' \
  -addext 'basicConstraints=critical,CA:TRUE' -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  >"$WORK_DIR/openssl-ca.log" 2>&1
python3 "$SCRIPT_DIR/render_kind_aigc_manifest_bundle.py" \
  --source-dir "$SOURCE_DIR/config/manifests/praxis-extproc/overlays/odh" \
  --output "$WORK_DIR/bundle.yaml" --certificates-output "$WORK_DIR/certificates.yaml" \
  --payload-ca-file "$WORK_DIR/test-ca.pem" --run-id renderer-contract-20261107 \
  --namespace opendatahub --payload-namespace maas-system --name kind-praxis-extproc-render-contract \
  >"$WORK_DIR/render.log"
python3 - "$WORK_DIR/bundle.yaml" "$WORK_DIR/certificates.yaml" "$SUMMARY" "$SOURCE_SHA" <<'PY'
import json, sys
from pathlib import Path
import yaml

bundle_path, certs_path, summary_path = map(Path, sys.argv[1:4])
source_sha = sys.argv[4]
bundle = yaml.safe_load(bundle_path.read_text())
resources = [item for item in yaml.safe_load_all(bundle["data"]["resources.yaml"]) if isinstance(item, dict)]
certificates = [item for item in yaml.safe_load_all(certs_path.read_text()) if isinstance(item, dict)]
expected = {"payload-pre-processing", "payload-processing"}
services = {item["metadata"]["name"] for item in resources if item.get("kind") == "Service" and item.get("metadata", {}).get("name") in expected}
deployments = {item["metadata"]["name"] for item in resources if item.get("kind") == "Deployment" and item.get("metadata", {}).get("name") in expected}
rules = {item["metadata"]["name"] for item in resources if item.get("kind") == "DestinationRule" and item.get("metadata", {}).get("name") in expected}
if services != expected or deployments != expected or rules != expected:
    raise SystemExit("rendered source does not contain the expected pair of payload Services, Deployments, and DestinationRules")
tenant_services = {f"{service}-{pool}" for pool in ("pool-a", "pool-b") for service in expected}
expected_certificate_names = {f"{name}-tls" for name in expected | tenant_services}
if len(certificates) != 6 or {item.get("metadata", {}).get("name") for item in certificates} != expected_certificate_names:
    raise SystemExit("renderer did not produce exactly the two shared and four tenant-pool payload Certificates")
for cert in certificates:
    service = cert["metadata"]["name"][:-4]
    if cert["spec"]["dnsNames"] != [f"{service}.maas-system.svc.cluster.local"]:
        raise SystemExit(f"Certificate/{cert['metadata']['name']} has an unexpected DNS identity")
    if cert.get("metadata", {}).get("labels", {}).get("praxis.kind/run") != "renderer-contract-20261107":
        raise SystemExit(f"Certificate/{cert['metadata']['name']} lacks test run ownership")
    if cert["spec"].get("secretTemplate", {}).get("labels", {}).get("praxis.kind/run") != "renderer-contract-20261107":
        raise SystemExit(f"Certificate/{cert['metadata']['name']} does not propagate run ownership to its Secret")
summary = {
    "status": "PASS",
    "sourceRevision": source_sha,
    "sourceManifestSha256": bundle["metadata"]["annotations"]["praxis.kind/source-manifest-sha256"],
    "renderedResources": len(resources),
    "payloadServices": sorted(services),
    "payloadDeployments": sorted(deployments),
    "payloadDestinationRules": sorted(rules),
    "payloadCertificates": sorted(item["metadata"]["name"] for item in certificates),
    "tenantPoolCertificates": sorted(f"{service}-tls" for service in tenant_services),
    "testCaPrivateKeyRecorded": False,
}
summary_path.parent.mkdir(parents=True, exist_ok=True)
summary_path.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n")
PY
printf 'locked AIGC payload TLS manifest rendering passed; test CA material was temporary and is not retained\n'
