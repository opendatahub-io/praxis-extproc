#!/usr/bin/env python3
"""Local regressions for source channel resolution and worktree safety."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("resolve_sources.py")
SPEC = importlib.util.spec_from_file_location("resolve_sources", MODULE_PATH)
assert SPEC and SPEC.loader
resolver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(resolver)


class SourceResolutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="source-resolution-test-")
        self.root = Path(self.temp.name)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def git(self, cwd: Path, *arguments: str) -> str:
        return subprocess.check_output(["git", *arguments], cwd=cwd, text=True).strip()

    def test_main_is_default_and_locked_revisions_remain_available(self) -> None:
        locks = json.loads(resolver.LOCK_FILE.read_text(encoding="utf-8"))
        self.assertEqual(resolver.source_ref("aigc", "main", None, locks), ("main", None))
        self.assertEqual(resolver.source_ref("epp", "main", None, locks), ("main", None))
        self.assertEqual(resolver.source_ref("llm-d-sim", "main", None, locks), ("main", None))
        self.assertEqual(resolver.source_ref("praxis-ai", "main", None, locks), ("main", None))
        self.assertEqual(
            resolver.source_ref("maas", "locked", None, locks),
            ("ccce7752af146eb3de9e99df107c26f414878726", "ccce7752af146eb3de9e99df107c26f414878726"),
        )
        self.assertEqual(
            resolver.source_ref("epp", "locked", None, locks),
            ("b9393d1a9f1d5a72ecd86f8d2b643ab54c2a9b92", "b9393d1a9f1d5a72ecd86f8d2b643ab54c2a9b92"),
        )
        self.assertEqual(
            resolver.source_ref("llm-d-sim", "locked", None, locks),
            ("bca276ef64263e214b4822940bb1e58d018cbf9d", "bca276ef64263e214b4822940bb1e58d018cbf9d"),
        )
        with self.assertRaises(resolver.ResolutionError):
            resolver.source_ref("maas", "locked", "main", locks)

    def test_ref_overrides_are_component_specific(self) -> None:
        self.assertEqual(resolver.REF_ENV["aigc"], "AIGC_REF")
        self.assertEqual(resolver.REF_ENV["maas"], "MAAS_REF")
        self.assertEqual(resolver.REF_ENV["extproc"], "EXTPROC_REF")
        self.assertEqual(resolver.REF_ENV["praxis-ai"], "PRAXIS_AI_REF")
        self.assertEqual(resolver.REF_ENV["grid"], "GRID_REF")
        self.assertEqual(resolver.REF_ENV["gitops"], "GITOPS_REF")
        self.assertEqual(resolver.SOURCE_DEFAULTS["kuadrant"], "v1.4.2")

    def test_lanes_resolve_only_their_source_roles(self) -> None:
        native = resolver.selected_components("grid-llmd", False)
        maas = resolver.selected_components("grid-maas", True)
        self.assertIn("grid", native)
        self.assertIn("maas", native)
        self.assertIn("praxis-ai", native)
        self.assertNotIn("aigc", native)
        self.assertIn("aigc", maas)
        self.assertIn("maas", maas)
        self.assertIn("gitops", maas)

    def test_embedded_remote_credentials_are_rejected(self) -> None:
        with self.assertRaises(resolver.ResolutionError):
            resolver.sanitize_remote("https://user:password@example.com/repo.git")

    def test_resolver_uses_clean_detached_copy_and_hashes_dirty_caller(self) -> None:
        seed = self.root / "seed"
        seed.mkdir()
        self.git(seed, "init", "-q")
        self.git(seed, "config", "user.email", "test@example.invalid")
        self.git(seed, "config", "user.name", "test")
        (seed / "source.txt").write_text("committed\n", encoding="utf-8")
        self.git(seed, "add", "source.txt")
        self.git(seed, "commit", "-qm", "initial")
        sha = self.git(seed, "rev-parse", "HEAD")
        remote = self.root / "remote.git"
        subprocess.run(["git", "clone", "--bare", str(seed), str(remote)], check=True, capture_output=True)
        caller = self.root / "caller"
        subprocess.run(["git", "clone", str(remote), str(caller)], check=True, capture_output=True)
        (caller / "source.txt").write_text("dirty tracked\n", encoding="utf-8")
        (caller / "new.txt").write_text("untracked contents\n", encoding="utf-8")
        before = self.git(caller, "status", "--porcelain", "--untracked-files=all")
        metadata = resolver.resolve_component(
            "extproc",
            remote.as_uri(),
            sha,
            sha,
            caller,
            self.root / "resolved",
        )
        after = self.git(caller, "status", "--porcelain", "--untracked-files=all")
        self.assertEqual(before, after)
        self.assertEqual(metadata["resolved_sha"], sha)
        self.assertEqual(metadata["branch_state"], "detached")
        self.assertTrue(metadata["resolved_worktree_clean"])
        self.assertEqual(metadata["caller_worktree_state"], "dirty")
        self.assertNotEqual(metadata["caller_tracked_diff_sha256"], resolver.hash_caller_tree(None)[1])
        self.assertNotEqual(metadata["caller_untracked_content_sha256"], resolver.hash_caller_tree(None)[2])
        self.assertEqual(self.git(Path(metadata["worktree_path"]), "status", "--porcelain"), "")


if __name__ == "__main__":
    unittest.main()
