#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
from __future__ import annotations

import sys
import unittest
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from render_kind_maas_gateway import render  # noqa: E402


SOURCE = """\
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: maas-default-gateway
  namespace: maas-system
  labels:
    app.kubernetes.io/managed-by: local-env
spec:
  gatewayClassName: istio
  listeners:
    - name: http
      protocol: HTTP
      port: 80
      allowedRoutes:
        namespaces:
          from: Selector
          selector:
            matchLabels:
              local-env.opendatahub.io/gateway-tenant: models-as-a-service
---
apiVersion: v1
kind: Service
metadata:
  name: maas-default-gateway
  namespace: maas-system
"""


class RenderKindMaaSGatewayTests(unittest.TestCase):
    def test_extracts_only_default_gateway_and_adds_run_ownership(self) -> None:
        rendered, summary = render(SOURCE, "kind-run-1")
        docs = list(yaml.safe_load_all(rendered))
        self.assertEqual(len(docs), 1)
        gateway = docs[0]
        self.assertEqual(gateway["kind"], "Gateway")
        self.assertEqual(gateway["metadata"]["labels"]["praxis.kind/run"], "kind-run-1")
        self.assertEqual(gateway["metadata"]["labels"]["app.kubernetes.io/managed-by"], "grid-llmd-kind")
        self.assertEqual(summary["resource"], "Gateway/maas-default-gateway")

    def test_rejects_changed_gateway_class_or_tenant_selector(self) -> None:
        source = SOURCE.replace("gatewayClassName: istio", "gatewayClassName: other")
        with self.assertRaisesRegex(ValueError, "Istio GatewayClass"):
            render(source, "kind-run-1")
        source = SOURCE.replace("models-as-a-service", "other-tenant")
        with self.assertRaisesRegex(ValueError, "admit only"):
            render(source, "kind-run-1")

    def test_rejects_missing_or_duplicate_default_gateway(self) -> None:
        with self.assertRaisesRegex(ValueError, "found 0"):
            render("kind: Service\nmetadata:\n  name: x\n", "kind-run-1")
        with self.assertRaisesRegex(ValueError, "found 2"):
            render(SOURCE + "\n" + SOURCE.split("---")[0], "kind-run-1")


if __name__ == "__main__":
    unittest.main()
