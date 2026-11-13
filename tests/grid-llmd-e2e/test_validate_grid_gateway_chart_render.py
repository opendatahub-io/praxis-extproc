#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import unittest

from validate_grid_gateway_chart_render import RenderContractError, validate_resources


def resource(kind: str, name: str, **fields: object) -> dict:
    return {"apiVersion": "v1", "kind": kind, "metadata": {"name": name}, **fields}


def valid_resources() -> list[dict]:
    return [
        resource(
            "ServiceAccount",
            "grid-gateway-metrics-scraper",
            automountServiceAccountToken=False,
        ),
        resource(
            "Secret",
            "grid-gateway-metrics-token",
            metadata={
                "name": "grid-gateway-metrics-token",
                "labels": {
                    "app.kubernetes.io/managed-by": "Helm",
                    "app.kubernetes.io/instance": "grid-gateway",
                    "praxis.fast/qualification-run": "kind-run-1",
                },
                "annotations": {"kubernetes.io/service-account.name": "grid-gateway-metrics-scraper"},
            },
            type="kubernetes.io/service-account-token",
        ),
        resource("Deployment", "grid-gateway"),
        resource("Service", "grid-gateway"),
    ]


class GatewayChartRenderTests(unittest.TestCase):
    def test_accepts_only_chart_owned_empty_service_account_token_secret(self) -> None:
        result = validate_resources(valid_resources(), "grid-gateway", "grid-gateway", "kind-run-1")
        self.assertEqual(result["status"], "PASS")
        self.assertFalse(result["chartOwnedSecret"]["credentialBytesPresent"])

    def test_rejects_gateway_api_resources_owned_by_the_separate_task_helper(self) -> None:
        with self.assertRaisesRegex(RenderContractError, "task Gateway helper"):
            validate_resources(valid_resources() + [resource("Gateway", "grid-llmd-ingress")], "grid-gateway", "grid-gateway", "kind-run-1")
        with self.assertRaisesRegex(RenderContractError, "task Gateway helper"):
            validate_resources(valid_resources() + [resource("HTTPRoute", "grid-llmd-inference")], "grid-gateway", "grid-gateway", "kind-run-1")

    def test_rejects_unexpected_secrets(self) -> None:
        with self.assertRaisesRegex(RenderContractError, "expected exactly"):
            validate_resources(valid_resources() + [resource("Secret", "site-identity")], "grid-gateway", "grid-gateway", "kind-run-1")

    def test_rejects_embedded_credential_bytes(self) -> None:
        resources = valid_resources()
        resources[1]["data"] = {"token": "must-not-be-rendered"}
        with self.assertRaisesRegex(RenderContractError, "credential bytes"):
            validate_resources(resources, "grid-gateway", "grid-gateway", "kind-run-1")

    def test_rejects_wrong_release_run_or_service_account_ownership(self) -> None:
        cases = (
            ("app.kubernetes.io/instance", "other-release"),
            ("praxis.fast/qualification-run", "other-run"),
        )
        for label, value in cases:
            with self.subTest(label=label):
                resources = valid_resources()
                resources[1]["metadata"]["labels"][label] = value
                with self.assertRaises(RenderContractError):
                    validate_resources(resources, "grid-gateway", "grid-gateway", "kind-run-1")

        resources = valid_resources()
        resources[1]["metadata"]["annotations"]["kubernetes.io/service-account.name"] = "unrelated"
        with self.assertRaisesRegex(RenderContractError, "unexpected ServiceAccount"):
            validate_resources(resources, "grid-gateway", "grid-gateway", "kind-run-1")

    def test_rejects_automounted_scraper_identity(self) -> None:
        resources = valid_resources()
        resources[0]["automountServiceAccountToken"] = True
        with self.assertRaisesRegex(RenderContractError, "automount disabled"):
            validate_resources(resources, "grid-gateway", "grid-gateway", "kind-run-1")


if __name__ == "__main__":
    unittest.main()
