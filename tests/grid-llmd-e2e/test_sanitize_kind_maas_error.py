#!/usr/bin/env python3
"""Regression tests for credential-safe MaaS failure diagnostics."""

import unittest

from sanitize_kind_maas_error import sanitize


class KindMaaSErrorSanitizationTests(unittest.TestCase):
    def test_empty_body_is_valid_json_diagnostic(self) -> None:
        self.assertEqual(
            sanitize("", "500"),
            {
                "httpStatus": "500",
                "responseBodyState": "empty",
                "sanitizedError": {},
                "credentialBytesRecorded": False,
            },
        )

    def test_invalid_and_non_object_bodies_are_classified(self) -> None:
        self.assertEqual(sanitize("not-json", "502")["responseBodyState"], "invalid-json")
        self.assertEqual(sanitize("[]", "502")["responseBodyState"], "json-non-object")

    def test_object_diagnostics_allowlist_fields_and_omit_credentials(self) -> None:
        result = sanitize('{"error":"upstream failed","message":"retry later","code":503,"key":"secret","token":"secret"}', "503")
        self.assertEqual(result["responseBodyState"], "json-object")
        self.assertEqual(result["sanitizedError"], {"error": "upstream failed", "message": "retry later", "code": 503})
        self.assertNotIn("secret", str(result))

    def test_strings_are_bounded(self) -> None:
        result = sanitize('{"error":"' + "x" * 300 + '"}', "500")
        self.assertEqual(len(result["sanitizedError"]["error"]), 256)


if __name__ == "__main__":
    unittest.main()
