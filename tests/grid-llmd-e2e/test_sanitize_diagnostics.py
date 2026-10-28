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


if __name__ == "__main__":
    unittest.main()
