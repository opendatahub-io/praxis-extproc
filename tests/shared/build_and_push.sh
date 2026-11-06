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
SOURCE_PATCH_FILE="${SOURCE_PATCH_FILE:-}"
BUILD_CONTEXT_SUBDIR="${BUILD_CONTEXT_SUBDIR:-}"
IMAGE_PUSH_TLS_VERIFY="${IMAGE_PUSH_TLS_VERIFY:-true}"
ALLOW_INSECURE_IMAGE_PUSH="${ALLOW_INSECURE_IMAGE_PUSH:-false}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
CONTAINER_BUILD_JOBS="${CONTAINER_BUILD_JOBS:-}"
CONTAINER_BUILD_CPU_QUOTA="${CONTAINER_BUILD_CPU_QUOTA:-}"
CONTAINER_BUILD_CPU_PERIOD="${CONTAINER_BUILD_CPU_PERIOD:-100000}"
CONTAINER_BUILD_MEMORY="${CONTAINER_BUILD_MEMORY:-}"

[[ "$COMPONENT" =~ ^[a-z0-9][a-z0-9-]*$ ]] || { printf 'COMPONENT is required and must be a safe identifier\n' >&2; exit 2; }
[[ -s "$SOURCE_LOCKS_FILE" && -n "$IMAGE_REPOSITORY" && -n "$RUN_ID" && -n "$EVIDENCE_DIR" ]] || {
  printf 'SOURCE_LOCKS_FILE, IMAGE_REPOSITORY, E2E_RUN_ID, and EVIDENCE_DIR are required\n' >&2
  exit 2
}
[[ "$RUN_ID" =~ ^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$ ]] || { printf 'E2E_RUN_ID is not a Kubernetes label value\n' >&2; exit 2; }
[[ "$IMAGE_PUSH_TLS_VERIFY" == true || "$IMAGE_PUSH_TLS_VERIFY" == false ]] || { printf 'IMAGE_PUSH_TLS_VERIFY must be true or false\n' >&2; exit 2; }
if [[ "$IMAGE_PUSH_TLS_VERIFY" == false ]]; then
  [[ ("$CLUSTER_ENV" == ephemeral-dev || "$CLUSTER_ENV" == kind) && "$ALLOW_INSECURE_IMAGE_PUSH" == true ]] || {
    printf 'disabling registry TLS verification requires CLUSTER_ENV=ephemeral-dev or kind and ALLOW_INSECURE_IMAGE_PUSH=true\n' >&2
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
print(record.get("source_patch_sha256") or "-")
PY
)"
mapfile -t fields <<<"$metadata"
(( ${#fields[@]} == 5 )) || { printf 'source lock is incomplete for %s\n' "$COMPONENT" >&2; exit 2; }
SOURCE_WORKTREE="${fields[0]}"
SOURCE_SHA="${fields[1]}"
SOURCE_URL="${fields[2]}"
BUILD_CONTEXT_SHA256="${fields[3]}"
LOCKED_SOURCE_PATCH_SHA256="${fields[4]}"
[[ "$LOCKED_SOURCE_PATCH_SHA256" == "-" ]] && LOCKED_SOURCE_PATCH_SHA256=""
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
base_context_sha="$(git -C "$SOURCE_WORKTREE" archive --format=tar "$SOURCE_SHA" | sha256sum | awk '{print $1}')"
if [[ -n "$LOCKED_SOURCE_PATCH_SHA256" ]]; then
  [[ "$LOCKED_SOURCE_PATCH_SHA256" =~ ^[0-9a-f]{64}$ && -f "$SOURCE_PATCH_FILE" ]] || {
    printf 'patched source lock requires SOURCE_PATCH_FILE and a valid source_patch_sha256\n' >&2
    exit 2
  }
  source_patch_sha256="$(sha256sum "$SOURCE_PATCH_FILE" | awk '{print $1}')"
  actual_diff_sha256="$(git -C "$SOURCE_WORKTREE" diff --binary "$SOURCE_SHA" | sha256sum | awk '{print $1}')"
  actual_untracked="$(git -C "$SOURCE_WORKTREE" ls-files --others --exclude-standard)"
  [[ "$source_patch_sha256" == "$LOCKED_SOURCE_PATCH_SHA256" && "$actual_diff_sha256" == "$LOCKED_SOURCE_PATCH_SHA256" && -z "$actual_untracked" && -n "$actual_status" ]] || {
    printf 'patched image source must match the locked source patch exactly and contain no untracked files\n' >&2
    exit 2
  }
  base_context_sha="$(python3 - "$SOURCE_LOCKS_FILE" "$SOURCE_COMPONENT" <<'PY'
import json, sys
from pathlib import Path
record = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["components"][sys.argv[2]]
print(record.get("base_build_context_sha256", record["build_context_sha256"]))
PY
)"
archived_context_sha="$(git -C "$SOURCE_WORKTREE" archive --format=tar "$SOURCE_SHA" | sha256sum | awk '{print $1}')"
[[ "$archived_context_sha" == "$base_context_sha" ]] || {
  printf 'patched source base archive does not match the source lock\n' >&2
  exit 2
}
BUILD_CONTEXT_SHA256="$(python3 - "$base_context_sha" "$source_patch_sha256" <<'PY'
import hashlib, sys
digest = hashlib.sha256(b"praxis-extproc-build-context-with-patch-v1\0")
digest.update(bytes.fromhex(sys.argv[1]))
digest.update(bytes.fromhex(sys.argv[2]))
print(digest.hexdigest())
PY
)"
LOCKED_PATCHED_CONTEXT_SHA256="$(python3 - "$SOURCE_LOCKS_FILE" "$SOURCE_COMPONENT" <<'PY'
import json, sys
from pathlib import Path
record = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["components"][sys.argv[2]]
print(record.get("patched_build_context_sha256", ""))
PY
)"
  [[ "$LOCKED_PATCHED_CONTEXT_SHA256" == "$BUILD_CONTEXT_SHA256" ]] || {
    printf 'patched source lock build-context identity does not match base archive plus patch\n' >&2
    exit 2
  }
