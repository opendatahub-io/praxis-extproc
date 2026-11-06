# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from render_kind_maas_default_gateway_service import render  # noqa: E402


RUN_ID = "kind-run-test"
GATEWAY = {
    "apiVersion": "gateway.networking.k8s.io/v1",
    "kind": "Gateway",
    "metadata": {
        "name": "maas-default-gateway",
        "namespace": "maas-system",
        "uid": "gateway-uid",
        "generation": 3,
        "labels": {"praxis.kind/run": RUN_ID},
    },
    "status": {
        "conditions": [
            {"type": "Accepted", "status": "True", "observedGeneration": 3},
            {"type": "Programmed", "status": "False", "reason": "AddressNotAssigned", "observedGeneration": 3},
        ]
        ,"addresses": [{"type": "Hostname", "value": "maas-default-gateway-istio.maas-system.svc.cluster.local"}],
        "listeners": [{"name": "http", "conditions": [
            {"type": "Accepted", "status": "True", "observedGeneration": 3},
            {"type": "Programmed", "status": "True", "observedGeneration": 3},
            {"type": "ResolvedRefs", "status": "True", "observedGeneration": 3},
        ]}],
    },
}
SERVICE = {
    "kind": "Service",
    "metadata": {
        "name": "maas-default-gateway-istio",
        "namespace": "maas-system",
        "ownerReferences": [{"kind": "Gateway", "name": "maas-default-gateway", "uid": "gateway-uid"}],
    },
    "spec": {
        "type": "LoadBalancer",
        "clusterIP": "10.0.0.8",
        "selector": {"gateway.networking.k8s.io/gateway-name": "maas-default-gateway"},
    },
}
SLICES = {
    "items": [{
        "ports": [{"port": 80}],
        "endpoints": [{"addresses": ["10.0.0.2"], "conditions": {"ready": True}}],
    }]
}


class RenderMaaSGatewayServiceTests(unittest.TestCase):
    def test_alias_targets_current_gateway_uid_and_ready_service_selector(self) -> None:
        result = render(GATEWAY, SERVICE, SLICES, RUN_ID)
        self.assertEqual(result["metadata"]["namespace"], "maas-system")
        self.assertEqual(result["spec"]["selector"], SERVICE["spec"]["selector"])
        self.assertEqual(
            [(port["port"], port["targetPort"]) for port in result["spec"]["ports"]],
            [(80, 80), (443, 80)],
        )
        self.assertEqual(result["metadata"]["ownerReferences"][0]["uid"], "gateway-uid")
        self.assertEqual(result["metadata"]["labels"]["praxis.kind/run"], RUN_ID)

    def test_rejects_gateway_without_run_ownership(self) -> None:
        gateway = {**GATEWAY, "metadata": {**GATEWAY["metadata"], "labels": {}}}
        with self.assertRaisesRegex(ValueError, "not owned by this run"):
            render(gateway, SERVICE, SLICES, RUN_ID)

    def test_rejects_stale_listener_conditions_even_when_global_is_address_pending(self) -> None:
        gateway = {**GATEWAY, "status": {**GATEWAY["status"], "listeners": [{"name": "http", "conditions": [
            {"type": "Accepted", "status": "True", "observedGeneration": 2},
            {"type": "Programmed", "status": "True", "observedGeneration": 3},
            {"type": "ResolvedRefs", "status": "True", "observedGeneration": 3},
        ]}]}}
        with self.assertRaisesRegex(ValueError, "current generation"):
            render(gateway, SERVICE, SLICES, RUN_ID)

    def test_rejects_service_owned_by_an_old_gateway_uid(self) -> None:
        service = {**SERVICE, "metadata": {**SERVICE["metadata"], "ownerReferences": [
            {"kind": "Gateway", "name": "maas-default-gateway", "uid": "old-uid"}
        ]}}
        with self.assertRaisesRegex(ValueError, "current Gateway UID"):
            render(GATEWAY, service, SLICES, RUN_ID)

    def test_rejects_missing_ready_endpoint(self) -> None:
        slices = {"items": [{"ports": [{"port": 80}], "endpoints": [{"conditions": {"ready": False}}]}]}
        with self.assertRaisesRegex(ValueError, "no Ready HTTP endpoint"):
            render(GATEWAY, SERVICE, slices, RUN_ID)

    def test_rejects_wrong_kind_internal_hostname(self) -> None:
        gateway = {**GATEWAY, "status": {**GATEWAY["status"], "addresses": [
            {"type": "Hostname", "value": "wrong.example"}
        ]}}
        with self.assertRaisesRegex(ValueError, "internal generated Service"):
            render(gateway, SERVICE, SLICES, RUN_ID)


if __name__ == "__main__":
    unittest.main()
