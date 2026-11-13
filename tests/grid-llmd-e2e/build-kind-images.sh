#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build the complete source-locked Kind image set after validating every build context.
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
RUN_ID="${E2E_RUN_ID:?E2E_RUN_ID is required}"
RUN_DIR="${GRID_LLMD_RUN_DIR:-}"
IMAGE_NAMESPACE="${KIND_IMAGE_NAMESPACE:-qualification}"
IMAGE_PUSH_TLS_VERIFY="${IMAGE_PUSH_TLS_VERIFY:-false}"
ALLOW_INSECURE_IMAGE_PUSH="${ALLOW_INSECURE_IMAGE_PUSH:-true}"
CAPACITY_PROFILE="${KIND_CAPACITY_PROFILE:-full}"
case "$CAPACITY_PROFILE" in
  full)
    BUILD_LIMIT_ARGS=()
    ;;
  gha-standard-4cpu)
    [[ "${CONTAINER_ENGINE:-podman}" == podman ]] || {
      printf 'gha-standard-4cpu requires Podman so per-build CPU/memory limits are explicit\n' >&2
      exit 2
    }
    BUILD_LIMIT_ARGS=(CONTAINER_BUILD_JOBS=1 CONTAINER_BUILD_CPU_QUOTA=400000 CONTAINER_BUILD_CPU_PERIOD=100000 CONTAINER_BUILD_MEMORY=6g)
    ;;
  *) printf 'KIND_CAPACITY_PROFILE must be full or gha-standard-4cpu\n' >&2; exit 2 ;;
esac
[[ -n "$RUN_DIR" && -d "$RUN_DIR/source-resolution" ]] || {
  printf 'GRID_LLMD_RUN_DIR must name this run evidence root with source-resolution/\n' >&2
  exit 2
}
[[ "$IMAGE_NAMESPACE" =~ ^[a-z0-9]+([.-][a-z0-9]+)*$ ]] || { printf 'KIND_IMAGE_NAMESPACE is invalid\n' >&2; exit 2; }

PATCH="${MAAS_CONTROLLER_SOURCE_PATCH_FILE:-$SCRIPT_DIR/patches/maas-kind-cleanup-tls.patch}"
BOOTSTRAP="$RUN_DIR/cluster/bootstrap"
REGISTRY_NAME="$(<"$BOOTSTRAP/registry-name.txt")"
PUSH_REGISTRY="127.0.0.1:$(<"$BOOTSTRAP/registry-port.txt")"
DEPLOY_REGISTRY="$REGISTRY_NAME:5000"
PLAN="$RUN_DIR/images/build-plan.json"
mkdir -p "$RUN_DIR/images"
chmod 0700 "$RUN_DIR/images"
[[ ! -e "$PLAN" ]] || { printf 'image build plan already exists; refusing overwrite\n' >&2; exit 2; }
[[ "$REGISTRY_NAME" =~ ^[a-z0-9][a-z0-9-]{2,50}$ && "$PUSH_REGISTRY" =~ ^127\.0\.0\.1:[0-9]+$ ]] || {
  printf 'run registry identity is malformed\n' >&2
  exit 2
}

SOURCE_LOCKS="$RUN_DIR/source-resolution/source-locks.json"
[[ -s "$SOURCE_LOCKS" ]] || { printf 'Kind source lock is missing: %s\n' "$SOURCE_LOCKS" >&2; exit 2; }
patch_recorded="$(python3 - "$SOURCE_LOCKS" <<'PY'
import json, sys
from pathlib import Path
maas = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["components"].get("maas", {})
print("yes" if maas.get("source_patch_sha256") else "no")
PY
)"
if [[ "$patch_recorded" == no ]]; then
  [[ ! -e "$RUN_DIR/source-resolution/kind-image-inputs.json" ]] || {
    printf 'Kind image source preparation evidence already exists; refusing to overwrite\n' >&2
    exit 2
  }
  worktree_root="$(python3 - "$SOURCE_LOCKS" <<'PY'
