#!/usr/bin/env python3
"""Contract tests for the Grid operator's rollout-safe Praxis ConfigMap slots."""

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("validate-pool-route-contract.py")
SPEC = importlib.util.spec_from_file_location("pool_route_contract", SCRIPT)
CONTRACT = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(CONTRACT)


def fixture():
    network = {"metadata": {"name": "task-network"}}
    gateway = {
        "name": "task-gateway",
        "namespace": "task-namespace",
        "praxisConfig": {"configMapName": "praxis-base", "mountReconciliation": {"enabled": True}},
    }
    content = "filter_chains: []\n"
    config_map = {
        "metadata": {
            "name": "grid-consumer-config-placeholder",
            "namespace": "task-namespace",
            "labels": {
                "app.kubernetes.io/managed-by": "grid-operator",
                "grid.praxis.fast/network": "task-network",
                "grid.praxis.fast/gateway": "task-gateway",
            },
        },
        "data": {"praxis.yaml": content},
    }
    return network, gateway, config_map, content


class OperatorPraxisConfigSlotTest(unittest.TestCase):
    def test_active_alternate_config_slot_passes_with_matching_applied_revision(self):
        network, gateway, config_map, content = fixture()
        alternate = f"grid-consumer-config-{CONTRACT.hashlib.sha256(b'praxis-base').hexdigest()[:16]}"
        config_map["metadata"]["name"] = alternate
        digest = CONTRACT.hashlib.sha256(content.encode()).hexdigest()

        result = CONTRACT.validate_operator_config_map_mount(network, gateway, alternate, config_map, digest)

        self.assertEqual(result, (content, digest))

    def test_config_mount_accepts_the_base_slot(self):
        network, gateway, config_map, content = fixture()
        config_map["metadata"]["name"] = "praxis-base"
        digest = CONTRACT.hashlib.sha256(content.encode()).hexdigest()

        result = CONTRACT.validate_operator_config_map_mount(network, gateway, "praxis-base", config_map, digest)

        self.assertEqual(result[0], content)

    def test_unrelated_slot_is_rejected(self):
        network, gateway, config_map, _ = fixture()
        config_map["metadata"]["name"] = "unrelated-config"

        with self.assertRaisesRegex(RuntimeError, "does not mount the current operator-generated"):
            CONTRACT.validate_operator_config_map_mount(network, gateway, "unrelated-config", config_map, None)

    def test_unowned_config_map_is_rejected(self):
        network, gateway, config_map, _ = fixture()
        config_map["metadata"]["name"] = "praxis-base"
        config_map["metadata"]["labels"]["grid.praxis.fast/network"] = "other-network"

        with self.assertRaisesRegex(RuntimeError, "not owned by the current GridNetwork Gateway"):
            CONTRACT.validate_operator_config_map_mount(network, gateway, "praxis-base", config_map, None)

    def test_reconciliation_digest_must_match_content(self):
        network, gateway, config_map, _ = fixture()
        config_map["metadata"]["name"] = "praxis-base"

        with self.assertRaisesRegex(RuntimeError, "does not match the current reconciliation appliedRevision"):
            CONTRACT.validate_operator_config_map_mount(network, gateway, "praxis-base", config_map, "stale")


if __name__ == "__main__":
    unittest.main()
