#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import copy
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from verify_kind_kserve_webhook_readiness import verify  # noqa: E402


REPOSITORY = "registry.local/kserve/controller"
TAG = "qualification-b3327ae"
DIGEST = "sha256:" + "a" * 64
LABELS = {"app": "llmisvc-controller", "instance": "kserve"}
POD_UID = "pod-uid-1"
POD_IP = "10.244.0.23"


def snapshots():
    deployment = {
        "metadata": {"name": "llmisvc-controller-manager", "generation": 3},
        "spec": {"replicas": 1, "selector": {"matchLabels": LABELS}},
        "status": {"observedGeneration": 3, "readyReplicas": 1, "availableReplicas": 1},
    }
    pods = {
        "items": [
            {
                "metadata": {"name": "llmisvc-controller-1", "uid": POD_UID, "labels": LABELS},
                "spec": {"containers": [{"name": "manager", "image": f"{REPOSITORY}:{TAG}"}]},
                "status": {
                    "phase": "Running",
                    "podIP": POD_IP,
                    "conditions": [{"type": "Ready", "status": "True"}],
                    "containerStatuses": [
                        {"name": "manager", "ready": True, "imageID": f"containerd://image@{DIGEST}"}
                    ],
                },
            }
        ]
    }
    service = {
        "metadata": {"name": "llmisvc-webhook-server-service", "namespace": "kserve"},
        "spec": {
            "clusterIP": "10.96.1.4",
            "selector": LABELS,
            "ports": [{"port": 443, "targetPort": "webhook-server"}],
        },
    }
    endpoint_slices = {
        "items": [
            {
                "metadata": {"namespace": "kserve", "labels": {"kubernetes.io/service-name": "llmisvc-webhook-server-service"}},
                "ports": [{"name": "https", "port": 9443}],
                "endpoints": [
                    {
                        "addresses": [POD_IP],
                        "conditions": {"ready": True, "serving": True, "terminating": False},
                        "targetRef": {"uid": POD_UID},
                    }
                ],
            }
        ]
    }
    return deployment, pods, service, endpoint_slices


def validate(snapshots_to_check):
    return verify(*snapshots_to_check, REPOSITORY, TAG, DIGEST)


class KServeWebhookReadinessTests(unittest.TestCase):
    def test_accepts_single_current_pod_and_matching_ready_endpoint(self):
        result = validate(snapshots())
        self.assertEqual(result["status"], "PASS")
        self.assertEqual(result["webhookService"]["endpointCount"], 1)

    def test_rejects_terminating_old_pod_during_rollout_overlap(self):
        deployment, pods, service, endpoint_slices = snapshots()
        old_pod = copy.deepcopy(pods["items"][0])
        old_pod["metadata"].update(name="llmisvc-controller-old", uid="old-uid", deletionTimestamp="2026-11-02T22:20:19Z")
        pods["items"].append(old_pod)
        with self.assertRaisesRegex(ValueError, "no rollout overlap"):
            validate((deployment, pods, service, endpoint_slices))

    def test_rejects_endpoint_not_pointing_at_current_pod(self):
        values = snapshots()
        values[3]["items"][0]["endpoints"][0]["targetRef"]["uid"] = "old-uid"
        with self.assertRaisesRegex(ValueError, "target UID"):
            validate(values)

    def test_rejects_unready_or_terminating_endpoint(self):
        values = snapshots()
        values[3]["items"][0]["endpoints"][0]["conditions"].update(ready=False, terminating=True)
        with self.assertRaisesRegex(ValueError, "not Ready and Serving"):
            validate(values)

    def test_rejects_stale_deployment_generation(self):
        values = snapshots()
        values[0]["status"]["observedGeneration"] = 2
        with self.assertRaisesRegex(ValueError, "not observed"):
            validate(values)

    def test_rejects_runtime_digest_mismatch(self):
        values = snapshots()
        values[1]["items"][0]["status"]["containerStatuses"][0]["imageID"] = "containerd://image@sha256:" + "b" * 64
        with self.assertRaisesRegex(ValueError, "runtime imageID"):
            validate(values)


if __name__ == "__main__":
    unittest.main()
