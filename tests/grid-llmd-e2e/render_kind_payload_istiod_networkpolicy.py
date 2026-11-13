#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render a narrow Kind-only egress exception for payload processor xDS."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any

import yaml


RUN_ID_RE = re.compile(r"^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$")


def render(service: dict[str, Any], slices: dict[str, Any], run_id: str, namespace: str = "maas-system") -> tuple[str, dict[str, Any]]:
    if not RUN_ID_RE.fullmatch(run_id):
        raise ValueError("run ID is not a valid Kubernetes label value")
    metadata = service.get("metadata", {})
    if metadata.get("name") != "istiod" or metadata.get("namespace") != "istio-system":
        raise ValueError("expected the istio-system/istiod Service")
    selector = service.get("spec", {}).get("selector")
    if not isinstance(selector, dict) or not selector:
        raise ValueError("Istiod Service must have a non-empty Pod selector")
    ports = service.get("spec", {}).get("ports", [])
    if not any(port.get("port") == 15012 and port.get("protocol", "TCP") == "TCP" for port in ports):
        raise ValueError("Istiod Service must expose xDS on TCP/15012")
    endpoints = [
        endpoint
        for endpoint_slice in slices.get("items", [])
        if endpoint_slice.get("metadata", {}).get("labels", {}).get("kubernetes.io/service-name") == "istiod"
        for endpoint in endpoint_slice.get("endpoints", [])
        if endpoint.get("conditions", {}).get("ready") is True and endpoint.get("addresses")
    ]
    ready_addresses = sorted({address for endpoint in endpoints for address in endpoint.get("addresses", [])})
    if not ready_addresses:
        raise ValueError("Istiod has no ready EndpointSlice addresses")

    manifest = {
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": {
            "name": "grid-llmd-kind-allow-istiod-xds",
            "namespace": namespace,
            "labels": {
                "praxis.kind/run": run_id,
                "app.kubernetes.io/managed-by": "grid-llmd-kind",
                "app.kubernetes.io/part-of": "grid-llmd-e2e",
            },
        },
        "spec": {
            "podSelector": {"matchExpressions": [{
                "key": "app", "operator": "In", "values": ["payload-pre-processing", "payload-processing"]
            }]},
            "policyTypes": ["Egress"],
            "egress": [{
                "to": [{
                    "namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "istio-system"}},
                    "podSelector": {"matchLabels": selector},
                }],
                "ports": [{"protocol": "TCP", "port": 15012}],
            }],
        },
    }
    summary = {
        "schema": "praxis-extproc.kind-istiod-egress/v1",
        "run_id": run_id,
        "namespace": namespace,
        "istiod_service": "istio-system/istiod",
        "istiod_selector": selector,
        "istiod_ready_endpoint_count": len(ready_addresses),
        "istiod_ready_endpoint_ips": ready_addresses,
        "allowed_port": "TCP/15012",
        "selected_apps": ["payload-pre-processing", "payload-processing"],
    }
    return yaml.safe_dump(manifest, sort_keys=False), summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--service", required=True, type=Path)
    parser.add_argument("--endpointslices", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--summary", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--namespace", default="maas-system")
    args = parser.parse_args()
    if args.output.resolve() == args.summary.resolve():
        parser.error("manifest and summary must use different paths")
    manifest, summary = render(json.loads(args.service.read_text()), json.loads(args.endpointslices.read_text()),
                              args.run_id, args.namespace)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.summary.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(manifest, encoding="utf-8")
    args.summary.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print("rendered a scoped TCP/15012 egress exception for the ready Istiod Service")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
