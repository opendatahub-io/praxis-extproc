#!/usr/bin/env python3
"""Validate a Grid-generated Gateway config without writing its contents to evidence."""

from __future__ import annotations

import hashlib
import json
import re
import sys
from pathlib import Path


def fail(message: str) -> int:
    print(message, file=sys.stderr)
    return 1


def main() -> int:
    if len(sys.argv) != 2:
        return fail("usage: validate-generated-praxis-config.py EXPECTED_ENDPOINTS.json")
    endpoint_path = Path(sys.argv[1])
    try:
        expected = json.loads(endpoint_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        return fail(f"could not read expected endpoint inventory: {exc}")
    if not isinstance(expected, list) or len(expected) != 2:
        return fail("expected endpoint inventory must contain exactly two pool endpoints")
    if any(
        not isinstance(entry, dict)
        or not isinstance(entry.get("cluster"), str)
        or not entry["cluster"]
        or not isinstance(entry.get("address"), str)
        or not entry["address"]
        for entry in expected
    ):
        return fail("each expected endpoint must contain a non-empty cluster and address")
    clusters = [entry["cluster"] for entry in expected]
    addresses = [entry["address"] for entry in expected]
    if len(set(clusters)) != 2 or len(set(addresses)) != 2:
        return fail("expected pool endpoints must have distinct cluster names and addresses")

    config = sys.stdin.read()
    config_hash = hashlib.sha256(config.encode("utf-8")).hexdigest()
    if not config.strip():
        return fail("Grid-generated Praxis config is empty")
    if "/etc/praxis/routing/routing-overlay.json" not in config:
        return fail("Grid-generated Praxis config does not read the Grid routing overlay")
    reload_enabled = re.search(
        r"(?m)^[ \t]*reload:[ \t]*\r?\n[ \t]+enabled:[ \t]+true(?:[ \t]*$)", config
    )
    if not reload_enabled:
        return fail("Grid-generated Praxis config does not enable overlay reload")
    credential_field = re.search(
        r"(?im)^\s*(?:authorization|api[_-]?key|password|bearer[_-]?token|access[_-]?token)\s*:",
        config,
    )
    if credential_field:
        return fail("Grid-generated Praxis config contains a credential-like field")
    absent = [
        entry["cluster"]
        for entry in expected
        if entry["cluster"] not in config or entry["address"] not in config
    ]
    if absent:
        return fail("Grid-generated Praxis config is missing one or more expected pool endpoints")

    print(
        json.dumps(
            {
                "status": "valid",
                "sha256": config_hash,
                "byteLength": len(config.encode("utf-8")),
                "overlayPath": "/etc/praxis/routing/routing-overlay.json",
                "overlayReloadEnabled": True,
                "expectedEndpoints": [
                    {"cluster": entry["cluster"], "address": entry["address"]}
                    for entry in expected
                ],
                "credentialLikeFields": [],
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
