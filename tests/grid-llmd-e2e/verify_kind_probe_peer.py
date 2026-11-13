#!/usr/bin/env python3
"""Validate staged pool setup-probe peer readiness and simulator independence."""

from __future__ import annotations

import json
import sys
from typing import Any


def validate_peer(payload: dict[str, Any]) -> dict[str, Any]:
    pool = payload.get("pool")
    peer = payload.get("peerPool")
    if (pool, peer) not in (("pool-a", "pool-b"), ("pool-b", "pool-a")):
        raise ValueError("pool and peerPool must identify the two distinct task pools")

    peer_resource = payload.get("peerResource")
    peer_deployment_present = payload.get("peerDeploymentPresent") is True
    target_uids = payload.get("targetPodUIDs")
    peer_uids = payload.get("peerPodUIDs")
    if not isinstance(target_uids, list) or not target_uids or any(not isinstance(uid, str) or not uid for uid in target_uids):
        raise ValueError("selected pool must have non-empty simulator Pod UIDs")

    if peer_resource is None:
        if pool != "pool-a":
            raise ValueError("Pool B setup requires the previously qualified Pool A graph")
        if peer_deployment_present or peer_uids:
            raise ValueError("peer simulator Deployment exists without its LLMInferenceService")
        return {
            "pool": pool,
            "peerPool": peer,
            "peerState": "not-created",
            "pairwiseDisjointVerified": False,
            "pairwiseDisjointDeferred": True,
            "targetPodUIDs": sorted(set(target_uids)),
            "peerPodUIDs": [],
            "reason": "Pool A setup probe is staged before Pool B creation",
        }

    if not isinstance(peer_resource, dict):
        raise ValueError("peerResource must be an LLMInferenceService object or null")
    metadata = peer_resource.get("metadata", {})
    status = peer_resource.get("status", {})
    if metadata.get("labels", {}).get("praxis.fast/qualification-run") != payload.get("runLabel"):
        raise ValueError("peer LLMInferenceService is not owned by this run")
    generation = metadata.get("generation")
    if not isinstance(generation, int) or status.get("observedGeneration") != generation:
        raise ValueError("peer LLMInferenceService is not observed at its current generation")
    if not any(
        condition.get("type") == "Ready" and condition.get("status") == "True"
        for condition in status.get("conditions", [])
    ):
        raise ValueError("peer LLMInferenceService is not Ready")
    if not peer_deployment_present:
        raise ValueError("ready peer LLMInferenceService has no verified simulator Deployment")
    if not isinstance(peer_uids, list) or not peer_uids or any(not isinstance(uid, str) or not uid for uid in peer_uids):
        raise ValueError("ready peer pool must have non-empty simulator Pod UIDs")
    if set(target_uids).intersection(peer_uids):
        raise ValueError("simulator Pod UID sets overlap across pools")

    return {
        "pool": pool,
        "peerPool": peer,
        "peerState": "ready-and-run-owned",
        "pairwiseDisjointVerified": True,
        "pairwiseDisjointDeferred": False,
        "targetPodUIDs": sorted(set(target_uids)),
        "peerPodUIDs": sorted(set(peer_uids)),
    }


def main() -> int:
    try:
        payload = json.load(sys.stdin)
        result = validate_peer(payload)
    except (json.JSONDecodeError, ValueError, TypeError) as error:
        print(f"invalid pool-probe peer state: {error}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
