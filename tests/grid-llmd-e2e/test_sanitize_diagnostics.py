#!/usr/bin/env python3
"""Focused tests for the EPP failure-diagnostic redactor."""

from __future__ import annotations

import importlib.util
import json
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("sanitize_diagnostics.py")
SPEC = importlib.util.spec_from_file_location("sanitize_diagnostics", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class SanitizeDiagnosticsTests(unittest.TestCase):
    def test_redacts_credential_assignments_and_preserves_context(self) -> None:
        text = (
            'API_TOKEN=token-value\n'
            '"client_secret": "secret-value"\n'
            'Authorization: Bearer bearer-value\n'
            '{"authorization": "Bearer json-bearer"}\n'
            'probe: /health\n'
        )
        redacted = MODULE.sanitize(text)
        for value in ("token-value", "secret-value", "bearer-value", "json-bearer"):
            self.assertNotIn(value, redacted)
        self.assertIn("probe: /health", redacted)
        self.assertIn("[REDACTED]", redacted)
        json_line = next(line for line in redacted.splitlines() if line.startswith("{"))
        self.assertEqual(json.loads(json_line), {"authorization": "[REDACTED]"})

    def test_redacts_private_key_blocks(self) -> None:
        text = "-----BEGIN PRIVATE KEY-----\nprivate-material\n-----END PRIVATE KEY-----\n"
        redacted = MODULE.sanitize(text)
        self.assertNotIn("private-material", redacted)
        self.assertIn("[REDACTED PRIVATE KEY]", redacted)

    def test_preserves_secret_reference_objects_as_valid_json(self) -> None:
        text = (
            '{"spec":{"tls":{"caSecretRef":{"name":"grid-ca",'
            '"namespace":"praxis-grid-run"},"siteSecretRef":'
            '{"name":"grid-site-identity","namespace":"praxis-grid-run"}},'
            '"client_secret":"must-redact","tokenRateLimits":[{"limit":1000}],'
            '"api-keys":{"plain":{"enabled":true}},'
            '"enableProjectedCredentials":false}}'
        )
        redacted = MODULE.sanitize(text)
        value = json.loads(redacted)
        self.assertEqual(
            value["spec"]["tls"]["caSecretRef"],
            {"name": "grid-ca", "namespace": "praxis-grid-run"},
        )
        self.assertEqual(
            value["spec"]["tls"]["siteSecretRef"],
            {"name": "grid-site-identity", "namespace": "praxis-grid-run"},
        )
        self.assertEqual(value["spec"]["client_secret"], "[REDACTED]")
        self.assertEqual(value["spec"]["tokenRateLimits"], [{"limit": 1000}])
        self.assertEqual(value["spec"]["api-keys"], {"plain": {"enabled": True}})
        self.assertIs(value["spec"]["enableProjectedCredentials"], False)

    def test_redacts_data_in_kubernetes_secret_objects(self) -> None:
        text = json.dumps(
            {
                "apiVersion": "v1",
                "kind": "Secret",
                "metadata": {"name": "tls-identity"},
                "data": {"tls.crt": "certificate-bytes", "tls.key": "private-key-bytes"},
                "type": "kubernetes.io/tls",
            }
        )
        value = json.loads(MODULE.sanitize(text))
        self.assertEqual(value["metadata"]["name"], "tls-identity")
        self.assertEqual(
            value["data"],
            {"tls.crt": "[REDACTED]", "tls.key": "[REDACTED]"},
        )
        self.assertNotIn("private-key-bytes", json.dumps(value))

    def test_redacts_credentials_embedded_in_structured_config_strings(self) -> None:
        text = json.dumps(
            {
                "kind": "ConfigMap",
                "data": {"client.yaml": "apiToken: hidden-token\nroute: /v1/chat"},
            }
        )
        value = json.loads(MODULE.sanitize(text))
        self.assertNotIn("hidden-token", value["data"]["client.yaml"])
        self.assertIn("route: /v1/chat", value["data"]["client.yaml"])

if __name__ == "__main__":
    unittest.main()
