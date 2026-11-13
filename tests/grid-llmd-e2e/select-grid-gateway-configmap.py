#!/usr/bin/env python3
"""Select the Grid config slot the Gateway chart must keep mounted."""

from __future__ import annotations

import re
import sys


_GRID_SLOT = re.compile(r"grid-consumer-config-[0-9a-f]{16}\Z")


def select_configmap(base: str, active: str, bootstrap: str) -> str:
    """Keep the active Grid slot after reconciliation; otherwise use the base."""
    if not base or not bootstrap:
        raise ValueError("base and bootstrap ConfigMap names are required")
    if active in ("", base, bootstrap):
        return base
    if _GRID_SLOT.fullmatch(active):
        return active
    raise ValueError("active ConfigMap is not the configured base, task bootstrap, or Grid slot")


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print(f"usage: {argv[0]} BASE ACTIVE BOOTSTRAP", file=sys.stderr)
        return 2
    try:
        print(select_configmap(argv[1], argv[2], argv[3]))
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
