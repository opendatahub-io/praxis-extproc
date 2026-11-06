#!/usr/bin/env python3
"""Validate the complete source-locked Kind image matrix before any build starts."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


IMAGE_SPECS = (
    ("kserve-llmisvc-controller", "kserve", "source-locks.json", "llmisvc-controller.Dockerfile", ""),
    ("maas-api", "maas", "maas-api-build-locks.json", "maas-api/Dockerfile", "maas-api"),
    ("maas-controller", "maas", "source-locks.json", "maas-controller/Dockerfile", ""),
    ("aigc-controller", "aigc", "source-locks.json", "Dockerfile", ""),
    ("praxis-ai", "praxis-ai", "source-locks.json", "Containerfile", ""),
    ("grid-gateway", "grid", "source-locks.json", "deploy/gateway/Containerfile", ""),
    ("grid-operator", "grid-operator", "source-locks.json", "deploy/operator/Containerfile", ""),
    ("epp", "epp", "source-locks.json", "Dockerfile.epp", ""),
    ("llm-d-sim", "llm-d-sim", "source-locks.json", "Dockerfile", ""),
    ("extproc", "extproc", "source-locks.json", "Containerfile", ""),
    ("payload-processing", "payload-processing", "source-locks.json", "Dockerfile", ""),
)


def load_record(lock_path: Path, component: str) -> dict[str, object]:
    try:
        lock = json.loads(lock_path.read_text(encoding="utf-8"))
        record = lock["components"][component]
    except (OSError, json.JSONDecodeError, KeyError, TypeError) as error:
        raise ValueError(f"missing or invalid source lock component {component!r} in {lock_path}") from error
    if not isinstance(record, dict):
        raise ValueError(f"source lock component {component!r} is not an object")
    return record


def create_plan(lock_dir: Path, maas_patch: Path) -> list[dict[str, str]]:
    result = []
    for image, source, lock_name, dockerfile, subdir in IMAGE_SPECS:
        record = load_record(lock_dir / lock_name, source)
        worktree = Path(str(record.get("worktree_path", ""))).resolve()
        context = worktree
        if subdir:
            relative = Path(subdir)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError(f"unsafe build context subdirectory for {image}: {subdir}")
            context = (worktree / relative).resolve()
            if worktree not in context.parents:
                raise ValueError(f"build context escapes locked worktree for {image}")
        if not context.is_dir():
            raise ValueError(f"locked build context is missing for {image}: {context}")
        containerfile = (worktree / dockerfile).resolve() if "/" in dockerfile else (context / dockerfile).resolve()
        if worktree not in containerfile.parents or not containerfile.is_file():
            raise ValueError(f"locked source has no expected Dockerfile for {image}: {containerfile}")
        if image == "maas-controller":
            expected_patch = record.get("source_patch_sha256")
            if not isinstance(expected_patch, str) or not expected_patch:
                raise ValueError("Kind MaaS controller image requires the source patch recorded in its lock")
            if not maas_patch.is_file():
                raise ValueError(f"MaaS controller patch is missing: {maas_patch}")
        result.append(
            {
                "image_component": image,
                "source_component": source,
                "source_lock": lock_name,
                "dockerfile": dockerfile,
                "build_context_subdir": subdir,
                "worktree_path": str(worktree),
            }
        )
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--lock-dir", required=True, type=Path)
    parser.add_argument("--maas-patch", required=True, type=Path)
    args = parser.parse_args()
    try:
        plan = create_plan(args.lock_dir, args.maas_patch)
    except ValueError as error:
        print(f"plan-kind-images: {error}", file=sys.stderr)
        return 2
    json.dump({"schema": "praxis-extproc.kind-image-plan/v1", "images": plan}, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
