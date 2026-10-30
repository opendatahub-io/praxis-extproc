#!/usr/bin/env python3
"""Verify an immutable image reference and its source/build OCI labels."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
from pathlib import Path


DIGEST_RE = re.compile(r"@sha256:[0-9a-f]{64}$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
CONTEXT_RE = re.compile(r"^[0-9a-f]{64}$")


def labels_from(document: dict) -> dict:
    config = document.get("config")
    if isinstance(config, dict) and isinstance(config.get("Labels"), dict):
        return config["Labels"]
    if isinstance(document.get("Labels"), dict):
        return document["Labels"]
    return {}


def verify(args: argparse.Namespace) -> dict:
    if not DIGEST_RE.search(args.image):
        raise ValueError("deployed image must use an immutable sha256 digest")
    if not SHA_RE.fullmatch(args.source_sha):
        raise ValueError("source revision must be a full Git SHA")
    if not CONTEXT_RE.fullmatch(args.build_context_sha256):
        raise ValueError("build context must be a SHA-256 digest")
    document = json.loads(args.inspect_config.read_text(encoding="utf-8"))
    labels = labels_from(document)
    expected = {
        "org.opencontainers.image.source": args.source_url,
        "org.opencontainers.image.revision": args.source_sha,
        "org.praxis.build-context.sha256": args.build_context_sha256,
    }
    mismatches = [key for key, value in expected.items() if labels.get(key) != value]
    if mismatches:
        raise ValueError("required OCI provenance labels are missing or mismatched: " + ", ".join(mismatches))
    result = {
        "schema": "praxis-extproc.image-provenance/v1",
        "image": args.image,
        "source_url": args.source_url,
        "source_sha": args.source_sha,
        "build_context_sha256": args.build_context_sha256,
        "oci_source_label": labels["org.opencontainers.image.source"],
        "oci_revision_label": labels["org.opencontainers.image.revision"],
        "oci_build_context_label": labels["org.praxis.build-context.sha256"],
        "verified": True,
    }
    if args.runtime_image_id:
        runtime_digest = re.search(r"sha256:[0-9a-f]{64}$", args.runtime_image_id)
        if runtime_digest is None:
            raise ValueError("runtime imageID must end in a sha256 digest")
        result["runtime_image_id"] = args.runtime_image_id
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--source-url", required=True)
    parser.add_argument("--source-sha", required=True)
    parser.add_argument("--build-context-sha256", required=True)
    parser.add_argument("--inspect-config", type=Path, required=True)
    parser.add_argument("--runtime-image-id", default="")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        result = verify(args)
        if args.output.exists():
            raise ValueError("image provenance evidence already exists")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.chmod(args.output, 0o600)
        print(f"immutable image provenance verified: {args.image}")
        return 0
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"image provenance blocked: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
