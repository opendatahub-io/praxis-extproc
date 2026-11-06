#!/usr/bin/env python3

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from resolve_kind_image_tag import resolve_tag  # noqa: E402


class ResolveKindImageTagTests(unittest.TestCase):
    repository = "grid-run-registry:5000/qualification/kserve-llmisvc-controller"
    metadata = {
        "component": "kserve-llmisvc-controller",
        "deployed_image": (
            "grid-run-registry:5000/qualification/kserve-llmisvc-controller@sha256:"
            + "a" * 64
        ),
        "build_tag_for_push_only": (
            "127.0.0.1:32926/qualification/kserve-llmisvc-controller:"
            "e2e-run-66edb1c2791e"
        ),
    }

    def test_derives_tag_when_registry_host_and_port_differ(self):
        self.assertEqual(
            resolve_tag(self.metadata, self.repository, "kserve-llmisvc-controller"),
            "e2e-run-66edb1c2791e",
        )

    def test_accepts_matching_explicit_tag(self):
        self.assertEqual(
            resolve_tag(
                self.metadata,
                self.repository,
                "kserve-llmisvc-controller",
                "e2e-run-66edb1c2791e",
            ),
            "e2e-run-66edb1c2791e",
        )

    def test_rejects_mismatched_explicit_tag(self):
        with self.assertRaisesRegex(ValueError, "differs from the published"):
            resolve_tag(self.metadata, self.repository, "kserve-llmisvc-controller", "run-id")

    def test_rejects_mismatched_repository_component(self):
        with self.assertRaisesRegex(ValueError, "does not pin the requested deploy repository"):
            resolve_tag(
                self.metadata,
                "grid-run-registry:5000/qualification/other",
                "kserve-llmisvc-controller",
            )

    def test_rejects_wrong_component_metadata(self):
        with self.assertRaisesRegex(ValueError, "different image component"):
            resolve_tag(self.metadata, self.repository, "other-controller")


if __name__ == "__main__":
    unittest.main()
