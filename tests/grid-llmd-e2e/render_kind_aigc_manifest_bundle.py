#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build an immutable AIGC Praxis manifest bundle for Kind sidecar TLS."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import subprocess
from typing import Any

import yaml


EXCLUDED_PORT = "9004"
PROCESSOR_DEPLOYMENTS = {"payload-pre-processing", "payload-processing"}


def render(source: str, run_id: str, namespace: str, name: str) -> dict[str, Any]:
    documents = [doc for doc in yaml.safe_load_all(source) if isinstance(doc, dict)]
    deployments = {
        doc.get("metadata", {}).get("name"): doc
        for doc in documents
        if doc.get("kind") == "Deployment"
        and doc.get("metadata", {}).get("name") in PROCESSOR_DEPLOYMENTS
    }
    if set(deployments) != PROCESSOR_DEPLOYMENTS:
        missing = sorted(PROCESSOR_DEPLOYMENTS - set(deployments))
        raise ValueError(f"source bundle must contain both AIGC processor Deployments; missing {missing}")

    for deployment_name, deployment in deployments.items():
        pod_spec = deployment.get("spec", {}).get("template", {}).get("spec", {})
        containers = pod_spec.get("containers", [])
        if len(containers) != 1:
            raise ValueError(f"{deployment_name} must have exactly one application container")
        ports = [
            str(port.get("containerPort"))
            for port in containers[0].get("ports", [])
            if isinstance(port, dict)
        ]
        if EXCLUDED_PORT not in ports:
            raise ValueError(f"{deployment_name} does not expose ExtProc port {EXCLUDED_PORT}")
        annotations = deployment.setdefault("spec", {}).setdefault("template", {}).setdefault("metadata", {}).setdefault("annotations", {})
        current = annotations.get("traffic.sidecar.istio.io/excludeInboundPorts")
        if current not in (None, EXCLUDED_PORT):
            raise ValueError(f"{deployment_name} already excludes different inbound ports: {current!r}")
        annotations["traffic.sidecar.istio.io/excludeInboundPorts"] = EXCLUDED_PORT

    manifest_yaml = yaml.safe_dump_all(documents, explicit_start=True, sort_keys=False)
    kustomization = "apiVersion: kustomize.config.k8s.io/v1beta1\nkind: Kustomization\nresources:\n  - resources.yaml\n"
    return {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": {
                "app.kubernetes.io/managed-by": "grid-llmd-kind",
                "app.kubernetes.io/part-of": "grid-llmd-e2e",
                "praxis.kind/run": run_id,
            },
            "annotations": {
                "praxis.kind/source-manifest-sha256": hashlib.sha256(source.encode("utf-8")).hexdigest(),
            },
        },
        "immutable": True,
        "data": {
            "kustomization.yaml": kustomization,
            "resources.yaml": manifest_yaml,
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--namespace", default="opendatahub")
    parser.add_argument("--name", required=True)
    args = parser.parse_args()
    if not args.source_dir.is_dir():
        parser.error(f"source directory does not exist: {args.source_dir}")
    result = subprocess.run(
        ["kustomize", "build", str(args.source_dir)],
        check=True,
        capture_output=True,
        text=True,
    )
    config_map = render(result.stdout, args.run_id, args.namespace, args.name)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(yaml.safe_dump(config_map, sort_keys=False), encoding="utf-8")
    os.chmod(args.output, 0o600)
    print(f"rendered immutable AIGC manifest bundle {args.namespace}/{args.name}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
