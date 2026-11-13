#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render the source-built AIGC controller for the native Kind lane."""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Any

import yaml

DEFAULT_MANIFEST_PATH = "/config/manifests/kind-native-praxis-extproc"


def render(
    source: str,
    run_id: str,
    controller_image: str,
    extproc_image: str,
    manifest_configmap: dict[str, Any] | None = None,
    manifest_path: str | None = None,
) -> str:
    documents = [doc for doc in yaml.safe_load_all(source) if isinstance(doc, dict)]
    documents = [doc for doc in documents if doc.get("kind") != "ServiceMonitor"]
    deployments = [doc for doc in documents if doc.get("kind") == "Deployment"
                   and doc.get("metadata", {}).get("name") == "ai-gateway-controller"]
    if len(deployments) != 1:
        raise ValueError(f"expected exactly one ai-gateway-controller Deployment, found {len(deployments)}")
    containers = deployments[0].get("spec", {}).get("template", {}).get("spec", {}).get("containers", [])
    managers = [container for container in containers if isinstance(container, dict) and container.get("name") == "manager"]
    if len(managers) != 1:
        raise ValueError(f"expected exactly one manager container, found {len(managers)}")
    manager: dict[str, Any] = managers[0]
    metrics_services = [doc for doc in documents if doc.get("kind") == "Service"
                        and doc.get("metadata", {}).get("name") == "ai-gateway-controller-metrics"]
    if len(metrics_services) != 1:
        raise ValueError(f"expected exactly one ai-gateway-controller-metrics Service, found {len(metrics_services)}")
    metrics_service = metrics_services[0]
    metrics_service.setdefault("metadata", {}).setdefault("annotations", {}).pop(
        "service.beta.openshift.io/serving-cert-secret-name", None
    )
    metrics_volumes = [volume for volume in deployments[0].get("spec", {}).get("template", {}).get("spec", {}).get("volumes", [])
                       if volume.get("name") == "metrics-certs"]
    if len(metrics_volumes) != 1 or metrics_volumes[0].get("secret", {}).get("secretName") != "ai-gateway-controller-metrics-tls":
        raise ValueError("source Deployment must mount ai-gateway-controller-metrics-tls as metrics-certs")
    manager["image"] = controller_image
    manager["imagePullPolicy"] = "Always"
    env = manager.setdefault("env", [])
    matches = [item for item in env if isinstance(item, dict) and item.get("name") == "RELATED_IMAGE_ODH_PRAXIS_EXTPROC_IMAGE"]
    if len(matches) != 1:
        raise ValueError(f"expected exactly one ExtProc image environment variable, found {len(matches)}")
    matches[0].pop("valueFrom", None)
    matches[0]["value"] = extproc_image
    if manifest_configmap is not None:
        if manifest_configmap.get("kind") != "ConfigMap" or not manifest_configmap.get("metadata", {}).get("name"):
            raise ValueError("manifest bundle must be a named ConfigMap")
        if manifest_configmap.get("immutable") is not True:
            raise ValueError("manifest bundle ConfigMap must be immutable")
        data = manifest_configmap.get("data", {})
        if not all(isinstance(data.get(key), str) and data[key] for key in ("kustomization.yaml", "resources.yaml")):
            raise ValueError("manifest bundle ConfigMap must contain kustomization.yaml and resources.yaml")
        manifest_path = manifest_path or DEFAULT_MANIFEST_PATH
        if not manifest_path.startswith("/config/manifests/"):
            raise ValueError("an absolute manifest path below /config/manifests is required with a bundle")
        manager_args = manager.setdefault("args", [])
        if not isinstance(manager_args, list):
            raise ValueError("AIGC manager args must be a list")
        cleaned_args: list[str] = []
        skip_manifest_value = False
        for arg in manager_args:
            if skip_manifest_value:
                skip_manifest_value = False
                continue
            if arg == "--manifest-path":
                skip_manifest_value = True
                continue
            if isinstance(arg, str) and arg.startswith("--manifest-path="):
                continue
            cleaned_args.append(arg)
        cleaned_args.append(f"--manifest-path={manifest_path}")
        manager["args"] = cleaned_args
        config_map_name = manifest_configmap["metadata"]["name"]
        volume_name = "kind-praxis-manifests"
        pod_spec = deployments[0].setdefault("spec", {}).setdefault("template", {}).setdefault("spec", {})
        volumes = pod_spec.setdefault("volumes", [])
        if any(volume.get("name") == volume_name for volume in volumes):
            raise ValueError(f"controller already has a volume named {volume_name}")
        volumes.append({"name": volume_name, "configMap": {"name": config_map_name}})
        mounts = manager.setdefault("volumeMounts", [])
        if any(mount.get("name") == volume_name for mount in mounts):
            raise ValueError(f"manager already has a volume mount named {volume_name}")
        mounts.append({"name": volume_name, "mountPath": manifest_path, "readOnly": True})
    deployment_index = documents.index(deployments[0])
    documents.insert(deployment_index, {
        "apiVersion": "cert-manager.io/v1",
        "kind": "Certificate",
        "metadata": {
            "name": "ai-gateway-controller-metrics-kind",
            "namespace": metrics_service["metadata"].get("namespace", "opendatahub"),
        },
        "spec": {
            "secretName": "ai-gateway-controller-metrics-tls",
            "issuerRef": {"name": "rhai-ca-issuer", "kind": "ClusterIssuer", "group": "cert-manager.io"},
            "duration": "48h",
            "renewBefore": "12h",
            "dnsNames": [
                "ai-gateway-controller-metrics.opendatahub.svc",
                "ai-gateway-controller-metrics.opendatahub.svc.cluster.local",
            ],
        },
    })
    if manifest_configmap is not None:
        manifest_configmap = dict(manifest_configmap)
        manifest_configmap["metadata"] = dict(manifest_configmap.get("metadata", {}))
        manifest_configmap["metadata"]["namespace"] = "opendatahub"
        documents.insert(documents.index(deployments[0]), manifest_configmap)
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
    parser.add_argument("--extproc-image", required=True)
    parser.add_argument("--manifest-bundle-configmap", type=Path)
    parser.add_argument("--manifest-path")
    args = parser.parse_args()
    if args.input.resolve() == args.output.resolve():
        parser.error("input and output must be different paths")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    manifest_configmap = None
    if args.manifest_bundle_configmap is not None:
        configmaps = [
            doc for doc in yaml.safe_load_all(args.manifest_bundle_configmap.read_text(encoding="utf-8"))
            if isinstance(doc, dict)
        ]
        if len(configmaps) != 1:
            parser.error("manifest bundle input must contain exactly one ConfigMap")
        manifest_configmap = configmaps[0]
    elif args.manifest_path:
        parser.error("--manifest-path requires --manifest-bundle-configmap")
    manifest_path = args.manifest_path or DEFAULT_MANIFEST_PATH
    args.output.write_text(render(args.input.read_text(encoding="utf-8"), args.run_id,
                                  args.controller_image, args.extproc_image,
                                  manifest_configmap, manifest_path if manifest_configmap else None), encoding="utf-8")
    print("rendered current-source AIGC controller for native Kind lane")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
