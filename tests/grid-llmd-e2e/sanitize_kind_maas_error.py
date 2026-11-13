#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Emit credential-safe diagnostics for a failed MaaS API response."""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


def sanitize(body: str, http_status: str) -> dict[str, Any]:
    state = "empty"
    fields: dict[str, Any] = {}
    if body:
        try:
            value = json.loads(body)
        except json.JSONDecodeError:
            state = "invalid-json"
        else:
            if not isinstance(value, dict):
                state = "json-non-object"
            else:
                state = "json-object"
                for key in ("error", "message"):
                    item = value.get(key)
                    if isinstance(item, str):
                        fields[key] = item[:256]
                code = value.get("code")
                if isinstance(code, str):
                    fields["code"] = code[:128]
                elif isinstance(code, (int, float)) and not isinstance(code, bool):
                    fields["code"] = code
    return {
        "httpStatus": http_status,
        "responseBodyState": state,
        "sanitizedError": fields,
        "credentialBytesRecorded": False,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--http-status", required=True)
    args = parser.parse_args()
    print(json.dumps(sanitize(sys.stdin.read(), args.http_status), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