import json, sys
from pathlib import Path
maas = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))["components"]["maas"]
print(Path(maas["worktree_path"]).resolve().parent.parent)
PY
)"
  python3 "$SCRIPT_DIR/prepare-kind-image-sources.py" \
    --source-locks "$SOURCE_LOCKS" \
    --worktree-root "$worktree_root" \
    --patch "$PATCH" \
    --output-dir "$RUN_DIR/source-resolution" \
    >"$RUN_DIR/source-resolution/kind-image-inputs.json"
  chmod 0600 "$RUN_DIR/source-resolution/kind-image-inputs.json"
elif [[ ! -s "$RUN_DIR/source-resolution/maas-api-build-locks.json" ]]; then
  printf 'source lock records a MaaS patch but its clean API build lock is missing; preserve evidence and start a fresh source resolution\n' >&2
  exit 2
fi

# Validate every locked context and Containerfile before creating the first image.
python3 "$SCRIPT_DIR/plan_kind_images.py" --lock-dir "$RUN_DIR/source-resolution" --maas-patch "$PATCH" >"$PLAN"
chmod 0600 "$PLAN"

while IFS=$'\t' read -r component source lock_name dockerfile subdir; do
  [[ -n "$component" ]] || continue
  locks="$RUN_DIR/source-resolution/$lock_name"
  image_repository="$PUSH_REGISTRY/$IMAGE_NAMESPACE/$component"
  deploy_repository="$DEPLOY_REGISTRY/$IMAGE_NAMESPACE/$component"
  evidence="$RUN_DIR/images/$component"
  args=(
    COMPONENT="$component" SOURCE_COMPONENT="$source" SOURCE_LOCKS_FILE="$locks"
    IMAGE_REPOSITORY="$image_repository" DEPLOY_IMAGE_REPOSITORY="$deploy_repository"
    DOCKERFILE="$dockerfile" E2E_RUN_ID="$RUN_ID" EVIDENCE_DIR="$evidence"
    CONTAINER_ENGINE="${CONTAINER_ENGINE:-podman}" IMAGE_INSPECTOR="${IMAGE_INSPECTOR:-skopeo}"
    IMAGE_PUSH_TLS_VERIFY="$IMAGE_PUSH_TLS_VERIFY" ALLOW_INSECURE_IMAGE_PUSH="$ALLOW_INSECURE_IMAGE_PUSH"
    CLUSTER_ENV=kind BUILD_PLATFORM="${BUILD_PLATFORM:-linux/amd64}"
  )
  if ((${#BUILD_LIMIT_ARGS[@]})); then args+=("${BUILD_LIMIT_ARGS[@]}"); fi
  [[ -z "$subdir" ]] || args+=(BUILD_CONTEXT_SUBDIR="$subdir")
  if [[ "$component" == maas-controller ]]; then args+=(SOURCE_PATCH_FILE="$PATCH"); fi
  if [[ "$component" == extproc ]]; then
    args+=(EXT_PROC_CARGO_FEATURES="${EXT_PROC_CARGO_FEATURES:-responses,aws-sigv4,store-postgres-cert-auth,k8s-e2e}")
  fi
  mkdir -p "$evidence"
  chmod 0700 "$evidence"
  [[ ! -e "$evidence/image-ref.txt" ]] || { printf 'image reference already exists for %s\n' "$component" >&2; exit 2; }
  printf 'Building %s from locked source %s\n' "$component" "$source"
  image_ref="$(env "${args[@]}" bash "$REPO_ROOT/tests/shared/build_and_push.sh")"
  printf '%s\n' "$image_ref" >"$evidence/image-ref.txt"
  printf '%s\n' "$image_ref"
done < <(jq -r '.images[] | [.image_component,.source_component,.source_lock,.dockerfile,.build_context_subdir] | @tsv' "$PLAN")

printf '{"status":"PASS","images":%s}\n' "$(jq '.images|length' "$PLAN")" >"$RUN_DIR/images/RESULT.json"
printf '%s\n' "$CAPACITY_PROFILE" >"$RUN_DIR/images/capacity-profile.txt"
chmod 0600 "$RUN_DIR/images/RESULT.json"
