# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import importlib.util
import ipaddress
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("validate-kind-registry-subnet.py")
SPEC = importlib.util.spec_from_file_location("validate_kind_registry_subnet", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class RegistrySubnetTests(unittest.TestCase):
    def test_accepts_unallocated_private_subnet(self) -> None:
        result = MODULE.validate_registry_network(
            "10.250.0.0/24", "10.250.0.1", [], []
        )
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["subnet"], "10.250.0.0/24")

    def test_rejects_existing_docker_network_overlap(self) -> None:
        networks = [
            {
                "Name": "unrelated-network",
                "IPAM": {"Config": [{"Subnet": "10.250.0.0/24"}]},
            }
        ]
        with self.assertRaisesRegex(ValueError, "unrelated-network"):
            MODULE.validate_registry_network("10.250.0.0/24", "10.250.0.1", networks, [])

    def test_ignores_ipv6_docker_network_ranges(self) -> None:
        networks = [
            {
                "Name": "kind",
                "IPAM": {"Config": [{"Subnet": "fc00:f853:ccd:e793::/64"}]},
            }
        ]
        result = MODULE.validate_registry_network("10.250.0.0/24", "10.250.0.1", networks, [])
        self.assertEqual(result["status"], "PASS")

    def test_rejects_host_route_overlap(self) -> None:
        routes = [{"dst": "10.250.0.0/24", "dev": "eth-test"}]
        with self.assertRaisesRegex(ValueError, "host route"):
            MODULE.validate_registry_network("10.250.0.0/24", "10.250.0.1", [], routes)

    def test_rejects_kind_pod_and_service_ranges(self) -> None:
        for subnet in ("10.244.1.0/24", "10.96.1.0/24"):
            gateway = str(next(ipaddress.ip_network(subnet).hosts()))
            with self.subTest(subnet=subnet), self.assertRaisesRegex(ValueError, "overlaps"):
                MODULE.validate_registry_network(subnet, gateway, [], [])

    def test_rejects_gateway_outside_subnet(self) -> None:
        with self.assertRaisesRegex(ValueError, "inside the registry subnet"):
            MODULE.validate_registry_network("10.250.0.0/24", "10.250.1.1", [], [])


if __name__ == "__main__":
    unittest.main()
