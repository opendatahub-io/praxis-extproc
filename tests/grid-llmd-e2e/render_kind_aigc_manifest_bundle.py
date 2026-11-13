#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render current AIGC payload manifests for the Kind serving-certificate contract."""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import ssl
import subprocess
from typing import Any

import yaml


EXCLUDED_PORT = "9004"
PROCESSOR_DEPLOYMENTS = {"payload-pre-processing", "payload-processing"}
PROCESSOR_SECRETS = {
    "payload-pre-processing": "payload-pre-processing-tls",
    "payload-processing": "payload-processing-tls",
}
TENANT_POOLS = ("pool-a", "pool-b")
PROCESSOR_CONFIGS = {
    "payload-pre-processing": "pre-extproc.yaml",
    "payload-processing": "extproc.yaml",
}
SERVING_CERT_ANNOTATION = "service.beta.openshift.io/serving-cert-secret-name"
TRUST_SECRET_NAME = "payload-extproc-serving-ca"
GATEWAY_LABEL = "gateway.networking.k8s.io/gateway-name"
GATEWAY_NAME = "maas-default-gateway"
SERVICE_CA_PATH = "/var/run/secrets/kubernetes.io/serviceaccount/service-ca.crt"


def service_fqdn(service: str, namespace: str) -> str:
    return f"{service}.{namespace}.svc.cluster.local"


