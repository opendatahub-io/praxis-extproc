#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Verify a live MaaS controller-runtime lease against its current Kind Pod."""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
from typing import Any


_DIGEST = re.compile(r"@sha256:[0-9a-f]{64}$")
_LOG_TIMESTAMP = re.compile(r"^(\d{4}-\d\d-\d\dT[^\s]+)\s+(.*)$")


def _parse_time(value: str) -> datetime:
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def _ready(pod: dict[str, Any]) -> bool:
    return (
        pod.get("metadata", {}).get("deletionTimestamp") is None
        and pod.get("status", {}).get("phase") == "Running"
        and any(
            condition.get("type") == "Ready" and condition.get("status") == "True"
            for condition in pod.get("status", {}).get("conditions", [])
        )
    )


def verify(
    pod: dict[str, Any],
    deployment: dict[str, Any],
    lease: dict[str, Any],
    logs: str,
    namespace: str,
    lease_name: str,
    now: datetime | None = None,
) -> dict[str, Any]:
    """Require a ready pinned Pod, a fresh holder prefix, and its own acquire log."""
    metadata = pod.get("metadata", {})
    pod_name = metadata.get("name")
    pod_uid = metadata.get("uid")
    if not pod_name or not pod_uid or metadata.get("namespace") != namespace or not _ready(pod):
        raise ValueError("controller Pod must be the named, non-terminating Running/Ready Pod in its namespace")

    dep_meta = deployment.get("metadata", {})
    dep_status = deployment.get("status", {})
    replicas = int(deployment.get("spec", {}).get("replicas", 1))
    if (
        dep_status.get("observedGeneration") != dep_meta.get("generation")
        or replicas != 1
        or int(dep_status.get("readyReplicas", 0)) != replicas
        or int(dep_status.get("availableReplicas", 0)) != replicas
    ):
        raise ValueError("MaaS controller Deployment has not observed its single ready replica")

    dep_containers = deployment.get("spec", {}).get("template", {}).get("spec", {}).get("containers", [])
    pod_containers = pod.get("spec", {}).get("containers", [])
    status_containers = pod.get("status", {}).get("containerStatuses", [])
    dep_manager = [container for container in dep_containers if container.get("name") == "manager"]
    pod_manager = [container for container in pod_containers if container.get("name") == "manager"]
    status_manager = [container for container in status_containers if container.get("name") == "manager"]
    if len(dep_manager) != 1 or len(pod_manager) != 1 or len(status_manager) != 1:
        raise ValueError("expected exactly one manager container in Deployment, Pod and Pod status")
    image = dep_manager[0].get("image", "")
    pod_image = pod_manager[0].get("image", "")
    image_id = status_manager[0].get("imageID", "")
    if not _DIGEST.search(image) or pod_image != image or image_id.removeprefix("docker-pullable://") != image:
        raise ValueError("current MaaS controller Pod image or runtime imageID does not match its immutable Deployment digest")

    started_at = status_manager[0].get("state", {}).get("running", {}).get("startedAt")
    if not started_at:
        raise ValueError("current MaaS controller manager container has no running start time")
    start_time = _parse_time(started_at)

    lease_meta = lease.get("metadata", {})
    lease_spec = lease.get("spec", {})
    if lease_meta.get("name") != lease_name or lease_meta.get("namespace") != namespace:
        raise ValueError("observed Lease does not match the locked MaaS controller Lease identity")
    holder = lease_spec.get("holderIdentity", "")
    prefix = f"{pod_name}_"
    if not holder.startswith(prefix) or not holder[len(prefix) :]:
        raise ValueError("Lease holder identity is not owned by the current controller Pod name")
    renew_time = lease_spec.get("renewTime")
    duration = int(lease_spec.get("leaseDurationSeconds", 0))
    if not renew_time or duration <= 0:
        raise ValueError("Lease has no renewal time or positive duration")
    current_time = now or datetime.now(timezone.utc)
    renewal = _parse_time(renew_time)
    age = (current_time - renewal).total_seconds()
    if age < -5 or age >= duration:
        raise ValueError("leader Lease renewal is stale or in the future")

    acquisition: tuple[datetime, str] | None = None
    expected_lock = f'lock="{namespace}/{lease_name}"'
    for line in logs.splitlines():
        match = _LOG_TIMESTAMP.match(line)
        if not match:
            continue
        timestamp = _parse_time(match.group(1))
        message = match.group(2)
        if timestamp >= start_time and "Successfully acquired lease" in message and expected_lock in message:
            acquisition = timestamp, line
    if acquisition is None:
        raise ValueError("current Ready Pod logs do not show its post-start acquisition of the expected Lease")

    return {
        "schema": "praxis.kind.maas-controller-lease/v1",
        "namespace": namespace,
        "deployment": dep_meta.get("name"),
        "deployment_generation": dep_meta.get("generation"),
        "observed_generation": dep_status.get("observedGeneration"),
        "pod_name": pod_name,
        "pod_uid": pod_uid,
        "pod_image": pod_image,
        "runtime_image_id": image_id,
        "lease_name": lease_name,
        "lease_uid": lease_meta.get("uid"),
        "lease_holder_identity": holder,
        "lease_identity_suffix_is_pod_uid": holder[len(prefix) :] == pod_uid,
        "lease_renew_time": renew_time,
        "lease_age_seconds_at_check": round(age, 3),
        "lease_duration_seconds": duration,
        "current_pod_acquire_log": acquisition[1],
        "result": "pass",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pod", required=True, type=Path)
    parser.add_argument("--deployment", required=True, type=Path)
    parser.add_argument("--lease", required=True, type=Path)
    parser.add_argument("--logs", required=True, type=Path)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--lease-name", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    result = verify(
        json.loads(args.pod.read_text()),
        json.loads(args.deployment.read_text()),
        json.loads(args.lease.read_text()),
        args.logs.read_text(),
        args.namespace,
        args.lease_name,
    )
    args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print("current Ready Pod image and runtime digest, lease RBAC, fresh Pod-name holder, and post-start lease acquisition verified")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
