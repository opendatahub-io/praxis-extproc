#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Add the Kind local registry host path to containerd's CRI registry table."""

from __future__ import annotations

import argparse
import os
import sys
import tomllib
from pathlib import Path


REGISTRY_TABLE = '[plugins."io.containerd.grpc.v1.cri".registry]'
REGISTRY_CONFIG_PATH = "/etc/containerd/certs.d"


def ensure_registry_config(text: str) -> str:
    try:
        parsed = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise ValueError(f"containerd config is not valid TOML: {error}") from error

    registry = (
        parsed.get("plugins", {})
        .get("io.containerd.grpc.v1.cri", {})
        .get("registry", {})
    )
    existing = registry.get("config_path")
    if existing == REGISTRY_CONFIG_PATH:
        return text
    if existing not in (None, ""):
        raise ValueError("containerd CRI registry config_path is already set to a different value")

    lines = text.splitlines(keepends=True)
    section_index = next(
        (index for index, line in enumerate(lines) if line.strip() == REGISTRY_TABLE),
        None,
    )
    if section_index is None:
        prefix = "" if not text or text.endswith("\n") else "\n"
        output = f'{text}{prefix}\n{REGISTRY_TABLE}\n  config_path = "{REGISTRY_CONFIG_PATH}"\n'
    else:
        section_end = next(
            (index for index in range(section_index + 1, len(lines)) if lines[index].lstrip().startswith("[")),
            len(lines),
        )
        path_index = next(
            (
                index
                for index in range(section_index + 1, section_end)
                if lines[index].split("=", 1)[0].strip() == "config_path"
            ),
            None,
        )
        assignment = f'  config_path = "{REGISTRY_CONFIG_PATH}"\n'
        if path_index is None:
            lines.insert(section_index + 1, assignment)
        else:
            lines[path_index] = assignment
        output = "".join(lines)

    try:
        verified = tomllib.loads(output)
    except tomllib.TOMLDecodeError as error:
        raise ValueError(f"updated containerd config is not valid TOML: {error}") from error
    value = (
        verified.get("plugins", {})
        .get("io.containerd.grpc.v1.cri", {})
        .get("registry", {})
        .get("config_path")
    )
    if value != REGISTRY_CONFIG_PATH:
        raise ValueError("containerd CRI registry config_path did not validate after update")
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("before", type=Path)
    parser.add_argument("after", type=Path)
    args = parser.parse_args()
    try:
        output = ensure_registry_config(args.before.read_text(encoding="utf-8"))
        args.after.write_text(output, encoding="utf-8")
        os.chmod(args.after, 0o600)
        print(f"containerd CRI registry config_path={REGISTRY_CONFIG_PATH} verified")
        return 0
    except (OSError, ValueError) as error:
        print(f"containerd registry configuration blocked: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
