#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Normalize source-patch metadata that varies when upstream moves code."""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
from pathlib import Path


HUNK = re.compile(rb"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$")


def canonicalize(patch: bytes) -> bytes:
    """Remove Git blob IDs and hunk positions while retaining patch content."""
    result: list[bytes] = []
    for line in patch.splitlines(keepends=True):
        if line.startswith(b"index "):
            continue
        body = line.rstrip(b"\r\n")
        ending = line[len(body) :]
        match = HUNK.match(body)
        if match:
            old_count = int(match.group(2) or b"1")
            new_count = int(match.group(4) or b"1")
            body = (
                f"@@ -0,{old_count} +0,{new_count} @@".encode("ascii")
                + match.group(5)
            )
        result.append(body + ending)
    return b"".join(result)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sha256", action="store_true", help="print the canonical digest")
    parser.add_argument("patch", nargs="?", type=Path, help="patch file; stdin when omitted")
    args = parser.parse_args()
    patch = args.patch.read_bytes() if args.patch else sys.stdin.buffer.read()
    normalized = canonicalize(patch)
    if args.sha256:
        print(hashlib.sha256(normalized).hexdigest())
    else:
        sys.stdout.buffer.write(normalized)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
