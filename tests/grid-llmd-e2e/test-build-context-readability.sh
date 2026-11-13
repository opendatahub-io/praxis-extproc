#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/praxis-build-context-permissions.XXXXXX")"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
SOURCE_REPO="$TEST_ROOT/source-repo"
WORKTREE_ROOT="$TEST_ROOT/private-worktrees"
SOURCE_WORKTREE="$WORKTREE_ROOT/locked-source"
BIN_DIR="$TEST_ROOT/bin"
mkdir -p "$SOURCE_REPO" "$WORKTREE_ROOT" "$BIN_DIR"
chmod 0700 "$TEST_ROOT" "$SOURCE_REPO" "$WORKTREE_ROOT" "$BIN_DIR"
git -C "$SOURCE_REPO" init -q
git -C "$SOURCE_REPO" config user.name "Kind Harness Test"
git -C "$SOURCE_REPO" config user.email "kind-harness-test@example.invalid"
printf 'FROM scratch\n' >"$SOURCE_REPO/Containerfile"
printf 'module probe\n' >"$SOURCE_REPO/go.mod"
git -C "$SOURCE_REPO" add Containerfile go.mod
git -C "$SOURCE_REPO" commit -qm "test locked source"
SOURCE_SHA="$(git -C "$SOURCE_REPO" rev-parse HEAD)"
git -C "$SOURCE_REPO" worktree add -q --detach "$SOURCE_WORKTREE" "$SOURCE_SHA"
[[ "$(stat -c '%a' "$SOURCE_WORKTREE/go.mod")" == 600 ]]
[[ "$(stat -c '%a' "$SOURCE_WORKTREE")" == 700 ]]

SOURCE_LOCKS_FILE="$TEST_ROOT/source-locks.json"
BUILD_CONTEXT_SHA256="$(git -C "$SOURCE_WORKTREE" archive --format=tar "$SOURCE_SHA" | sha256sum | awk '{print $1}')"
python3 - "$SOURCE_LOCKS_FILE" "$SOURCE_WORKTREE" "$SOURCE_SHA" "$BUILD_CONTEXT_SHA256" <<'PY'
import json, sys
path, worktree, revision, context = sys.argv[1:]
with open(path, "w", encoding="utf-8") as output:
    json.dump({"components": {"probe": {
        "worktree_path": worktree,
        "resolved_sha": revision,
        "remote_url": "https://example.invalid/probe.git",
        "build_context_sha256": context,
    }}}, output)
PY

cat >"$BIN_DIR/podman" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
[[ "$1" == build ]]
context="${@: -1}"
python3 - "$context" "$BUILD_CONTEXT_PERMISSION_CHECK" <<'PY'
import os, stat, sys
from pathlib import Path
root = Path(sys.argv[1])
record = Path(sys.argv[2])
root_mode = stat.S_IMODE(root.stat().st_mode)
file_mode = stat.S_IMODE((root / "go.mod").stat().st_mode)
gitfile_mode = stat.S_IMODE((root / ".git").stat().st_mode)
if root_mode & 0o055 != 0o055 or file_mode & 0o044 != 0o044:
    raise SystemExit("container build context is still unreadable to a non-root image builder")
if gitfile_mode & 0o077:
    raise SystemExit("build-context preparation changed the worktree Git metadata permissions")
record.write_text(f"root={root_mode:o} go.mod={file_mode:o} .git={gitfile_mode:o}\n", encoding="utf-8")
PY
exit 79
SH
cat >"$BIN_DIR/skopeo" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod 0700 "$BIN_DIR/podman" "$BIN_DIR/skopeo"

set +e
PATH="$BIN_DIR:$PATH" \
COMPONENT=probe SOURCE_COMPONENT=probe \
SOURCE_LOCKS_FILE="$SOURCE_LOCKS_FILE" \
IMAGE_REPOSITORY=localhost/probe \
E2E_RUN_ID=kind-permission-test \
EVIDENCE_DIR="$TEST_ROOT/evidence" \
DOCKERFILE=Containerfile \
CONTAINER_ENGINE=podman IMAGE_INSPECTOR=skopeo \
OCI_SOURCE_URL=https://example.invalid/probe.git \
BUILD_CONTEXT_PERMISSION_CHECK="$TEST_ROOT/permission-check.txt" \
  bash "$REPO_ROOT/tests/shared/build_and_push.sh" >/dev/null 2>&1
build_status=$?
set -e
[[ "$build_status" == 79 ]] || {
  printf 'expected the permission-checking fake builder to stop with 79, got %s\n' "$build_status" >&2
  exit 1
}
grep -Eq '^root=755 go\.mod=644 \.git=600$' "$TEST_ROOT/permission-check.txt"
printf 'locked build context is readable to non-root image builders without changing .git permissions\n'
