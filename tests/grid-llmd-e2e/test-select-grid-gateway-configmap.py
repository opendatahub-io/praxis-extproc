#!/usr/bin/env python3
"""Regression coverage for the chart handoff's active Praxis ConfigMap."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


HELPER = Path(__file__).with_name("select-grid-gateway-configmap.py")
SPEC = importlib.util.spec_from_file_location("select_grid_gateway_configmap", HELPER)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SelectGridGatewayConfigMapTests(unittest.TestCase):
    def test_base_config_remains_target_before_grid_slot_activation(self):
        self.assertEqual(
            MODULE.select_configmap("grid-network-praxis", "grid-network-praxis", "grid-gateway-config"),
            "grid-network-praxis",
        )

    def test_bootstrap_config_selects_the_generated_base_as_upgrade_target(self):
        self.assertEqual(
            MODULE.select_configmap("grid-network-praxis", "grid-gateway-config", "grid-gateway-config"),
            "grid-network-praxis",
        )

    def test_double_buffered_grid_slot_is_preserved_for_helm_handoff(self):
        self.assertEqual(
            MODULE.select_configmap(
                "grid-network-praxis", "grid-consumer-config-821b14e55b585177", "grid-gateway-config"
            ),
            "grid-consumer-config-821b14e55b585177",
        )

    def test_unknown_active_config_is_rejected(self):
        with self.assertRaises(ValueError):
            MODULE.select_configmap("grid-network-praxis", "some-other-config", "grid-gateway-config")


if __name__ == "__main__":
    unittest.main()
