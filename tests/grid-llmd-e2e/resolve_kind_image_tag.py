#!/usr/bin/env python3
"""Resolve a deployment tag from verified build metadata."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path


TAG_RE = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$")


def resolve_tag(
    metadata: dict[str, object],
    image_repository: str,
    component: str,
    requested_tag: str | None = None,
) -> str:
    if metadata.get("component") != component:
        raise ValueError("build metadata belongs to a different image component")

    deployed_image = metadata.get("deployed_image")
    if not isinstance(deployed_image, str) or not re.fullmatch(
        re.escape(image_repository) + r"@sha256:[0-9a-f]{64}", deployed_image
    ):
        raise ValueError("build metadata does not pin the requested deploy repository")

    build_tag = metadata.get("build_tag_for_push_only")
    if not isinstance(build_tag, str) or ":" not in build_tag:
        raise ValueError("build metadata has no published tag")
    pushed_repository, tag = build_tag.rsplit(":", 1)
    if not tag or not TAG_RE.fullmatch(tag):
        raise ValueError("published build tag is malformed")
    if "/" not in pushed_repository or "/" not in image_repository:
        raise ValueError("build and deploy image repositories must include a namespace")
    if pushed_repository.split("/", 1)[1] != image_repository.split("/", 1)[1]:
        raise ValueError("published tag belongs to a different repository path")
    if requested_tag is not None and requested_tag != tag:
        raise ValueError("requested deployment tag differs from the published build tag")
    return tag


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--metadata", required=True, type=Path)
    parser.add_argument("--image-repository", required=True)
    parser.add_argument("--component", required=True)
    parser.add_argument("--requested-tag")
    args = parser.parse_args()
    try:
        metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
        tag = resolve_tag(metadata, args.image_repository, args.component, args.requested_tag)
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        parser.error(str(error))
    print(tag)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
