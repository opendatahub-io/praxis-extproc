#!/usr/bin/env python3
"""Regression tests for staged Kind pool setup-probe peer verification."""

from __future__ import annotations

import unittest

from verify_kind_probe_peer import validate_peer


def llmisvc(pool: str = "pool-b", run: str = "run-r9", generation: int = 2) -> dict:
    return {
        "metadata": {
            "generation": generation,
            "labels": {"praxis.fast/qualification-run": run},
        },
        "status": {
            "observedGeneration": generation,
            "conditions": [{"type": "Ready", "status": "True"}],
        },
    }


def payload(**changes: object) -> dict:
    result: dict[str, object] = {
        "pool": "pool-a",
        "peerPool": "pool-b",
        "runLabel": "run-r9",
        "peerResource": None,
        "peerDeploymentPresent": False,
        "targetPodUIDs": ["sim-a"],
        "peerPodUIDs": [],
    }
    result.update(changes)
    return result


class VerifyKindProbePeerTests(unittest.TestCase):
    def test_first_pool_probe_allows_peer_not_created_and_defers_disjoint_check(self) -> None:
        result = validate_peer(payload())
        self.assertEqual(result["peerState"], "not-created")
        self.assertFalse(result["pairwiseDisjointVerified"])
        self.assertTrue(result["pairwiseDisjointDeferred"])

    def test_pool_b_probe_requires_pool_a(self) -> None:
        with self.assertRaisesRegex(ValueError, "previously qualified Pool A"):
            validate_peer(payload(pool="pool-b", peerPool="pool-a"))

    def test_first_pool_probe_rejects_orphan_peer_deployment(self) -> None:
        with self.assertRaisesRegex(ValueError, "without its LLMInferenceService"):
            validate_peer(payload(peerDeploymentPresent=True))

    def test_existing_peer_must_be_current_ready_and_run_owned(self) -> None:
        result = validate_peer(
            payload(
                peerResource=llmisvc(),
                peerDeploymentPresent=True,
                peerPodUIDs=["sim-b"],
            )
        )
        self.assertTrue(result["pairwiseDisjointVerified"])
        self.assertEqual(result["peerState"], "ready-and-run-owned")

        unowned = llmisvc()
        unowned["metadata"]["labels"]["praxis.fast/qualification-run"] = "other-run"
        with self.assertRaisesRegex(ValueError, "not owned"):
            validate_peer(payload(peerResource=unowned, peerDeploymentPresent=True, peerPodUIDs=["sim-b"]))

        stale = llmisvc(generation=1)
        stale["status"]["observedGeneration"] = 0
        with self.assertRaisesRegex(ValueError, "current generation"):
            validate_peer(payload(peerResource=stale, peerDeploymentPresent=True, peerPodUIDs=["sim-b"]))

    def test_existing_peer_must_have_disjoint_simulator_uid(self) -> None:
        with self.assertRaisesRegex(ValueError, "sets overlap"):
            validate_peer(
                payload(
                    peerResource=llmisvc(),
                    peerDeploymentPresent=True,
                    peerPodUIDs=["sim-a"],
                )
            )


if __name__ == "__main__":
    unittest.main()
