#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render the MaaS service alias expected by the xKS MaaS API."""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Any

import yaml


GATEWAY_NAME = "maas-default-gateway"
GENERATED_SERVICE_NAME = "maas-default-gateway-istio"
RUN_LABEL = "praxis.kind/run"
GATEWAY_LABEL = "gateway.networking.k8s.io/gateway-name"


def render(gateway: dict[str, Any], istio_service: dict[str, Any], slices: dict[str, Any], run_id: str) -> dict[str, Any]:
    """Return a same-namespace alias after validating the current Istio Gateway endpoint."""
    metadata = gateway.get("metadata", {})
    namespace = metadata.get("namespace")
    uid = metadata.get("uid")
    generation = metadata.get("generation")
    if gateway.get("kind") != "Gateway" or metadata.get("name") != GATEWAY_NAME:
        raise ValueError("expected the maas-default-gateway Gateway")
    if not isinstance(namespace, str) or not namespace or not isinstance(uid, str) or not uid:
        raise ValueError("Gateway namespace and UID are required")
    if metadata.get("labels", {}).get(RUN_LABEL) != run_id:
        raise ValueError("Gateway is not owned by this run")
    conditions = gateway.get("status", {}).get("conditions", [])
    accepted = [c for c in conditions if c.get("type") == "Accepted"]
    if len(accepted) != 1 or accepted[0].get("status") != "True" or accepted[0].get("observedGeneration") != generation:
        raise ValueError("Gateway Accepted condition is not true at the current generation")
    # Istio's Kind GatewayClass creates a LoadBalancer Service whose external
    # hostname remains Pending. AIGC's Kind harness therefore gates on the
    # current listener conditions and Ready internal endpoint, and supplies
    # the in-cluster hostname in Gateway status after those checks.
    listeners = gateway.get("status", {}).get("listeners", [])
    selected = [listener for listener in listeners if listener.get("name") == "http"]
    if len(selected) != 1:
        raise ValueError("Gateway has no unique default HTTP listener status")
    for condition_type in ("Accepted", "Programmed", "ResolvedRefs"):
        matches = [c for c in selected[0].get("conditions", []) if c.get("type") == condition_type]
        if len(matches) != 1 or matches[0].get("status") != "True" or matches[0].get("observedGeneration") != generation:
            raise ValueError(f"Gateway HTTP listener {condition_type} condition is not true at the current generation")
    expected_host = f"{GENERATED_SERVICE_NAME}.{namespace}.svc.cluster.local"
    addresses = gateway.get("status", {}).get("addresses", [])
    if addresses != [{"type": "Hostname", "value": expected_host}]:
        raise ValueError("Gateway Kind status address does not match its internal generated Service")

    service_meta = istio_service.get("metadata", {})
    if istio_service.get("kind") != "Service" or service_meta.get("name") != GENERATED_SERVICE_NAME:
        raise ValueError("expected the Istio generated Gateway Service")
    if service_meta.get("namespace") != namespace:
        raise ValueError("Istio Gateway Service is in a different namespace")
    if not any(
        ref.get("kind") == "Gateway" and ref.get("name") == GATEWAY_NAME and ref.get("uid") == uid
        for ref in service_meta.get("ownerReferences", [])
    ):
        raise ValueError("Istio Gateway Service does not reference the current Gateway UID")
    selector = istio_service.get("spec", {}).get("selector")
    if selector != {GATEWAY_LABEL: GATEWAY_NAME}:
        raise ValueError("Istio Gateway Service selector is unexpected")
    service_spec = istio_service.get("spec", {})
    if service_spec.get("type", "ClusterIP") not in {"ClusterIP", "LoadBalancer"} or not service_spec.get("clusterIP"):
        raise ValueError("Istio MaaS Gateway Service must retain an internal ClusterIP")

    endpoints = [
        endpoint
        for endpoint_slice in slices.get("items", [])
        for endpoint in endpoint_slice.get("endpoints", [])
        if endpoint.get("conditions", {}).get("ready") is True
    ]
    ports = [
        port
        for endpoint_slice in slices.get("items", [])
        for port in endpoint_slice.get("ports", [])
        if port.get("port") == 80
    ]
    if not endpoints or not ports:
        raise ValueError("Istio Gateway Service has no Ready HTTP endpoint on port 80")

    return {
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": GATEWAY_NAME,
            "namespace": namespace,
            "labels": {
                RUN_LABEL: run_id,
                "app.kubernetes.io/managed-by": "grid-llmd-kind",
                GATEWAY_LABEL: GATEWAY_NAME,
            },
            "ownerReferences": [{
                "apiVersion": gateway.get("apiVersion", "gateway.networking.k8s.io/v1"),
                "kind": "Gateway",
                "name": GATEWAY_NAME,
                "uid": uid,
                "controller": False,
                "blockOwnerDeletion": False,
            }],
        },
        "spec": {
            "type": "ClusterIP",
            "selector": selector,
            "ports": [
                {"name": "http", "port": 80, "protocol": "TCP", "targetPort": 80},
                {"name": "https", "port": 443, "protocol": "TCP", "targetPort": 80},
            ],
        },
    }


def read_json(path: Path) -> dict[str, Any]:
    import json

    return json.loads(path.read_text(encoding="utf-8"))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway", required=True, type=Path)
    parser.add_argument("--istio-service", required=True, type=Path)
    parser.add_argument("--endpointslices", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if args.output.resolve() in {args.gateway.resolve(), args.istio_service.resolve(), args.endpointslices.resolve()}:
        parser.error("output must be different from every input")
    result = render(read_json(args.gateway), read_json(args.istio_service), read_json(args.endpointslices), args.run_id)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(yaml.safe_dump(result, sort_keys=False), encoding="utf-8")
    print("rendered run-owned MaaS Gateway alias with current Gateway UID and Ready Istio endpoint")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
