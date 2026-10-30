#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build one source-locked image and emit only its immutable reference/provenance.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
COMPONENT="${COMPONENT:-}"
SOURCE_COMPONENT="${SOURCE_COMPONENT:-$COMPONENT}"
SOURCE_LOCKS_FILE="${SOURCE_LOCKS_FILE:-}"
IMAGE_REPOSITORY="${IMAGE_REPOSITORY:-}"
RUN_ID="${E2E_RUN_ID:-}"
EVIDENCE_DIR="${EVIDENCE_DIR:-}"
DOCKERFILE="${DOCKERFILE:-Containerfile}"
CONTAINERFILE_OVERRIDE="${CONTAINERFILE_OVERRIDE:-}"
BUILD_PLATFORM="${BUILD_PLATFORM:-linux/amd64}"
CONTAINER_ENGINE="${CONTAINER_ENGINE:-podman}"
INSPECTOR="${IMAGE_INSPECTOR:-skopeo}"
DEPLOY_IMAGE_REPOSITORY="${DEPLOY_IMAGE_REPOSITORY:-$IMAGE_REPOSITORY}"
REGISTRY_AUTH_FILE="${REGISTRY_AUTH_FILE:-}"
REGISTRY_CERT_DIR="${REGISTRY_CERT_DIR:-}"
OCI_SOURCE_URL="${OCI_SOURCE_URL:-}"
EXT_PROC_CARGO_FEATURES="${EXT_PROC_CARGO_FEATURES:-}"
IMAGE_PUSH_TLS_VERIFY="${IMAGE_PUSH_TLS_VERIFY:-true}"
ALLOW_INSECURE_IMAGE_PUSH="${ALLOW_INSECURE_IMAGE_PUSH:-false}"
CLUSTER_ENV="${CLUSTER_ENV:-}"