else
  source_patch_sha256=""
  [[ -z "$SOURCE_PATCH_FILE" && -z "$actual_status" && "$base_context_sha" == "$BUILD_CONTEXT_SHA256" ]] || {
    printf 'image build source must be the exact clean detached worktree recorded in source-locks.json\n' >&2
    exit 2
  }
fi
[[ "$actual_head" == "$SOURCE_SHA" && -z "$actual_branch" ]] || {
  printf 'image build source must be detached at the exact resolved source revision\n' >&2
  exit 2
}

# Some image Dockerfiles expect their repository subdirectory to be the build
# context. Validate the root archive against the source lock, then hash and
# record the exact subtree archive used by the container engine.
SOURCE_LOCK_BUILD_CONTEXT_SHA256="$BUILD_CONTEXT_SHA256"
BUILD_CONTEXT_DIR="$SOURCE_WORKTREE"
if [[ -n "$BUILD_CONTEXT_SUBDIR" ]]; then
  [[ -z "$LOCKED_SOURCE_PATCH_SHA256" ]] || {
    printf 'BUILD_CONTEXT_SUBDIR cannot be combined with a patched source lock\n' >&2
    exit 2
  }
  python3 - "$SOURCE_WORKTREE" "$BUILD_CONTEXT_SUBDIR" <<'PY'
import sys
from pathlib import Path
root = Path(sys.argv[1]).resolve()
subdir = Path(sys.argv[2])
if subdir.is_absolute() or not subdir.parts or any(part in ("", ".", "..") for part in subdir.parts):
    raise SystemExit("BUILD_CONTEXT_SUBDIR must be a normalized relative directory")
target = (root / subdir).resolve()
if root not in target.parents or not target.is_dir():
    raise SystemExit("BUILD_CONTEXT_SUBDIR must identify a directory inside the locked source worktree")
PY
  BUILD_CONTEXT_DIR="$SOURCE_WORKTREE/$BUILD_CONTEXT_SUBDIR"
  BUILD_CONTEXT_SHA256="$(git -C "$SOURCE_WORKTREE" archive --format=tar "$SOURCE_SHA" "$BUILD_CONTEXT_SUBDIR" | sha256sum | awk '{print $1}')"
  [[ "$BUILD_CONTEXT_SHA256" =~ ^[0-9a-f]{64}$ ]] || { printf 'subtree build-context hash is invalid\n' >&2; exit 2; }
fi

mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"
tag="$IMAGE_REPOSITORY:e2e-$RUN_ID-${SOURCE_SHA:0:12}"
deploy_repository="${DEPLOY_IMAGE_REPOSITORY%@*}"
push_repository="${IMAGE_REPOSITORY%@*}"
[[ "$push_repository" != *'@'* && "$deploy_repository" != *'@'* ]] || { printf 'image repositories must not contain digest references\n' >&2; exit 2; }
inspect_file="$(mktemp "${TMPDIR:-/tmp}/praxis-image-inspect.XXXXXX")"
digest_file="$(mktemp "${TMPDIR:-/tmp}/praxis-image-digest.XXXXXX")"
docker_config_dir=""
cleanup_build_temps() {
  rm -f "$inspect_file" "$digest_file"
  if [[ -n "$docker_config_dir" ]]; then rm -rf "$docker_config_dir"; fi
}
trap cleanup_build_temps EXIT

