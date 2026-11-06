#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Contract tests for the Kind Istiod egress-policy renderer."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from render_kind_payload_istiod_networkpolicy import render  # noqa: E402


SERVICE = {
    "metadata": {"name": "istiod", "namespace": "istio-system"},
    "spec": {"selector": {"app": "istiod", "istio": "pilot"}, "ports": [{"port": 15012, "protocol": "TCP"}]},
}
SLICES = {"items": [{
    "metadata": {"labels": {"kubernetes.io/service-name": "istiod"}},
    "endpoints": [{"addresses": ["10.0.0.2"], "conditions": {"ready": True}}],
}]}


class RenderKindIstiodPolicyTests(unittest.TestCase):
    def test_policy_uses_live_service_selector_and_only_xds_port(self) -> None:
        import yaml
        rendered, summary = render(SERVICE, SLICES, "kind-run-1")
        policy = yaml.safe_load(rendered)
        self.assertEqual(policy["metadata"]["labels"]["praxis.kind/run"], "kind-run-1")
        peer = policy["spec"]["egress"][0]["to"][0]
        self.assertEqual(peer["podSelector"]["matchLabels"], SERVICE["spec"]["selector"])
        self.assertEqual(policy["spec"]["egress"][0]["ports"], [{"protocol": "TCP", "port": 15012}])
        self.assertEqual(summary["istiod_ready_endpoint_count"], 1)

    def test_rejects_unready_or_wrong_service(self) -> None:
        with self.assertRaisesRegex(ValueError, "no ready EndpointSlice"):
            render(SERVICE, {"items": []}, "kind-run-1")
        with self.assertRaisesRegex(ValueError, "expected the istio-system/istiod Service"):
            render({**SERVICE, "metadata": {"name": "other", "namespace": "istio-system"}}, SLICES, "kind-run-1")

    def test_rejects_service_without_xds_port_or_selector(self) -> None:
        with self.assertRaisesRegex(ValueError, "TCP/15012"):
            render({**SERVICE, "spec": {"selector": SERVICE["spec"]["selector"], "ports": [{"port": 80}]}}, SLICES, "kind-run-1")
        with self.assertRaisesRegex(ValueError, "non-empty Pod selector"):
            render({**SERVICE, "spec": {"selector": {}, "ports": SERVICE["spec"]["ports"]}}, SLICES, "kind-run-1")


if __name__ == "__main__":
    unittest.main()
