#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render the locked MaaS controller for the native Kind topology."""

from __future__ import annotations

import argparse
import re
from pathlib import Path
from typing import Any

import yaml


NAMESPACES = {"opendatahub", "system", "openshift-ingress", "redhat-ods-applications"}
DEPLOYMENT_NAME = "maas-controller"
MANAGER = "manager"
CLUSTER_SCOPED_KINDS = {
    "ClusterRole",
    "ClusterRoleBinding",
    "CustomResourceDefinition",
    "ValidatingWebhookConfiguration",
    "ClusterIssuer",
}


def rewrite_namespaces(value: Any, key: str | None = None) -> Any:
    if isinstance(value, list):
        return [rewrite_namespaces(item, key) for item in value]
    if isinstance(value, dict):
        result = {}
        for key, item in value.items():
            result[key] = rewrite_namespaces(item, key)
        return result
    if isinstance(value, str):
        if value in NAMESPACES:
            return "maas-system"
        if key in {"commonName", "dnsNames", "serverName"}:
            return replace_dns_namespace(value)
        if key == "cert-manager.io/inject-ca-from":
            namespace, separator, remainder = value.partition("/")
            return f"maas-system/{remainder}" if separator and namespace in NAMESPACES else value
    return value


def replace_dns_namespace(value: str) -> str:
    names = "|".join(re.escape(namespace) for namespace in sorted(NAMESPACES, key=len, reverse=True))
    return re.sub(rf"(?<=\.)(?:{names})(?=\.|$)", "maas-system", value)


def set_env(container: dict[str, Any], name: str, value: str) -> None:
    env = container.setdefault("env", [])
    matches = [entry for entry in env if isinstance(entry, dict) and entry.get("name") == name]
    if len(matches) > 1:
        raise ValueError(f"duplicate {name} environment entries")
    if matches:
        matches[0].pop("valueFrom", None)
        matches[0]["value"] = value
    else:
        env.append({"name": name, "value": value})


def render(source: str, run_id: str, controller_image: str, api_image: str, payload_image: str) -> str:
    documents = [doc for doc in yaml.safe_load_all(source) if isinstance(doc, dict)]
    documents = [
        rewrite_namespaces(doc)
        for doc in documents
        if doc.get("kind") != "ServiceMonitor"
    ]
    for document in documents:
        if document.get("kind") not in CLUSTER_SCOPED_KINDS:
            document.setdefault("metadata", {}).setdefault("namespace", "maas-system")
    deployments = [
        doc for doc in documents
        if doc.get("kind") == "Deployment" and doc.get("metadata", {}).get("name") == DEPLOYMENT_NAME
    ]
    if len(deployments) != 1:
        raise ValueError(f"expected exactly one {DEPLOYMENT_NAME} Deployment, found {len(deployments)}")
    deployment = deployments[0]
    containers = deployment.get("spec", {}).get("template", {}).get("spec", {}).get("containers", [])
    managers = [container for container in containers if isinstance(container, dict) and container.get("name") == MANAGER]
    if len(managers) != 1:
        raise ValueError(f"expected exactly one {MANAGER} container, found {len(managers)}")
    manager = managers[0]
    manager.update({"image": controller_image, "imagePullPolicy": "Always"})
    set_env(manager, "GATEWAY_NAMESPACE", "maas-system")
    set_env(manager, "RELATED_IMAGE_ODH_MAAS_API_IMAGE", api_image)
    set_env(manager, "RELATED_IMAGE_ODH_AI_GATEWAY_PAYLOAD_PROCESSING_IMAGE", payload_image)
    for document in documents:
        if document.get("kind") == "Deployment" and document.get("metadata", {}).get("name") == DEPLOYMENT_NAME:
            for env_name, expected in (("GATEWAY_NAMESPACE", "maas-system"),):
                entries = [entry for entry in manager.get("env", []) if entry.get("name") == env_name]
                if len(entries) != 1 or entries[0].get("value") != expected:
                    raise ValueError(f"xKS source render must contain exactly one {env_name}={expected}")
            if not any(arg == "--metrics-secure=true" for arg in manager.get("args", [])):
                raise ValueError("xKS source render must retain secure controller metrics")
    for document in documents:
        metadata = document.setdefault("metadata", {})
        metadata.setdefault("labels", {})["praxis.kind/run"] = run_id
    return "\n---\n".join(yaml.safe_dump(doc, sort_keys=False).rstrip() for doc in documents) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--controller-image", required=True)
    parser.add_argument("--api-image", required=True)
    parser.add_argument("--payload-image", required=True)
    args = parser.parse_args()
    if args.input.resolve() == args.output.resolve():
        parser.error("input and output must be different paths")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(render(args.input.read_text(encoding="utf-8"), args.run_id,
                                  args.controller_image, args.api_image, args.payload_image), encoding="utf-8")
    print("rendered current-source MaaS controller for Kind; secrets are referenced, not serialized")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