build_args=(build)
if [[ -n "$BUILD_PLATFORM" ]]; then build_args+=(--platform "$BUILD_PLATFORM"); fi
if [[ -n "$CONTAINER_BUILD_JOBS" || -n "$CONTAINER_BUILD_CPU_QUOTA" || -n "$CONTAINER_BUILD_MEMORY" ]]; then
  [[ "$CONTAINER_ENGINE" == podman ]] || {
    printf 'resource-limited container builds require CONTAINER_ENGINE=podman; host/daemon CPU constraints are not inferred\n' >&2
    exit 2
  }
fi
if [[ -n "$CONTAINER_BUILD_JOBS" ]]; then
  [[ "$CONTAINER_BUILD_JOBS" =~ ^[1-9][0-9]*$ ]] || { printf 'CONTAINER_BUILD_JOBS must be a positive integer\n' >&2; exit 2; }
  build_args+=(--jobs "$CONTAINER_BUILD_JOBS")
fi
if [[ -n "$CONTAINER_BUILD_CPU_QUOTA" ]]; then
  [[ "$CONTAINER_BUILD_CPU_QUOTA" =~ ^[1-9][0-9]*$ && "$CONTAINER_BUILD_CPU_PERIOD" =~ ^[1-9][0-9]*$ ]] || {
    printf 'CONTAINER_BUILD_CPU_QUOTA and CONTAINER_BUILD_CPU_PERIOD must be positive integers\n' >&2; exit 2;
  }
  build_args+=(--cpu-quota "$CONTAINER_BUILD_CPU_QUOTA" --cpu-period "$CONTAINER_BUILD_CPU_PERIOD")
fi
if [[ -n "$CONTAINER_BUILD_MEMORY" ]]; then
  [[ "$CONTAINER_BUILD_MEMORY" =~ ^[1-9][0-9]*(b|k|m|g)$ ]] || { printf 'CONTAINER_BUILD_MEMORY must include a positive b/k/m/g unit\n' >&2; exit 2; }
  build_args+=(--memory "$CONTAINER_BUILD_MEMORY")
fi
if [[ -n "$REGISTRY_AUTH_FILE" && "$CONTAINER_ENGINE" == podman ]]; then
  build_args+=(--authfile "$REGISTRY_AUTH_FILE")
fi
if [[ "$COMPONENT" == grid-gateway || "$COMPONENT" == grid-operator ]]; then
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
  --tag "$tag" "$BUILD_CONTEXT_DIR"
)
if [[ -n "$BUILD_CONTEXT_SUBDIR" ]]; then
  build_args+=(--label "org.praxis.build-context.subdir=$BUILD_CONTEXT_SUBDIR")
fi
if [[ -n "$source_patch_sha256" ]]; then
  build_args+=(--label "org.praxis.source-patch.sha256=$source_patch_sha256")
