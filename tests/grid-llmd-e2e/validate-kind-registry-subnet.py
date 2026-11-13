#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Fail closed when a run-owned Docker registry subnet overlaps local routes."""

from __future__ import annotations

import argparse
import ipaddress
import json
from pathlib import Path
from typing import Any


def _network(value: str, label: str) -> ipaddress.IPv4Network:
    try:
        network = ipaddress.ip_network(value, strict=False)
    except ValueError as exc:
        raise ValueError(f"{label} is not a valid IPv4 CIDR: {value}") from exc
    if not isinstance(network, ipaddress.IPv4Network):
        raise ValueError(f"{label} must be IPv4")
    return network


def validate_registry_network(
    subnet: str,
    gateway: str,
    docker_networks: list[dict[str, Any]],
    host_routes: list[dict[str, Any]],
    pod_cidr: str = "10.244.0.0/16",
    service_cidr: str = "10.96.0.0/12",
) -> dict[str, Any]:
    selected = _network(subnet, "registry subnet")
    if selected.prefixlen > 29:
        raise ValueError("registry subnet must provide at least six usable IPv4 addresses")
    try:
        gateway_address = ipaddress.ip_address(gateway)
    except ValueError as exc:
        raise ValueError(f"registry gateway is not an IPv4 address: {gateway}") from exc
    if not isinstance(gateway_address, ipaddress.IPv4Address):
        raise ValueError("registry gateway must be IPv4")
    if gateway_address not in selected or gateway_address in (selected.network_address, selected.broadcast_address):
        raise ValueError("registry gateway must be a usable address inside the registry subnet")

    reserved = [
        ("Kind pod CIDR", _network(pod_cidr, "Kind pod CIDR")),
        ("Kubernetes service CIDR", _network(service_cidr, "Kubernetes service CIDR")),
    ]
    for label, network in reserved:
        if selected.overlaps(network):
            raise ValueError(f"registry subnet overlaps {label}: {network}")

    conflicts: list[str] = []
    for docker_network in docker_networks:
        name = str(docker_network.get("Name", "<unnamed>"))
        for config in (docker_network.get("IPAM", {}).get("Config") or []):
            candidate = config.get("Subnet")
            if candidate:
                try:
                    network = ipaddress.ip_network(candidate, strict=False)
                except ValueError as exc:
                    raise ValueError(f"Docker network {name} has an invalid subnet: {candidate}") from exc
                if not isinstance(network, ipaddress.IPv4Network):
                    continue
                if selected.overlaps(network):
                    conflicts.append(f"Docker network {name} ({network})")
    for route in host_routes:
        destination = route.get("dst")
        if not destination or destination == "default":
            continue
        network = _network(destination, "host route")
        if selected.overlaps(network):
            conflicts.append(f"host route {network} on {route.get('dev', '<unknown>')}")
    if conflicts:
        raise ValueError("registry subnet overlaps " + "; ".join(conflicts))

    return {
        "status": "PASS",
        "subnet": str(selected),
        "gateway": str(gateway_address),
        "docker_networks_checked": len(docker_networks),
        "host_routes_checked": len(host_routes),
        "kind_pod_cidr": pod_cidr,
        "kubernetes_service_cidr": service_cidr,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--subnet", required=True)
    parser.add_argument("--gateway", required=True)
    parser.add_argument("--docker-networks", required=True, type=Path)
    parser.add_argument("--host-routes", required=True, type=Path)
    parser.add_argument("--pod-cidr", default="10.244.0.0/16")
    parser.add_argument("--service-cidr", default="10.96.0.0/12")
    args = parser.parse_args()
    try:
        result = validate_registry_network(
            args.subnet,
            args.gateway,
            json.loads(args.docker_networks.read_text()),
            json.loads(args.host_routes.read_text()),
            args.pod_cidr,
            args.service_cidr,
        )
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        parser.error(str(exc))
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
