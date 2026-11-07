#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Check the live AIGC resources and Gateway xDS payload TLS contract."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
from pathlib import Path
from typing import Any


BASE_SERVICES = ("payload-pre-processing", "payload-processing")
GATEWAY_LABEL = "gateway.networking.k8s.io/gateway-name"
GATEWAY_NAME = "maas-default-gateway"
TRUST_SECRET = "payload-extproc-serving-ca"
TLS_TYPE = "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext"


def field(value: dict[str, Any], snake: str, camel: str) -> Any:
    return value.get(snake, value.get(camel))


def _data_source_pem(data_source: dict[str, Any]) -> str:
    value = field(data_source, "inline_string", "inlineString")
    if isinstance(value, str):
        return value
    encoded = field(data_source, "inline_bytes", "inlineBytes")
    if isinstance(encoded, str):
        return base64.b64decode(encoded, validate=True).decode("utf-8")
    raise ValueError("Gateway xDS trusted_ca is not an inline CA certificate")


def _walk_clusters(value: Any, name: str, found: list[dict[str, Any]]) -> None:
    if isinstance(value, dict):
        if value.get("name") == name and (
            "transport_socket" in value or "transportSocket" in value
        ):
            found.append(value)
        for child in value.values():
            _walk_clusters(child, name, found)
    elif isinstance(value, list):
        for child in value:
            _walk_clusters(child, name, found)


def _socket_address(cluster: dict[str, Any]) -> dict[str, Any]:
    assignment = field(cluster, "load_assignment", "loadAssignment")
    endpoints = assignment.get("endpoints", []) if isinstance(assignment, dict) else []
    if not endpoints:
        raise ValueError("Gateway xDS ExtProc cluster has no load assignment")
    lb_endpoints = field(endpoints[0], "lb_endpoints", "lbEndpoints")
    if not lb_endpoints:
        raise ValueError("Gateway xDS ExtProc cluster has no endpoint")
    endpoint = lb_endpoints[0].get("endpoint", {})
    address = endpoint.get("address", {})
    socket = field(address, "socket_address", "socketAddress")
    if not isinstance(socket, dict):
        raise ValueError("Gateway xDS ExtProc cluster does not use a socket address")
    return socket


def _tls_context(cluster: dict[str, Any]) -> dict[str, Any]:
    transport = field(cluster, "transport_socket", "transportSocket")
    typed = field(transport, "typed_config", "typedConfig") if isinstance(transport, dict) else None
    if not isinstance(typed, dict) or typed.get("@type") != TLS_TYPE:
        raise ValueError("Gateway xDS ExtProc cluster has no typed upstream TLS context")
    return typed