fi
"$CONTAINER_ENGINE" "${build_args[@]}" >"$EVIDENCE_DIR/build-$COMPONENT.log" 2>&1
if [[ "$CONTAINER_ENGINE" == docker ]]; then
  # Docker's CLI has no --digestfile. Use its registry push and parse the
  # digest emitted by the daemon, then verify that immutable reference with
  # the registry inspector below. Docker registry trust must be configured
  # in the daemon; a Podman-style per-registry cert directory is not honored.
  [[ -z "$REGISTRY_CERT_DIR" ]] || {
    printf 'REGISTRY_CERT_DIR is not supported by CONTAINER_ENGINE=docker; configure Docker daemon trust or use podman\n' >&2
    exit 2
  }
  push_output="$EVIDENCE_DIR/push-$COMPONENT.log"
  if [[ -n "$REGISTRY_AUTH_FILE" ]]; then
    docker_config_dir="$(mktemp -d "${TMPDIR:-/tmp}/praxis-docker-config.XXXXXX")"
    chmod 0700 "$docker_config_dir"
    install -m 0600 "$REGISTRY_AUTH_FILE" "$docker_config_dir/config.json"
    DOCKER_CONFIG="$docker_config_dir" "$CONTAINER_ENGINE" push "$tag" >"$push_output" 2>&1
  else
    "$CONTAINER_ENGINE" push "$tag" >"$push_output" 2>&1
  fi
  digest="$(sed -nE 's/.*digest: (sha256:[0-9a-f]{64}).*/\1/p' "$push_output" | tail -n 1)"
  [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || {
    printf 'Docker push did not report an immutable sha256 digest; see %s\n' "$push_output" >&2
    exit 1
  }
  printf '%s' "$digest" >"$digest_file"
else
  push_args=(push --digestfile "$digest_file")
  if [[ -n "$REGISTRY_AUTH_FILE" ]]; then push_args+=(--authfile "$REGISTRY_AUTH_FILE"); fi
  if [[ -n "$REGISTRY_CERT_DIR" ]]; then
    push_args+=(--cert-dir "$REGISTRY_CERT_DIR/${push_repository%%/*}")
  fi
  if [[ "$IMAGE_PUSH_TLS_VERIFY" == false ]]; then push_args+=(--tls-verify=false); fi
  "$CONTAINER_ENGINE" "${push_args[@]}" "$tag" >"$EVIDENCE_DIR/push-$COMPONENT.log" 2>&1
  digest="$(<"$digest_file")"
fi
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || { printf 'registry did not return an immutable sha256 digest\n' >&2; exit 1; }
image_ref="$deploy_repository@$digest"
inspect_ref="$push_repository@$digest"
inspect_args=(inspect --config)
if [[ -n "$REGISTRY_AUTH_FILE" ]]; then inspect_args+=(--authfile "$REGISTRY_AUTH_FILE"); fi
if [[ -n "$REGISTRY_CERT_DIR" ]]; then inspect_args+=(--cert-dir "$REGISTRY_CERT_DIR/${push_repository%%/*}"); fi
if [[ "$IMAGE_PUSH_TLS_VERIFY" == false ]]; then inspect_args+=(--tls-verify=false); fi
"$INSPECTOR" "${inspect_args[@]}" "docker://$inspect_ref" >"$inspect_file"
verify_args=(
  --image "$image_ref"
  --source-url "$OCI_SOURCE_URL"
  --source-sha "$SOURCE_SHA"
  --build-context-sha256 "$BUILD_CONTEXT_SHA256"
  --inspect-config "$inspect_file"
  --output "$EVIDENCE_DIR/image-provenance-$COMPONENT.json"
)
if [[ -n "$BUILD_CONTEXT_SUBDIR" ]]; then verify_args+=(--build-context-subdir "$BUILD_CONTEXT_SUBDIR"); fi
if [[ -n "$source_patch_sha256" ]]; then verify_args+=(--source-patch-sha256 "$source_patch_sha256"); fi
python3 "$SCRIPT_DIR/verify_image_provenance.py" "${verify_args[@]}" >&2
containerfile_sha256="$(sha256sum "$containerfile" | awk '{print $1}')"
python3 - "$EVIDENCE_DIR/build-metadata-$COMPONENT.json" "$COMPONENT" "$SOURCE_URL" "$OCI_SOURCE_URL" "$SOURCE_SHA" "$SOURCE_LOCK_BUILD_CONTEXT_SHA256" "$BUILD_CONTEXT_SHA256" "$BUILD_CONTEXT_SUBDIR" "$containerfile_sha256" "$BUILD_PLATFORM" "$tag" "$inspect_ref" "$image_ref" "$IMAGE_PUSH_TLS_VERIFY" "$REGISTRY_CERT_DIR" "$source_patch_sha256" "$CONTAINER_BUILD_JOBS" "$CONTAINER_BUILD_CPU_QUOTA" "$CONTAINER_BUILD_CPU_PERIOD" "$CONTAINER_BUILD_MEMORY" <<'PY'
import json, os, sys
path, component, source, oci_source, revision, source_lock_context, context, context_subdir, containerfile_sha, platform, build_tag, pushed_ref, image, tls_verify, cert_dir, source_patch, jobs, cpu_quota, cpu_period, memory = sys.argv[1:]
with open(path, "x", encoding="utf-8") as output:
    json.dump({
        "schema": "praxis-extproc.image-build/v1",
        "component": component,
        "source_url": source,
        "oci_source_url": oci_source,
        "source_sha": revision,
        "source_patch_sha256": source_patch or None,
        "source_lock_build_context_sha256": source_lock_context,
        "build_context_sha256": context,
        "build_context_subdir": context_subdir or None,
        "containerfile_sha256": containerfile_sha,
        "build_platform": platform,
        "extproc_cargo_features": os.environ.get("EXT_PROC_CARGO_FEATURES") or None,
        "build_tag_for_push_only": build_tag,
        "registry_reference_inspected": pushed_ref,
        "deployed_image": image,
        "registry_tls_verified": tls_verify == "true",
        "registry_trust_bundle_supplied": bool(cert_dir),
        "container_build_limits": {
            "jobs": int(jobs) if jobs else None,
            "cpu_quota": int(cpu_quota) if cpu_quota else None,
            "cpu_period": int(cpu_period) if cpu_quota else None,
            "memory": memory or None,
        },
        "credentials_written_to_evidence": False,
    }, output, indent=2, sort_keys=True)
    output.write("\n")
os.chmod(path, 0o600)
PY
printf '%s\n' "$image_ref"
