#!/usr/bin/env python3
"""Disable only grid.id so a first Helm release can establish Grid CRDs."""
from pathlib import Path
import sys


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: prepare-grid-operator-bootstrap-values.py INPUT OUTPUT", file=sys.stderr)
        return 2

    source = Path(sys.argv[1])
    destination = Path(sys.argv[2])
    lines = source.read_text(encoding="utf-8").splitlines()
    in_grid = False
    grid_sections = 0
    grid_ids = 0
    output: list[str] = []

    for line in lines:
        if line == "grid:":
            in_grid = True
            grid_sections += 1
        elif line and not line[0].isspace():
            in_grid = False

        if in_grid and line.startswith("  id:"):
            grid_ids += 1
            line = '  id: ""'
        output.append(line)

    if grid_sections != 1 or grid_ids != 1:
        print("expected exactly one top-level grid section with one grid.id; refusing values rewrite", file=sys.stderr)
        return 1

    destination.write_text("\n".join(output) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
