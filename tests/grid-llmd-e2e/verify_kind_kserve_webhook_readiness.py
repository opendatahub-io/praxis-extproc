#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Validate that the KServe webhook Service targets one converged controller Pod."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any


def load(path: str) -> dict[str, Any]:
    return json.loads(Path(path).read_text(encoding="utf-8"))


def require(condition: bool, message: str) -> None:
    if not condition:
        raise ValueError(message)


def ready(pod: dict[str, Any]) -> bool:
    conditions = pod.get("status", {}).get("conditions", [])
    return any(c.get("type") == "Ready" and c.get("status") == "True" for c in conditions)


def verify(
    deployment: dict[str, Any],
    pods: dict[str, Any],
    service: dict[str, Any],
    endpoint_slices: dict[str, Any],
    image_repository: str,
    image_tag: str,
    image_digest: str,
) -> dict[str, Any]:
    metadata = deployment.get("metadata", {})
    spec = deployment.get("spec", {})
    status = deployment.get("status", {})
    generation = metadata.get("generation", 0)
    replicas = spec.get("replicas", 1)
    require(status.get("observedGeneration", 0) >= generation, "Deployment has not observed its current generation")
    require(replicas == 1, f"expected one webhook controller replica, found {replicas}")
    require(status.get("readyReplicas", 0) == replicas, "Deployment Ready replicas do not match desired replicas")
    require(status.get("availableReplicas", 0) == replicas, "Deployment Available replicas do not match desired replicas")

    selector = spec.get("selector", {}).get("matchLabels", {})
    require(bool(selector), "Deployment selector is empty")
    selected = pods.get("items", [])
    require(all(all(p.get("metadata", {}).get("labels", {}).get(k) == v for k, v in selector.items()) for p in selected),
            "Pod snapshot contains an object outside the Deployment selector")
    require(len(selected) == replicas, f"expected exactly {replicas} selected Pod and no rollout overlap, found {len(selected)}")
    pod = selected[0]
    pod_meta = pod.get("metadata", {})
    pod_status = pod.get("status", {})
    require(not pod_meta.get("deletionTimestamp"), "a selected controller Pod is terminating")
    require(pod_status.get("phase") == "Running" and ready(pod), "the selected controller Pod is not Running and Ready")
    pod_ip = pod_status.get("podIP")
    pod_uid = pod_meta.get("uid")
    require(bool(pod_ip) and bool(pod_uid), "the Ready controller Pod is missing its IP or UID")

    spec_containers = {c.get("name"): c for c in pod.get("spec", {}).get("containers", [])}
    runtime_containers = {c.get("name"): c for c in pod_status.get("containerStatuses", [])}
    desired_image = f"{image_repository}:{image_tag}"
    require(spec_containers.get("manager", {}).get("image") == desired_image, "controller Pod image reference differs from the locked image")
    manager_status = runtime_containers.get("manager", {})
    require(manager_status.get("ready") is True, "controller manager container is not Ready")
    require(str(manager_status.get("imageID", "")).endswith(image_digest), "controller manager runtime imageID differs from the locked digest")

    service_meta = service.get("metadata", {})
    service_spec = service.get("spec", {})
    require(service_meta.get("name") == "llmisvc-webhook-server-service" and service_meta.get("namespace") == "kserve",
            "webhook Service identity is not the KServe namespace Service")
    service_ports = service_spec.get("ports", [])
    require(any(p.get("port") == 443 and p.get("targetPort") in ("webhook-server", 9443) for p in service_ports),
            "webhook Service does not map HTTPS 443 to the controller webhook port")
    require(all(service_spec.get("selector", {}).get(k) == v for k, v in selector.items()),
            "webhook Service selector does not target the controller Deployment")

    slices = [s for s in endpoint_slices.get("items", [])
              if s.get("metadata", {}).get("labels", {}).get("kubernetes.io/service-name") == service_meta.get("name")]
    require(bool(slices), "webhook Service has no EndpointSlice")
    endpoints = [endpoint for endpoint_slice in slices for endpoint in endpoint_slice.get("endpoints", [])]
    require(len(endpoints) == 1, f"expected exactly one webhook endpoint with no rollout overlap, found {len(endpoints)}")
    endpoint = endpoints[0]
    conditions = endpoint.get("conditions", {})
    require(conditions.get("ready") is True and conditions.get("serving", True) is True,
            "webhook EndpointSlice endpoint is not Ready and Serving")
    require(conditions.get("terminating") is not True, "webhook EndpointSlice endpoint is terminating")
    require(endpoint.get("addresses") == [pod_ip], "webhook EndpointSlice address does not match the current controller Pod")
    require(endpoint.get("targetRef", {}).get("uid") == pod_uid, "webhook EndpointSlice target UID does not match the current controller Pod")
    ports = [p for endpoint_slice in slices for p in endpoint_slice.get("ports", [])]
    require(any(p.get("port") == 9443 for p in ports), "webhook EndpointSlice does not expose the controller HTTPS port 9443")

    return {
        "schema": "praxis-extproc.kserve-webhook-readiness/v1",
        "status": "PASS",
        "deployment": {"name": metadata.get("name"), "generation": generation,
                       "observedGeneration": status.get("observedGeneration"), "replicas": replicas},
        "pod": {"name": pod_meta.get("name"), "uid": pod_uid, "podIP": pod_ip,
                "image": desired_image, "imageID": manager_status.get("imageID")},
        "webhookService": {"name": service_meta.get("name"), "namespace": service_meta.get("namespace"),
                           "clusterIP": service_spec.get("clusterIP"), "endpointCount": len(endpoints),
                           "endpointPort": 9443},
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--deployment", required=True)
    parser.add_argument("--pods", required=True)
    parser.add_argument("--service", required=True)
    parser.add_argument("--endpointslices", required=True)
    parser.add_argument("--image-repository", required=True)
    parser.add_argument("--image-tag", required=True)
    parser.add_argument("--image-digest", required=True)
    args = parser.parse_args()
    try:
        result = verify(load(args.deployment), load(args.pods), load(args.service), load(args.endpointslices),
                        args.image_repository, args.image_tag, args.image_digest)
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        print(f"KServe webhook readiness: {exc}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout, sort_keys=True, indent=2)
    print()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
