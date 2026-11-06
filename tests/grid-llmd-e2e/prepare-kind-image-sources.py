#!/usr/bin/env python3
"""Prepare the separately built MaaS API source and pinned Kind patch identity."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


PATCH_PREFIX = b"praxis-extproc-build-context-with-patch-v1\0"
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()


class PreparationError(ValueError):
    """Raised when source or patch identity does not match the run lock."""


def git(root: Path, *args: str, binary: bool = False) -> bytes | str:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise PreparationError(f"git operation failed (exit {result.returncode})")
    return result.stdout if binary else result.stdout.decode("utf-8", errors="strict").strip()


def archive_hash(root: Path, revision: str, subdir: str | None = None) -> str:
    args = ["archive", "--format=tar", revision]
    if subdir:
        args.append(subdir)
    archive = git(root, *args, binary=True)
    assert isinstance(archive, bytes)
    return hashlib.sha256(archive).hexdigest()


def source_record(record: dict, worktree: Path, revision: str, context_hash: str) -> dict:
    return {
        "remote_url": record["remote_url"],
        "requested_ref": record["requested_ref"],
        "resolved_sha": revision,
        "branch_state": "detached",
        "resolved_worktree_clean": True,
        "caller_worktree_state": "not-provided",
        "caller_branch_state": "not-provided",
        "caller_head": "not-provided",
        "caller_tracked_diff_sha256": EMPTY_SHA256,
        "caller_untracked_content_sha256": EMPTY_SHA256,
        "build_context_sha256": context_hash,
        "worktree_path": str(worktree),
    }


def prepare(source_locks: Path, worktree_root: Path, patch_file: Path, output_dir: Path) -> dict:
    lock = json.loads(source_locks.read_text(encoding="utf-8"))
    if lock.get("schema") != "praxis-extproc.source-locks-run/v1" or lock.get("scenario") != "grid-llmd-kind":
        raise PreparationError("input lock must be a Kind source-resolution record")
    maas = lock.get("components", {}).get("maas")
    if not isinstance(maas, dict):
        raise PreparationError("source lock is missing MaaS")
    maas_root = Path(maas["worktree_path"]).resolve()
    maas_sha = maas["resolved_sha"]
    if git(maas_root, "rev-parse", "HEAD") != maas_sha or git(maas_root, "branch", "--show-current"):
        raise PreparationError("MaaS source must be detached at its resolved commit")
    if git(maas_root, "status", "--porcelain", "--untracked-files=all"):
        raise PreparationError("MaaS source must be clean before applying the Kind patch")
    base_context = archive_hash(maas_root, maas_sha)
    if base_context != maas["build_context_sha256"]:
        raise PreparationError("MaaS archive differs from the resolved source lock")
    patch_bytes = patch_file.read_bytes()
    patch_sha = hashlib.sha256(patch_bytes).hexdigest()
    if not patch_bytes:
        raise PreparationError("MaaS patch is empty")
    try:
        subprocess.run(["git", "-C", str(maas_root), "apply", "--check", str(patch_file)], check=True, capture_output=True)
        subprocess.run(["git", "-C", str(maas_root), "apply", str(patch_file)], check=True, capture_output=True)
    except subprocess.CalledProcessError as error:
        raise PreparationError("Kind MaaS patch does not apply to the resolved source") from error
    # Full object IDs keep the rendered patch independent of repository-local
    # abbreviation settings and shallow-clone object availability.
    actual_diff = git(maas_root, "diff", "--binary", "--full-index", maas_sha, binary=True)
    assert isinstance(actual_diff, bytes)
    if hashlib.sha256(actual_diff).hexdigest() != patch_sha:
        raise PreparationError("applied MaaS diff is not byte-identical to the packaged patch")
    patched_context = hashlib.sha256(PATCH_PREFIX + bytes.fromhex(base_context) + bytes.fromhex(patch_sha)).hexdigest()
    maas.update(
        {
            "base_build_context_sha256": base_context,
            "source_patch_sha256": patch_sha,
            "patched_build_context_sha256": patched_context,
            "resolved_worktree_clean": False,
        }
    )
    source_locks.write_text(json.dumps(lock, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.chmod(source_locks, 0o600)

    maas_api_root = worktree_root / "maas-api-worktree"
    maas_api_root.parent.mkdir(parents=True, exist_ok=True)
    if maas_api_root.exists():
        raise PreparationError("MaaS API source worktree path already exists")
    git(maas_root, "worktree", "add", "--detach", str(maas_api_root), maas_sha)
    if git(maas_api_root, "status", "--porcelain", "--untracked-files=all"):
        raise PreparationError("MaaS API source worktree is not clean")
    api_context = archive_hash(maas_api_root, maas_sha)
    api_lock = {
        "schema": "praxis-extproc.source-locks/v1",
        "scenario": "grid-llmd-kind-maas-api",
        "source_mode": lock["source_mode"],
        "captured_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "components": {"maas": source_record(maas, maas_api_root, maas_sha, api_context)},
    }
    api_lock_path = output_dir / "maas-api-build-locks.json"
    api_lock_path.write_text(json.dumps(api_lock, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.chmod(api_lock_path, 0o600)

    identity = {
        "schema": "praxis-extproc.kind-maas-patch-identity/v1",
        "source_sha": maas_sha,
        "source_remote": maas["remote_url"],
        "patch_repository_path": "tests/grid-llmd-e2e/patches/maas-kind-cleanup-tls.patch",
        "source_patch_sha256": patch_sha,
        "applied_diff_sha256": hashlib.sha256(actual_diff).hexdigest(),
        "base_build_context_sha256": base_context,
        "patched_build_context_sha256": patched_context,
        "maas_api_build_context_sha256": api_context,
    }
    identity_path = output_dir / "maas-patch-identity.json"
    identity_path.write_text(json.dumps(identity, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.chmod(identity_path, 0o600)
    source_markdown = output_dir / "source-locks.md"
    with source_markdown.open("a", encoding="utf-8") as report:
        report.write(
            "\n## Kind build inputs\n\n"
            f"- MaaS controller source patch SHA-256: `{patch_sha}`\n"
            f"- MaaS controller patched build context SHA-256: `{patched_context}`\n"
            f"- MaaS API uses the same resolved commit in a separate clean worktree; lock: `{api_lock_path.name}`\n"
        )
    os.chmod(source_markdown, 0o600)
    return identity


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-locks", type=Path, required=True)
    parser.add_argument("--worktree-root", type=Path, required=True)
    parser.add_argument("--patch", type=Path, default=Path(__file__).with_name("patches") / "maas-kind-cleanup-tls.patch")
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    try:
        args.output_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        print(json.dumps(prepare(args.source_locks, args.worktree_root, args.patch, args.output_dir), indent=2, sort_keys=True))
        return 0
    except (OSError, json.JSONDecodeError, KeyError, PreparationError) as error:
        print(f"prepare-kind-image-sources: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