def _map(value: Any, description: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError(f"{description} must be an object")
    return value


def _named_resources(documents: list[dict[str, Any]], kind: str, names: set[str]) -> dict[str, dict[str, Any]]:
    found = {
        doc.get("metadata", {}).get("name"): doc
        for doc in documents
        if doc.get("kind") == kind and doc.get("metadata", {}).get("name") in names
    }
    if set(found) != names:
        raise ValueError(f"source bundle must contain {kind}s {sorted(names)}; found {sorted(found)}")
    return found


def _patch_deployments(documents: list[dict[str, Any]]) -> None:
    deployments = _named_resources(documents, "Deployment", PROCESSOR_DEPLOYMENTS)
    for deployment_name, deployment in deployments.items():
        pod_spec = deployment.get("spec", {}).get("template", {}).get("spec", {})
        containers = pod_spec.get("containers", [])
        if len(containers) != 1:
            raise ValueError(f"{deployment_name} must have exactly one application container")
        container = containers[0]
        ports = [str(port.get("containerPort")) for port in container.get("ports", []) if isinstance(port, dict)]
        if EXCLUDED_PORT not in ports:
            raise ValueError(f"{deployment_name} does not expose ExtProc port {EXCLUDED_PORT}")

        mounts = [mount for mount in container.get("volumeMounts", []) if isinstance(mount, dict)]
        if not any(mount.get("name") == "tls" and mount.get("mountPath") == "/etc/tls/private" for mount in mounts):
            raise ValueError(f"{deployment_name} must mount its TLS Secret at /etc/tls/private")
        volumes = [volume for volume in pod_spec.get("volumes", []) if isinstance(volume, dict)]
        tls_volumes = [volume for volume in volumes if volume.get("name") == "tls"]
        if len(tls_volumes) != 1:
            raise ValueError(f"{deployment_name} must have exactly one TLS volume")
        secret = _map(tls_volumes[0].get("secret"), f"{deployment_name} TLS volume secret")
        expected_secret = PROCESSOR_SECRETS[deployment_name]
        if secret.get("secretName") != expected_secret:
            raise ValueError(f"{deployment_name} TLS volume must reference {expected_secret}")

        annotations = (
            deployment.setdefault("spec", {})
            .setdefault("template", {})
            .setdefault("metadata", {})
            .setdefault("annotations", {})
        )
        current = annotations.get("traffic.sidecar.istio.io/excludeInboundPorts")
        if current not in (None, EXCLUDED_PORT):
            raise ValueError(f"{deployment_name} already excludes different inbound ports: {current!r}")
        annotations["traffic.sidecar.istio.io/excludeInboundPorts"] = EXCLUDED_PORT


def _patch_services(documents: list[dict[str, Any]]) -> None:
    services = _named_resources(documents, "Service", PROCESSOR_DEPLOYMENTS)
    for service_name, service in services.items():
        metadata = service.setdefault("metadata", {})
        annotations = metadata.setdefault("annotations", {})
        annotation_value = annotations.get(SERVING_CERT_ANNOTATION)
        expected_secret = PROCESSOR_SECRETS[service_name]
        if annotation_value not in (None, expected_secret):
            raise ValueError(f"{service_name} OpenShift serving-cert annotation has unexpected Secret {annotation_value!r}")
        annotations.pop(SERVING_CERT_ANNOTATION, None)
        if not annotations:
            metadata.pop("annotations", None)


def _patch_plugin_configs(documents: list[dict[str, Any]]) -> None:
    configs = _named_resources(documents, "ConfigMap", {"payload-processing-plugins"})
    data = _map(configs["payload-processing-plugins"].get("data"), "payload-processing-plugins data")
    for deployment_name, config_name in PROCESSOR_CONFIGS.items():
        raw_config = data.get(config_name)
        if not isinstance(raw_config, str):
            raise ValueError(f"payload-processing-plugins is missing {config_name}")
        config = _map(yaml.safe_load(raw_config), f"{config_name} configuration")
        server = _map(config.get("server"), f"{config_name} server configuration")
        tls = _map(server.get("tls"), f"{config_name} TLS configuration")
        if tls.get("mode") != "provided" or tls.get("cert_path") != "/etc/tls/private/tls.crt" or tls.get("key_path") != "/etc/tls/private/tls.key":
            raise ValueError(f"{deployment_name} TLS configuration does not match its mounted certificate and key")


def _patch_destination_rules(documents: list[dict[str, Any]], payload_namespace: str) -> None:
    rules = _named_resources(documents, "DestinationRule", PROCESSOR_DEPLOYMENTS)
    for service_name, rule in rules.items():
        fqdn = service_fqdn(service_name, payload_namespace)
        source_fqdn = service_fqdn(service_name, "openshift-ingress")
        spec = _map(rule.setdefault("spec", {}), f"{service_name} DestinationRule spec")
        policy = _map(spec.get("trafficPolicy"), f"{service_name} DestinationRule trafficPolicy")
        tls = _map(policy.get("tls"), f"{service_name} DestinationRule TLS policy")
        if tls.get("mode") != "SIMPLE":
            raise ValueError(f"{service_name} DestinationRule must retain SIMPLE TLS origination")
        if spec.get("host") != source_fqdn or tls.get("sni") != source_fqdn or tls.get("subjectAltNames") != [source_fqdn]:
            raise ValueError(f"{service_name} DestinationRule source host, SNI, and SAN must match the known OpenShift placeholder")
        if tls.get("caCertificates") != SERVICE_CA_PATH:
            raise ValueError(f"{service_name} DestinationRule has an unexpected trust source")
        spec["host"] = fqdn
        spec["workloadSelector"] = {"matchLabels": {GATEWAY_LABEL: GATEWAY_NAME}}
        tls.pop("caCertificates", None)
        tls["credentialName"] = TRUST_SECRET_NAME
        tls["sni"] = fqdn
        tls["subjectAltNames"] = [fqdn]


def _patch_envoy_filter(documents: list[dict[str, Any]], payload_namespace: str, ca_bundle: str) -> None:
    envoy_filter = _named_resources(documents, "EnvoyFilter", {"payload-processing"})["payload-processing"]
    patches = envoy_filter.get("spec", {}).get("configPatches", [])
    expected_clusters = {f"{service}-extproc" for service in PROCESSOR_DEPLOYMENTS}
    found: set[str] = set()

    for patch in patches:
        if not isinstance(patch, dict) or patch.get("applyTo") != "CLUSTER":
            continue
        value = patch.get("patch", {}).get("value", {})
        if not isinstance(value, dict) or value.get("name") not in expected_clusters:
            continue
        cluster_name = value["name"]
        service_name = cluster_name.removesuffix("-extproc")
        fqdn = service_fqdn(service_name, payload_namespace)
        socket_address = (
            value.get("load_assignment", {})
            .get("endpoints", [{}])[0]
            .get("lb_endpoints", [{}])[0]
            .get("endpoint", {})
            .get("address", {})
            .get("socket_address", {})
        )
        expected_source_host = f"{service_name}.openshift-ingress.svc.cluster.local"
        if not isinstance(socket_address, dict) or socket_address.get("address") != expected_source_host:
            raise ValueError(f"{cluster_name} source endpoint is not the expected OpenShift placeholder")
        socket_address["address"] = fqdn

        tls_context = value.get("transport_socket", {}).get("typed_config", {})
        expected_type = "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext"
        if tls_context.get("@type") != expected_type:
            raise ValueError(f"{cluster_name} must retain its Envoy upstream TLS context")
        tls_context["sni"] = fqdn
        validation = _map(
            tls_context.setdefault("common_tls_context", {}).setdefault("validation_context", {}),
            f"{cluster_name} TLS validation context",
        )
        expected_san = [{"san_type": "DNS", "matcher": {"exact": expected_source_host}}]
        if validation.get("match_typed_subject_alt_names") != expected_san:
            raise ValueError(f"{cluster_name} source SAN does not match the expected OpenShift placeholder")
        validation["match_typed_subject_alt_names"] = [
            {"san_type": "DNS", "matcher": {"exact": fqdn}}
        ]
        if validation.get("trusted_ca") != {"filename": SERVICE_CA_PATH}:
            raise ValueError(f"{cluster_name} has an unexpected Envoy trusted_ca source")
        validation["trusted_ca"] = {"inline_string": ca_bundle.rstrip() + "\n"}
        found.add(cluster_name)

    if found != expected_clusters:
        raise ValueError(f"payload EnvoyFilter must contain both TLS ExtProc clusters; found {sorted(found)}")


def render(
    source: str,
    run_id: str,
    namespace: str,
    name: str,
    *,
    payload_namespace: str,
    ca_bundle: str,
) -> dict[str, Any]:
    documents = [doc for doc in yaml.safe_load_all(source) if isinstance(doc, dict)]
    if not payload_namespace or "." in payload_namespace:
        raise ValueError("payload namespace must be a Kubernetes namespace name")
    if not ca_bundle.strip().startswith("-----BEGIN CERTIFICATE-----"):
        raise ValueError("payload CA bundle must be a PEM certificate")
    try:
        ssl.PEM_cert_to_DER_cert(ca_bundle)
    except ValueError as error:
        raise ValueError("payload CA bundle must contain a parseable PEM certificate") from error

    _patch_deployments(documents)
    _patch_services(documents)
    _patch_plugin_configs(documents)
    _patch_destination_rules(documents, payload_namespace)
    _patch_envoy_filter(documents, payload_namespace, ca_bundle)

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
                "praxis.kind/payload-ca-sha256": hashlib.sha256(ca_bundle.encode("utf-8")).hexdigest(),
            },
        },
        "immutable": True,
        "data": {"kustomization.yaml": kustomization, "resources.yaml": manifest_yaml},
    }