def check(
    destination_rules: list[dict[str, Any]],
    envoy_filter: dict[str, Any],
    xds_dump: dict[str, Any],
    ca_pem: str,
    namespace: str,
    pool: str | None = None,
) -> dict[str, Any]:
    if pool not in (None, "pool-a", "pool-b"):
        raise ValueError("pool must be pool-a, pool-b, or omitted for the shared payload Gateway")
    services = tuple(f"{service}-{pool}" if pool else service for service in BASE_SERVICES)
    gateway = f"grid-llmd-{pool}" if pool else GATEWAY_NAME
    filter_name = f"payload-processing-{pool}" if pool else "payload-processing"
    rules = {item.get("metadata", {}).get("name"): item for item in destination_rules}
    if len(destination_rules) != len(services) or set(rules) != set(services):
        raise ValueError("live DestinationRules for both payload Services are required")
    if envoy_filter.get("metadata", {}).get("name") != filter_name:
        raise ValueError(f"live EnvoyFilter must be {filter_name}")
    filter_labels = envoy_filter.get("spec", {}).get("workloadSelector", {}).get("labels", {})
    if filter_labels != {GATEWAY_LABEL: gateway}:
        raise ValueError(f"live EnvoyFilter is not scoped to Gateway {gateway}")
    ca_sha = hashlib.sha256(ca_pem.encode()).hexdigest()
    checks = []

    for service in services:
        processor = service.removesuffix(f"-{pool}") if pool else service
        fqdn = f"{service}.{namespace}.svc.cluster.local"
        rule = rules[service].get("spec", {})
        tls = rule.get("trafficPolicy", {}).get("tls", {})
        selector = rule.get("workloadSelector", {}).get("matchLabels", {})
        if rule.get("host") != fqdn or tls.get("mode") != "SIMPLE":
            raise ValueError(f"DestinationRule/{service} host or TLS mode differs from {fqdn}")
        if tls.get("sni") != fqdn or tls.get("subjectAltNames") != [fqdn]:
            raise ValueError(f"DestinationRule/{service} SNI/SAN does not match the Service DNS name")
        if tls.get("credentialName") != TRUST_SECRET or tls.get("caCertificates"):
            raise ValueError(f"DestinationRule/{service} does not use the run CA credential Secret")
        if selector != {GATEWAY_LABEL: gateway}:
            raise ValueError(f"DestinationRule/{service} credential is not scoped to Gateway {gateway}")

        clusters = []
        for patch in envoy_filter.get("spec", {}).get("configPatches", []):
            value = patch.get("patch", {}).get("value", {})
            if patch.get("applyTo") == "CLUSTER" and value.get("name") == f"{processor}-extproc":
                clusters.append(value)
        if len(clusters) != 1:
            raise ValueError(f"live EnvoyFilter must define exactly one {processor}-extproc cluster")
        source_cluster = clusters[0]
        source_socket = _socket_address(source_cluster)
        if source_socket.get("address") != fqdn or source_socket.get("port_value", source_socket.get("portValue")) != 9004:
            raise ValueError(f"live EnvoyFilter {service}-extproc endpoint does not match Service DNS and port 9004")
        source_tls = _tls_context(source_cluster)
        source_common = field(source_tls, "common_tls_context", "commonTlsContext")
        source_validation = field(source_common, "validation_context", "validationContext")
        source_trust = field(source_validation, "trusted_ca", "trustedCa")
        source_sans = field(source_validation, "match_typed_subject_alt_names", "matchTypedSubjectAltNames")
        if source_tls.get("sni") != fqdn or source_sans != [{"san_type": "DNS", "matcher": {"exact": fqdn}}]:
            raise ValueError(f"live EnvoyFilter {service}-extproc SNI/SAN does not match Service DNS")
        if _data_source_pem(source_trust).strip() != ca_pem.strip():
            raise ValueError(f"live EnvoyFilter {service}-extproc does not trust the run CA issuer")

        active_clusters: list[dict[str, Any]] = []
        _walk_clusters(xds_dump, f"{processor}-extproc", active_clusters)
        if len(active_clusters) != 1:
            raise ValueError(f"Gateway xDS must contain exactly one active {processor}-extproc cluster")
        active = active_clusters[0]
        active_socket = _socket_address(active)
        if active_socket.get("address") != fqdn or active_socket.get("port_value", active_socket.get("portValue")) != 9004:
            raise ValueError(f"Gateway xDS {service}-extproc cluster does not resolve the expected Service DNS endpoint")
        active_tls = _tls_context(active)
        active_common = field(active_tls, "common_tls_context", "commonTlsContext")
        active_validation = field(active_common, "validation_context", "validationContext")
        active_trust = field(active_validation, "trusted_ca", "trustedCa")
        active_sans = field(active_validation, "match_typed_subject_alt_names", "matchTypedSubjectAltNames")
        if active_tls.get("sni") != fqdn or active_sans != [{"san_type": "DNS", "matcher": {"exact": fqdn}}]:
            raise ValueError(f"active Gateway xDS {service}-extproc TLS uses unexpected SNI or SAN validation")
        if _data_source_pem(active_trust).strip() != ca_pem.strip():
            raise ValueError(f"active Gateway xDS {service}-extproc does not trust the run CA issuer")
        checks.append({"service": service, "cluster": f"{processor}-extproc", "dnsName": fqdn, "port": 9004, "sni": fqdn, "sanVerified": True, "caSha256": ca_sha})

    return {"status": "PASS", "gateway": gateway, "namespace": namespace, "services": checks}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination-rules", required=True, type=Path)
    parser.add_argument("--envoy-filter", required=True, type=Path)
    parser.add_argument("--xds-dump", required=True, type=Path)
    parser.add_argument("--ca-file", required=True, type=Path)
    parser.add_argument("--namespace", default="maas-system")
    parser.add_argument("--pool", choices=("pool-a", "pool-b"))
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    rules_raw = json.loads(args.destination_rules.read_text())
    rules = rules_raw.get("items", [])
    filter_raw = json.loads(args.envoy_filter.read_text())
    xds = json.loads(args.xds_dump.read_text())
    result = check(rules, filter_raw, xds, args.ca_file.read_text(), args.namespace, args.pool)
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
