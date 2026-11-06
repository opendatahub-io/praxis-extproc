#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Extract the AIGC Kind default Gateway fixture with run ownership."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

import yaml


RUN_ID_RE = re.compile(r"^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$")


def render(source: str, run_id: str) -> tuple[str, dict[str, Any]]:
    if not RUN_ID_RE.fullmatch(run_id):
        raise ValueError("run ID is not a valid Kubernetes label value")
    docs = [doc for doc in yaml.safe_load_all(source) if isinstance(doc, dict)]
    matches = [
        doc for doc in docs
        if doc.get("kind") == "Gateway"
        and doc.get("metadata", {}).get("name") == "maas-default-gateway"
        and doc.get("metadata", {}).get("namespace") == "maas-system"
    ]
    if len(matches) != 1:
        raise ValueError(f"expected one AIGC Kind maas-system/maas-default-gateway Gateway, found {len(matches)}")
    gateway = matches[0]
    spec = gateway.get("spec", {})
    if spec.get("gatewayClassName") != "istio":
        raise ValueError("default MaaS Gateway must use the source-supported Istio GatewayClass")
    listeners = spec.get("listeners", [])
    if len(listeners) != 1 or listeners[0].get("protocol") != "HTTP" or listeners[0].get("port") != 80:
        raise ValueError("default MaaS Gateway listener contract changed")
    selector = listeners[0].get("allowedRoutes", {}).get("namespaces", {}).get("selector", {}).get("matchLabels", {})
    if selector.get("local-env.opendatahub.io/gateway-tenant") != "models-as-a-service":
        raise ValueError("default MaaS Gateway must admit only the default tenant namespace selector")
    metadata = gateway.setdefault("metadata", {})
    labels = metadata.setdefault("labels", {})
    labels["praxis.kind/run"] = run_id
    labels["app.kubernetes.io/managed-by"] = "grid-llmd-kind"
    output = yaml.safe_dump(gateway, sort_keys=False)
    summary = {
        "schema": "praxis-extproc.kind-maas-gateway-render/v1",
        "run_id": run_id,
        "source_fixture": "test/kind-env/manifests/30-gateway.yaml",
        "resource": "Gateway/maas-default-gateway",
        "namespace": "maas-system",
        "gateway_class": "istio",
        "listener": {"name": listeners[0].get("name"), "protocol": "HTTP", "port": 80},
        "tenant_selector": selector,
        "generated_services_are_created_by_gateway_controller": True,
        "alias_service_is_created_after_current_gateway_uid_and_ready_endpoint_validation": True,
    }
    return output, summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--summary", required=True, type=Path)
    args = parser.parse_args()
    if args.input.resolve() in {args.output.resolve(), args.summary.resolve()} or args.output.resolve() == args.summary.resolve():
        parser.error("input, output, and summary must use different paths")
    manifest, summary = render(args.input.read_text(encoding="utf-8"), args.run_id)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.summary.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(manifest, encoding="utf-8")
    args.summary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print("rendered only the run-owned source AIGC default MaaS Gateway")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
