#!/usr/bin/env python3
"""Write a bounded, credential-redacted summary of the failure-closed HTTP response."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import re
import sys

from sanitize_diagnostics import sanitize


ALLOWED_HEADERS = {
    "content-length",
    "content-type",
    "date",
    "grpc-message",
    "grpc-status",
    "retry-after",
    "server",
    "via",
    "x-envoy-response-flags",
    "x-envoy-upstream-service-time",
    "x-praxis-extproc",
    "x-request-id",
}
PREVIEW_LIMIT = 4096


def summarize(headers_text: str, body: bytes) -> dict[str, object]:
    status_line = ""
    headers: dict[str, list[str]] = {}
    for block in re.split(r"\r?\n\r?\n", headers_text):
        lines = block.splitlines()
        if not lines or not lines[0].startswith("HTTP/"):
            continue
        status_line = lines[0][:256]
        current: dict[str, list[str]] = {}
        for line in lines[1:]:
            name, separator, value = line.partition(":")
            normalized = name.strip().lower()
            if separator and normalized in ALLOWED_HEADERS:
                current.setdefault(normalized, []).append(value.strip()[:1024])
        headers = current

    body_text = body.decode("utf-8", errors="replace")
    safe_preview = sanitize(body_text)
    return {
        "statusLine": status_line,
        "headers": headers,
        "bodyBytes": len(body),
        "bodySha256": hashlib.sha256(body).hexdigest(),
        "bodyPreview": safe_preview[:PREVIEW_LIMIT],
        "bodyPreviewTruncated": len(safe_preview) > PREVIEW_LIMIT,
    }


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: summarize_failure_response.py HEADERS_FILE BODY_FILE", file=sys.stderr)
        return 2
    headers_path, body_path = map(Path, sys.argv[1:])
    summary = summarize(headers_path.read_text(encoding="utf-8", errors="replace"), body_path.read_bytes())
    json.dump(summary, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