[[ "$COMPONENT" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { printf 'COMPONENT is required and must be a safe identifier\n' >&2; exit 2; }
[[ -s "$SOURCE_LOCKS_FILE" && -n "$IMAGE_REPOSITORY" && -n "$RUN_ID" && -n "$EVIDENCE_DIR" ]] || {
  printf 'SOURCE_LOCKS_FILE, IMAGE_REPOSITORY, E2E_RUN_ID, and EVIDENCE_DIR are required\n' >&2
  exit 2
}
[[ "$RUN_ID" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'E2E_RUN_ID is not a Kubernetes label value\n' >&2; exit 2; }
[[ "$IMAGE_PUSH_TLS_VERIFY" == true || "$IMAGE_PUSH_TLS_VERIFY" == false ]] || { printf 'IMAGE_PUSH_TLS_VERIFY must be true or false\n' >&2; exit 2; }
if [[ "$IMAGE_PUSH_TLS_VERIFY" == false ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_IMAGE_PUSH" == true ]] || {
    printf 'disabling registry TLS verification requires CLUSTER_ENV=ephemeral-dev and ALLOW_INSECURE_IMAGE_PUSH=true\n' >&2
    exit 2
  }
fi
if [[ -n "$REGISTRY_AUTH_FILE" ]]; then
  [[ -f "$REGISTRY_AUTH_FILE" ]] || { printf 'REGISTRY_AUTH_FILE must refer to a file outside evidence\n' >&2; exit 2; }
  [[ "$REGISTRY_AUTH_FILE" != "$EVIDENCE_DIR"/* ]] || { printf 'registry credentials must not be stored in evidence\n' >&2; exit 2; }
  auth_mode="$(stat -c '%a' "$REGISTRY_AUTH_FILE")"
  [[ "$auth_mode" == 600 || "$auth_mode" == 400 ]] || { printf 'REGISTRY_AUTH_FILE must have mode 0600 or 0400\n' >&2; exit 2; }
fi
if [[ -n "$REGISTRY_CERT_DIR" ]]; then
  [[ -d "$REGISTRY_CERT_DIR" ]] || { printf 'REGISTRY_CERT_DIR must refer to a directory containing registry TLS trust material\n' >&2; exit 2; }
  [[ "$IMAGE_PUSH_TLS_VERIFY" == true ]] || { printf 'REGISTRY_CERT_DIR cannot be combined with disabled registry TLS verification\n' >&2; exit 2; }
fi
if [[ -n "$EXT_PROC_CARGO_FEATURES" ]]; then
  [[ "$COMPONENT" == extproc ]] || { printf 'EXT_PROC_CARGO_FEATURES is only supported for COMPONENT=extproc\n' >&2; exit 2; }
  [[ "$EXT_PROC_CARGO_FEATURES" =~ ^[A-Za-z0-9_-]+(,[A-Za-z0-9_-]+)*$ ]] || {
    printf 'EXT_PROC_CARGO_FEATURES must be a comma-separated list of Cargo feature names\n' >&2
    exit 2
  }
fi
command -v "$CONTAINER_ENGINE" >/dev/null || { printf '%s is required\n' "$CONTAINER_ENGINE" >&2; exit 2; }
command -v "$INSPECTOR" >/dev/null || { printf '%s is required\n' "$INSPECTOR" >&2; exit 2; }
command -v python3 >/dev/null || { printf 'python3 is required\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR/image-provenance-$COMPONENT.json" ]] || { printf 'image provenance evidence already exists\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR/build-metadata-$COMPONENT.json" ]] || { printf 'image build metadata evidence already exists\n' >&2; exit 2; }

metadata="$(python3 - "$SOURCE_LOCKS_FILE" "$SOURCE_COMPONENT" <<'PY'
import json
import sys
from pathlib import Path

components = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8")).get("components", {})
record = components.get(sys.argv[2])
if record is None:
    raise SystemExit(f"source lock is missing component {sys.argv[2]!r}")
print(record["worktree_path"])
print(record["resolved_sha"])
print(record["remote_url"])
print(record["build_context_sha256"])
PY
)"
mapfile -t fields <<<"$metadata"
(( ${#fields[@]} == 4 )) || { printf 'source lock is incomplete for %s\n' "$COMPONENT" >&2; exit 2; }
SOURCE_WORKTREE="${fields[0]}"
SOURCE_SHA="${fields[1]}"
SOURCE_URL="${fields[2]}"
BUILD_CONTEXT_SHA256="${fields[3]}"
if [[ -z "$OCI_SOURCE_URL" ]]; then OCI_SOURCE_URL="$SOURCE_URL"; fi
[[ "$OCI_SOURCE_URL" =~ ^https://[^[:space:]]+$ ]] || { printf 'OCI_SOURCE_URL must be an HTTPS source URL without whitespace\n' >&2; exit 2; }
[[ -d "$SOURCE_WORKTREE" ]] || { printf 'resolved build context is missing\n' >&2; exit 2; }
containerfile="$SOURCE_WORKTREE/$DOCKERFILE"
if [[ -n "$CONTAINERFILE_OVERRIDE" ]]; then containerfile="$CONTAINERFILE_OVERRIDE"; fi
[[ -f "$containerfile" ]] || { printf 'resolved build context or Containerfile is missing\n' >&2; exit 2; }
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{40}$ && "$BUILD_CONTEXT_SHA256" =~ ^[0-9a-f]{64}$ ]] || { printf 'resolved source provenance is malformed\n' >&2; exit 2; }
[[ "$BUILD_PLATFORM" =~ ^linux/(amd64|arm64)$ ]] || { printf 'BUILD_PLATFORM must be linux/amd64 or linux/arm64\n' >&2; exit 2; }
actual_head="$(git -C "$SOURCE_WORKTREE" rev-parse HEAD)"
actual_branch="$(git -C "$SOURCE_WORKTREE" branch --show-current)"
actual_status="$(git -C "$SOURCE_WORKTREE" status --porcelain --untracked-files=all)"
actual_context_sha="$(git -C "$SOURCE_WORKTREE" archive --format=tar "$SOURCE_SHA" | sha256sum | awk '{print $1}')"
[[ "$actual_head" == "$SOURCE_SHA" && -z "$actual_branch" && -z "$actual_status" && "$actual_context_sha" == "$BUILD_CONTEXT_SHA256" ]] || {
  printf 'image build source must be the exact clean detached worktree recorded in source-locks.json\n' >&2
  exit 2
}

mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
tag="$IMAGE_REPOSITORY:e2e-$RUN_ID-${SOURCE_SHA:0:12}"
deploy_repository="${DEPLOY_IMAGE_REPOSITORY%@*}"
push_repository="${IMAGE_REPOSITORY%@*}"
[[ "$push_repository" != *'@'* && "$deploy_repository" != *'@'* ]] || { printf 'image repositories must not contain digest references\n' >&2; exit 2; }
inspect_file="$(mktemp "${TMPDIR:-/tmp}/praxis-image-inspect.XXXXXX")"
digest_file="$(mktemp "${TMPDIR:-/tmp}/praxis-image-digest.XXXXXX")"
trap 'rm -f "$inspect_file" "$digest_file"' EXIT

build_args=(build)
if [[ -n "$BUILD_PLATFORM" ]]; then build_args+=(--platform "$BUILD_PLATFORM"); fi
if [[ -n "$REGISTRY_AUTH_FILE" && "$CONTAINER_ENGINE" == podman ]]; then
  build_args+=(--authfile "$REGISTRY_AUTH_FILE")
fi
if [[ "$COMPONENT" == grid-gateway ]]; then
  version="$(git -C "$SOURCE_WORKTREE" describe --tags --always 2>/dev/null || printf unknown)"
  build_args+=(
    --build-arg "GRID_GIT_COMMIT=$SOURCE_SHA"
    --build-arg "GRID_GIT_VERSION=$version"
    --build-arg GRID_GIT_TREE_STATE=clean
    --build-arg "GRID_BUILD_DATE=$(date -u +%Y%m%d)"
  )
fi
if [[ "$COMPONENT" == epp ]]; then
  build_args+=(
    --build-arg "COMMIT_SHA=$SOURCE_SHA"
    --build-arg "BUILD_REF=e2e-$RUN_ID"
  )
fi
if [[ "$COMPONENT" == extproc && -n "$EXT_PROC_CARGO_FEATURES" ]]; then
  build_args+=(--build-arg "CARGO_FEATURES=$EXT_PROC_CARGO_FEATURES")
fi
build_args+=(
  --file "$containerfile"
  --label "org.opencontainers.image.source=$OCI_SOURCE_URL" \
  --label "org.opencontainers.image.revision=$SOURCE_SHA" \
  --label "org.praxis.build-context.sha256=$BUILD_CONTEXT_SHA256" \
  --tag "$tag" "$SOURCE_WORKTREE"
)
"$CONTAINER_ENGINE" "${build_args[@]}" >"$EVIDENCE_DIR/build-$COMPONENT.log" 2>&1
push_args=(push --digestfile "$digest_file")
if [[ -n "$REGISTRY_AUTH_FILE" ]]; then push_args+=(--authfile "$REGISTRY_AUTH_FILE"); fi
if [[ -n "$REGISTRY_CERT_DIR" && "$CONTAINER_ENGINE" == podman ]]; then
  push_args+=(--cert-dir "$REGISTRY_CERT_DIR/${push_repository%%/*}")
fi
if [[ "$IMAGE_PUSH_TLS_VERIFY" == false && "$CONTAINER_ENGINE" == podman ]]; then push_args+=(--tls-verify=false); fi
"$CONTAINER_ENGINE" "${push_args[@]}" "$tag" >"$EVIDENCE_DIR/push-$COMPONENT.log" 2>&1
digest="$(<"$digest_file")"
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || { printf 'registry did not return an immutable sha256 digest\n' >&2; exit 1; }
image_ref="$deploy_repository@$digest"
inspect_ref="$push_repository@$digest"
inspect_args=(inspect --config)
if [[ -n "$REGISTRY_AUTH_FILE" ]]; then inspect_args+=(--authfile "$REGISTRY_AUTH_FILE"); fi
if [[ -n "$REGISTRY_CERT_DIR" ]]; then inspect_args+=(--cert-dir "$REGISTRY_CERT_DIR/${push_repository%%/*}"); fi
if [[ "$IMAGE_PUSH_TLS_VERIFY" == false ]]; then inspect_args+=(--tls-verify=false); fi
"$INSPECTOR" "${inspect_args[@]}" "docker://$inspect_ref" >"$inspect_file"
python3 "$SCRIPT_DIR/verify_image_provenance.py" \
  --image "$image_ref" \
  --source-url "$OCI_SOURCE_URL" \
  --source-sha "$SOURCE_SHA" \
  --build-context-sha256 "$BUILD_CONTEXT_SHA256" \
  --inspect-config "$inspect_file" \
  --output "$EVIDENCE_DIR/image-provenance-$COMPONENT.json" >&2
containerfile_sha256="$(sha256sum "$containerfile" | awk '{print $1}')"
python3 - "$EVIDENCE_DIR/build-metadata-$COMPONENT.json" "$COMPONENT" "$SOURCE_URL" "$OCI_SOURCE_URL" "$SOURCE_SHA" "$BUILD_CONTEXT_SHA256" "$containerfile_sha256" "$BUILD_PLATFORM" "$tag" "$inspect_ref" "$image_ref" "$IMAGE_PUSH_TLS_VERIFY" "$REGISTRY_CERT_DIR" <<'PY'
import json, os, sys
path, component, source, oci_source, revision, context, containerfile_sha, platform, build_tag, pushed_ref, image, tls_verify, cert_dir = sys.argv[1:]
with open(path, "x", encoding="utf-8") as output:
    json.dump({
        "schema": "praxis-extproc.image-build/v1",
        "component": component,
        "source_url": source,
        "oci_source_url": oci_source,
        "source_sha": revision,
        "build_context_sha256": context,
        "containerfile_sha256": containerfile_sha,
        "build_platform": platform,
        "extproc_cargo_features": os.environ.get("EXT_PROC_CARGO_FEATURES") or None,
        "build_tag_for_push_only": build_tag,
        "registry_reference_inspected": pushed_ref,
        "deployed_image": image,
        "registry_tls_verified": tls_verify == "true",
        "registry_trust_bundle_supplied": bool(cert_dir),
        "credentials_written_to_evidence": False,
    }, output, indent=2, sort_keys=True)
    output.write("\n")
os.chmod(path, 0o600)
PY
printf '%s\n' "$image_ref"
