#!/usr/bin/env python3
"""Tests that deployment images are digest-pinned and source matched."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("verify_image_provenance.py")
SPEC = importlib.util.spec_from_file_location("verify_image_provenance", MODULE_PATH)
assert SPEC and SPEC.loader
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)


class ImageProvenanceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="image-provenance-test-")
        self.root = Path(self.temp.name)
        self.inspect = self.root / "config.json"
        self.inspect.write_text(json.dumps({"config": {"Labels": self.labels()}}), encoding="utf-8")
        self.args = type("Args", (), {
            "image": "registry.example.invalid/praxis/extproc@sha256:" + "a" * 64,
            "source_url": "https://github.com/opendatahub-io/praxis-extproc.git",
            "source_sha": "b" * 40,
            "build_context_sha256": "c" * 64,
            "inspect_config": self.inspect,
            "runtime_image_id": "containerd://sha256:" + "a" * 64,
        })()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def labels(self) -> dict[str, str]:
        return {
            "org.opencontainers.image.source": "https://github.com/opendatahub-io/praxis-extproc.git",
            "org.opencontainers.image.revision": "b" * 40,
            "org.praxis.build-context.sha256": "c" * 64,
        }

    def test_digest_source_sha_context_and_runtime_id_are_recorded(self) -> None:
        result = verifier.verify(self.args)
        self.assertTrue(result["verified"])
        self.assertEqual(result["source_sha"], "b" * 40)
        self.assertIn("runtime_image_id", result)

    def test_mutable_image_tag_is_rejected(self) -> None:
        self.args.image = "registry.example.invalid/praxis/extproc:main"
        with self.assertRaisesRegex(ValueError, "immutable sha256 digest"):
            verifier.verify(self.args)

    def test_label_mismatch_is_rejected(self) -> None:
        self.inspect.write_text(json.dumps({"config": {"Labels": {}}}), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "labels are missing or mismatched"):
            verifier.verify(self.args)


if __name__ == "__main__":
    unittest.main()
