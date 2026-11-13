#!/usr/bin/env python3
"""Behavioral tests for immutable image provenance verification."""

import argparse
import json
from pathlib import Path
import tempfile
import unittest

from verify_image_provenance import verify


class VerifyImageProvenanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.addCleanup(self.tempdir.cleanup)
        self.inspect_path = Path(self.tempdir.name) / "inspect.json"
        self.source_url = "https://github.com/example/project.git"
        self.source_sha = "a" * 40
        self.context_sha = "b" * 64
        self.image = "registry.example/project@sha256:" + "c" * 64

    def _verify(self, labels: dict[str, str], patch_sha: str = "", context_subdir: str = "") -> dict:
        self.inspect_path.write_text(json.dumps({"config": {"Labels": labels}}), encoding="utf-8")
        args = argparse.Namespace(
            image=self.image,
            source_url=self.source_url,
            source_sha=self.source_sha,
            build_context_sha256=self.context_sha,
            source_patch_sha256=patch_sha,
            build_context_subdir=context_subdir,
            inspect_config=self.inspect_path,
            runtime_image_id="",
        )
        return verify(args)

    def _base_labels(self) -> dict[str, str]:
        return {
            "org.opencontainers.image.source": self.source_url,
            "org.opencontainers.image.revision": self.source_sha,
            "org.praxis.build-context.sha256": self.context_sha,
        }

    def test_pristine_source_provenance_passes(self) -> None:
        result = self._verify(self._base_labels())
        self.assertTrue(result["verified"])
        self.assertNotIn("source_patch_sha256", result)

    def test_locked_source_patch_provenance_passes(self) -> None:
        patch_sha = "d" * 64
        labels = self._base_labels()
        labels["org.praxis.source-patch.sha256"] = patch_sha
        result = self._verify(labels, patch_sha)
        self.assertEqual(result["source_patch_sha256"], patch_sha)

    def test_missing_locked_source_patch_label_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "source patch OCI provenance label"):
            self._verify(self._base_labels(), "d" * 64)

    def test_unrecorded_source_patch_label_fails(self) -> None:
        labels = self._base_labels()
        labels["org.praxis.source-patch.sha256"] = "d" * 64
        with self.assertRaisesRegex(ValueError, "no patch was recorded"):
            self._verify(labels)

    def test_subdirectory_build_context_is_verified_and_recorded(self) -> None:
        labels = self._base_labels()
        labels["org.praxis.build-context.subdir"] = "maas-api"
        result = self._verify(labels, context_subdir="maas-api")
        self.assertEqual(result["build_context_subdir"], "maas-api")

    def test_missing_subdirectory_build_context_label_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "build context subdirectory OCI provenance label"):
            self._verify(self._base_labels(), context_subdir="maas-api")

    def test_unrecorded_subdirectory_build_context_label_fails(self) -> None:
        labels = self._base_labels()
        labels["org.praxis.build-context.subdir"] = "maas-api"
        with self.assertRaisesRegex(ValueError, "none was recorded"):
            self._verify(labels)


if __name__ == "__main__":
    unittest.main()
