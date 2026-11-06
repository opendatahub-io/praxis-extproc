#!/usr/bin/env python3
"""Regression tests for the Kind AIGC ExtProc manifest overlay."""

import unittest

from render_kind_aigc_manifest_bundle import render

import yaml


SOURCE = """
apiVersion: apps/v1
kind: Deployment
metadata:
  name: payload-processing
spec:
  template:
    spec:
      containers:
      - name: processor
        ports:
        - name: grpc
          containerPort: 9004
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: payload-pre-processing
spec:
  template:
    spec:
      containers:
      - name: processor
        ports:
        - name: grpc
          containerPort: 9004
---
apiVersion: v1
kind: Service
metadata:
  name: payload-processing
spec:
  ports:
  - port: 9004
    appProtocol: HTTP2
"""


class KindAIGCManifestBundleTests(unittest.TestCase):
    def test_excludes_only_tls_extproc_inbound_port_and_preserves_service(self) -> None:
        bundle = render(SOURCE, "run-123", "opendatahub", "praxis-extproc-kind-run-123")
        self.assertTrue(bundle["immutable"])
        self.assertEqual(bundle["metadata"]["labels"]["praxis.kind/run"], "run-123")
        docs = list(yaml.safe_load_all(bundle["data"]["resources.yaml"]))
        deployments = {doc["metadata"]["name"]: doc for doc in docs if doc["kind"] == "Deployment"}
        self.assertEqual(
            deployments["payload-processing"]["spec"]["template"]["metadata"]["annotations"][
                "traffic.sidecar.istio.io/excludeInboundPorts"
            ],
            "9004",
        )
        self.assertEqual(
            deployments["payload-pre-processing"]["spec"]["template"]["metadata"]["annotations"][
                "traffic.sidecar.istio.io/excludeInboundPorts"
            ],
            "9004",
        )
        service = next(doc for doc in docs if doc["kind"] == "Service")
        self.assertEqual(service["spec"]["ports"][0]["appProtocol"], "HTTP2")
        self.assertEqual(yaml.safe_load(bundle["data"]["kustomization.yaml"])["resources"], ["resources.yaml"])

    def test_rejects_a_preexisting_different_exclusion(self) -> None:
        source = SOURCE.replace("template:\n    spec:", "template:\n    metadata:\n      annotations:\n        traffic.sidecar.istio.io/excludeInboundPorts: '9004,50052'\n    spec:", 1)
        with self.assertRaisesRegex(ValueError, "already excludes different inbound ports"):
            render(source, "run-123", "opendatahub", "bundle")

    def test_requires_both_processor_deployments(self) -> None:
        with self.assertRaisesRegex(ValueError, "must contain both AIGC processor Deployments"):
            render(SOURCE.split("---", 1)[0], "run-123", "opendatahub", "bundle")


if __name__ == "__main__":
    unittest.main()
