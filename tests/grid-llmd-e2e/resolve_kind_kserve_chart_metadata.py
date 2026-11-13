#!/usr/bin/env python3
"""Read chart identities from one immutable KServe source worktree."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path


CHARTS = {
    "kserve-llmisvc-crd": "charts/kserve-llmisvc-crd/Chart.yaml",
    "kserve-llmisvc-resources": "charts/kserve-llmisvc-resources/Chart.yaml",
    "kserve-runtime-configs": "charts/kserve-runtime-configs/Chart.yaml",
}
VERSION_RE = re.compile(r"^v?[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")


def scalar_value(chart_file: Path, key: str) -> str:
    matches = re.findall(
        rf"^{re.escape(key)}:\s*([^\s#]+)\s*(?:#.*)?$",
        chart_file.read_text(encoding="utf-8"),
        flags=re.MULTILINE,
    )
    if len(matches) != 1:
        raise ValueError(f"{chart_file}: expected exactly one {key} scalar")
    return matches[0]


def resolve_chart_metadata(source_dir: Path, source_sha: str) -> dict[str, object]:
    if not re.fullmatch(r"[0-9a-f]{40}", source_sha):
        raise ValueError("KServe source SHA must be a full lowercase Git SHA")

    charts: dict[str, dict[str, str]] = {}
    for expected_name, relative_path in CHARTS.items():
        chart_file = source_dir / relative_path
        if not chart_file.is_file():
            raise ValueError(f"locked KServe source is missing {relative_path}")
        name = scalar_value(chart_file, "name")
        version = scalar_value(chart_file, "version")
        app_version = scalar_value(chart_file, "appVersion")
        if name != expected_name:
            raise ValueError(f"{relative_path}: expected chart {expected_name}, observed {name}")
        if not VERSION_RE.fullmatch(version):
            raise ValueError(f"{relative_path}: chart version is malformed: {version}")
        if not VERSION_RE.fullmatch(app_version):
            raise ValueError(f"{relative_path}: appVersion is malformed: {app_version}")
        charts[expected_name] = {
            "source_path": relative_path,
            "version": version,
            "app_version": app_version,
            "chart_yaml_sha256": hashlib.sha256(chart_file.read_bytes()).hexdigest(),
        }

    runtime_values = source_dir / "charts/kserve-runtime-configs/values.yaml"
    if not runtime_values.is_file():
        raise ValueError("locked KServe source is missing charts/kserve-runtime-configs/values.yaml")
    return {
        "schema": "praxis-extproc.kserve-chart-source/v1",
        "source_sha": source_sha,
        "charts": charts,
        "runtime_values_sha256": hashlib.sha256(runtime_values.read_bytes()).hexdigest(),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source-dir", required=True, type=Path)
    parser.add_argument("--source-sha", required=True)
    args = parser.parse_args()
    try:
        metadata = resolve_chart_metadata(args.source_dir, args.source_sha)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    print(json.dumps(metadata, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
