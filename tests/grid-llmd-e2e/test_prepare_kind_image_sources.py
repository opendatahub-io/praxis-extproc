#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import hashlib
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("prepare-kind-image-sources.py")
SPEC = importlib.util.spec_from_file_location("prepare_kind_image_sources", MODULE_PATH)
assert SPEC and SPEC.loader
prepare_sources = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(prepare_sources)


class PrepareKindImageSourcesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="kind-image-sources-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def git(self, root: Path, *args: str) -> str:
        return subprocess.check_output(["git", "-C", str(root), *args], text=True).strip()

    def test_patches_locked_source_and_keeps_api_build_context_clean(self) -> None:
        maas = self.root / "maas"
        maas.mkdir()
        self.git(maas, "init", "-q")
        self.git(maas, "config", "user.email", "test@example.invalid")
        self.git(maas, "config", "user.name", "test")
        base_content = "".join(f"key-{index}: value\n" for index in range(1, 8)) + "secretName: old\nlast: value\n"
        (maas / "controller.yaml").write_text(base_content, encoding="utf-8")
        self.git(maas, "add", "controller.yaml")
        self.git(maas, "commit", "-qm", "source")
        patch_base_sha = self.git(maas, "rev-parse", "HEAD")
        self.git(maas, "checkout", "--detach", patch_base_sha)
        (maas / "controller.yaml").write_text(base_content.replace("secretName: old\n", "secretName: corrected\n"), encoding="utf-8")
        patch = self.root / "maas.patch"
        patch.write_bytes(subprocess.check_output(["git", "-C", str(maas), "diff", "--binary", "--full-index", patch_base_sha]))
        self.git(maas, "checkout", "--", "controller.yaml")
        (maas / "controller.yaml").write_text("unrelated: moved-source\n" + base_content, encoding="utf-8")
        self.git(maas, "add", "controller.yaml")
        self.git(maas, "commit", "-qm", "unrelated upstream line movement")
        sha = self.git(maas, "rev-parse", "HEAD")
        self.git(maas, "checkout", "--detach", sha)

        source_locks = self.root / "source-locks.json"
        source_lock = {
            "schema": "praxis-extproc.source-locks-run/v1",
            "scenario": "grid-llmd-kind",
            "source_mode": "main",
            "components": {
                "maas": {
                    "remote_url": "https://example.invalid/maas.git",
                    "requested_ref": "main",
                    "resolved_sha": sha,
                    "build_context_sha256": prepare_sources.archive_hash(maas, sha),
                    "worktree_path": str(maas),
                }
            },
        }
        source_locks.write_text(json.dumps(source_lock), encoding="utf-8")
        output = self.root / "source-resolution"
        output.mkdir()
        (output / "source-locks.md").write_text("# Source locks\n", encoding="utf-8")

        result = prepare_sources.prepare(source_locks, self.root / "worktrees", patch, output)

        patch_sha = hashlib.sha256(prepare_sources.canonicalize(patch.read_bytes())).hexdigest()
        self.assertEqual(result["source_patch_sha256"], patch_sha)
        self.assertEqual(result["source_patch_file_sha256"], hashlib.sha256(patch.read_bytes()).hexdigest())
        patched_lock = json.loads(source_locks.read_text(encoding="utf-8"))["components"]["maas"]
        self.assertEqual(patched_lock["source_patch_sha256"], patch_sha)
        diff = subprocess.check_output(["git", "-C", str(maas), "diff", "--binary", "--full-index", sha])
        self.assertNotEqual(diff, patch.read_bytes())
        self.assertEqual(prepare_sources.canonicalize(diff), prepare_sources.canonicalize(patch.read_bytes()))
        api_lock = json.loads((output / "maas-api-build-locks.json").read_text(encoding="utf-8"))
        api_root = Path(api_lock["components"]["maas"]["worktree_path"])
        self.assertEqual(self.git(api_root, "rev-parse", "HEAD"), sha)
        self.assertEqual(self.git(api_root, "status", "--porcelain"), "")
        self.assertEqual(api_lock["components"]["maas"]["build_context_sha256"], source_lock["components"]["maas"]["build_context_sha256"])

    def test_non_kind_lock_is_rejected_before_worktree_changes(self) -> None:
        locks = self.root / "source-locks.json"
        locks.write_text(json.dumps({"schema": "praxis-extproc.source-locks-run/v1", "scenario": "grid-llmd", "components": {}}))
        with self.assertRaisesRegex(prepare_sources.PreparationError, "Kind source-resolution"):
            prepare_sources.prepare(locks, self.root / "worktrees", self.root / "missing.patch", self.root)


if __name__ == "__main__":
    unittest.main()
