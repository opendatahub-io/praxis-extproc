#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("plan_kind_images.py")
SPEC = importlib.util.spec_from_file_location("plan_kind_images", MODULE_PATH)
assert SPEC and SPEC.loader
planner = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(planner)


class PlanKindImagesTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.lock_dir = self.root / "locks"
        self.lock_dir.mkdir()
        self.patch = self.root / "maas-kind-cleanup-tls.patch"
        self.patch.write_text("test patch\n", encoding="utf-8")
        locks: dict[str, dict[str, dict[str, str]]] = {}

        for image, source, lock_name, dockerfile, subdir in planner.IMAGE_SPECS:
            source_root = self.root / f"{source}-{lock_name}"
            context = source_root / subdir if subdir else source_root
            context.mkdir(parents=True, exist_ok=True)
            containerfile = source_root / dockerfile
            containerfile.parent.mkdir(parents=True, exist_ok=True)
            containerfile.write_text("FROM scratch\n", encoding="utf-8")
            record = {"worktree_path": str(source_root)}
            if image == "maas-controller":
                record["source_patch_sha256"] = "a" * 64
            locks.setdefault(lock_name, {})[source] = record

        for lock_name, components in locks.items():
            (self.lock_dir / lock_name).write_text(
                json.dumps({"components": components}), encoding="utf-8"
            )

    def test_all_locked_image_contexts_are_validated_before_build(self) -> None:
        plan = planner.create_plan(self.lock_dir, self.patch)
        self.assertEqual(len(planner.IMAGE_SPECS), len(plan))
        payload = next(item for item in plan if item["image_component"] == "payload-processing")
        self.assertEqual("Dockerfile", payload["dockerfile"])

    def test_missing_payload_dockerfile_fails_during_preflight(self) -> None:
        payload = next(item for item in planner.IMAGE_SPECS if item[0] == "payload-processing")
        worktree = self.root / f"{payload[1]}-{payload[2]}"
        (worktree / "Dockerfile").unlink()
        with self.assertRaisesRegex(ValueError, "payload-processing.*Dockerfile"):
            planner.create_plan(self.lock_dir, self.patch)

    def test_missing_maas_patch_fails_preflight(self) -> None:
        with self.assertRaisesRegex(ValueError, "MaaS controller patch is missing"):
            planner.create_plan(self.lock_dir, self.root / "missing.patch")


if __name__ == "__main__":
    unittest.main()
