#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Contract tests for Kind containerd local-registry configuration."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("configure-kind-registry-config.py")
SPEC = importlib.util.spec_from_file_location("configure_kind_registry_config", MODULE_PATH)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = module
SPEC.loader.exec_module(module)


class ContainerdRegistryConfigTests(unittest.TestCase):
    def test_replaces_empty_registry_path_in_cri_registry_table(self) -> None:
        before = '''version = 2

[plugins."io.containerd.grpc.v1.cri".registry]
  config_path = ""

[plugins."io.containerd.grpc.v1.cri".containerd]
  snapshotter = "overlayfs"
'''
        updated = module.ensure_registry_config(before)
        parsed = module.tomllib.loads(updated)
        self.assertEqual(
            parsed["plugins"]["io.containerd.grpc.v1.cri"]["registry"]["config_path"],
            "/etc/containerd/certs.d",
        )
        self.assertEqual(updated.count("config_path ="), 1)

    def test_update_is_idempotent(self) -> None:
        source = '''version = 2
[plugins."io.containerd.grpc.v1.cri".registry]
  config_path = "/etc/containerd/certs.d"
'''
        self.assertEqual(module.ensure_registry_config(source), source)

    def test_rejects_conflicting_existing_path(self) -> None:
        source = '''version = 2
[plugins."io.containerd.grpc.v1.cri".registry]
  config_path = "/tmp/other"
'''
        with self.assertRaisesRegex(ValueError, "different value"):
            module.ensure_registry_config(source)

    def test_creates_registry_table_when_missing(self) -> None:
        source = '''version = 2
[plugins."io.containerd.grpc.v1.cri"]
  sandbox_image = "registry.k8s.io/pause:3.10"
'''
        updated = module.ensure_registry_config(source)
        parsed = module.tomllib.loads(updated)
        self.assertEqual(parsed["version"], 2)
        self.assertEqual(
            parsed["plugins"]["io.containerd.grpc.v1.cri"]["registry"]["config_path"],
            "/etc/containerd/certs.d",
        )


if __name__ == "__main__":
    unittest.main()