def render_certificates(run_id: str, payload_namespace: str) -> list[dict[str, Any]]:
    if not run_id or not payload_namespace or "." in payload_namespace:
        raise ValueError("run ID and a Kubernetes payload namespace are required")
    services = list(PROCESSOR_SECRETS.items())
    services.extend(
        (f"{service_name}-{pool}", f"{service_name}-{pool}-tls")
        for pool in TENANT_POOLS
        for service_name in PROCESSOR_SECRETS
    )
    return [
        {
            "apiVersion": "cert-manager.io/v1",
            "kind": "Certificate",
            "metadata": {
                "name": secret_name,
                "namespace": payload_namespace,
                "labels": {
                    "app.kubernetes.io/managed-by": "grid-llmd-kind",
                    "app.kubernetes.io/part-of": "grid-llmd-e2e",
                    "praxis.kind/run": run_id,
                },
            },
            "spec": {
                "secretName": secret_name,
                "secretTemplate": {
                    "labels": {
                        "app.kubernetes.io/managed-by": "grid-llmd-kind",
                        "app.kubernetes.io/part-of": "grid-llmd-e2e",
                        "praxis.kind/run": run_id,
                    }
                },
                "duration": "24h",
                "renewBefore": "4h",
                "dnsNames": [service_fqdn(service_name, payload_namespace)],
                "issuerRef": {"name": "rhai-ca-issuer", "kind": "ClusterIssuer", "group": "cert-manager.io"},
                "usages": ["digital signature", "key encipherment", "server auth"],
            },
        }
        for service_name, secret_name in services
    ]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--certificates-output", required=True, type=Path)
    parser.add_argument("--payload-ca-file", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--namespace", default="opendatahub")
    parser.add_argument("--payload-namespace", default="maas-system")
    parser.add_argument("--name", required=True)
    args = parser.parse_args()
    if not args.source_dir.is_dir():
        parser.error(f"source directory does not exist: {args.source_dir}")
    if not args.payload_ca_file.is_file():
        parser.error(f"payload CA file does not exist: {args.payload_ca_file}")
    result = subprocess.run(
        ["kustomize", "build", str(args.source_dir)],
        check=True,
        capture_output=True,
        text=True,
    )
    ca_bundle = args.payload_ca_file.read_text(encoding="utf-8")
    config_map = render(
        result.stdout,
        args.run_id,
        args.namespace,
        args.name,
        payload_namespace=args.payload_namespace,
        ca_bundle=ca_bundle,
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(yaml.safe_dump(config_map, sort_keys=False), encoding="utf-8")
    os.chmod(args.output, 0o600)
    args.certificates_output.parent.mkdir(parents=True, exist_ok=True)
    args.certificates_output.write_text(
        yaml.safe_dump_all(render_certificates(args.run_id, args.payload_namespace), explicit_start=True, sort_keys=False),
        encoding="utf-8",
    )
    os.chmod(args.certificates_output, 0o600)
    print(f"rendered immutable AIGC manifest bundle {args.namespace}/{args.name} and six run-scoped payload Certificates")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
