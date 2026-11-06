#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/build-kind-images.sh" "$SCRIPT_DIR/prepare-kind-image-sources.py" <<'PY'
import sys
from pathlib import Path

builder = Path(sys.argv[1]).read_text(encoding="utf-8")
preparer = Path(sys.argv[2]).read_text(encoding="utf-8")
required = (
    'PATCH="${MAAS_CONTROLLER_SOURCE_PATCH_FILE:-$SCRIPT_DIR/patches/maas-kind-cleanup-tls.patch}"',
    'prepare-kind-image-sources.py',
    '--source-locks "$SOURCE_LOCKS"',
    '--worktree-root "$worktree_root"',
    'plan_kind_images.py',
    'maas-api-build-locks.json',
)
if any(item not in builder for item in required):
    raise SystemExit("Kind image builder must prepare repository-local locked sources before validating all image inputs")
if "patch.repository_path" in preparer or "patch_file.read_text" in preparer:
    raise SystemExit("source preparation must record repository-relative patch provenance without changing patch bytes")
print("Kind image builder source preparation and packaged patch contract: PASS")
PY
