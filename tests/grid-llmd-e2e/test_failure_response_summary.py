#!/usr/bin/env python3
"""Contract tests for safe failure-closed response evidence."""

from __future__ import annotations

import unittest

from summarize_failure_response import summarize


class FailureResponseSummaryTests(unittest.TestCase):
    def test_keeps_status_and_allowlisted_headers_only(self) -> None:
        response = summarize(
            "HTTP/2 503 Service Unavailable\r\n"
            "Content-Type: text/plain\r\n"
            "X-Request-Id: envoy-generated-id\r\n"
            "X-Envoy-Response-Flags: UH\r\n"
            "Set-Cookie: session=private\r\n"
            "Authorization: Bearer private-token\r\n\r\n",
            b"upstream unavailable",
        )
        self.assertEqual(response["statusLine"], "HTTP/2 503 Service Unavailable")
        self.assertEqual(
            response["headers"],
            {
                "content-type": ["text/plain"],
                "x-envoy-response-flags": ["UH"],
                "x-request-id": ["envoy-generated-id"],
            },
        )
        self.assertNotIn("private", str(response))

    def test_body_preview_is_bounded_and_credential_redacted(self) -> None:
        body = b'{"error":"upstream unavailable","access_token":"private-token"}' + b"x" * 5000
        response = summarize("HTTP/1.1 503 Service Unavailable\r\n\r\n", body)
        self.assertEqual(response["bodyBytes"], len(body))
        self.assertTrue(response["bodyPreviewTruncated"])
        self.assertLessEqual(len(response["bodyPreview"]), 4096)
        self.assertNotIn("private-token", response["bodyPreview"])
        self.assertEqual(len(response["bodySha256"]), 64)


if __name__ == "__main__":
    unittest.main()
