#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from datetime import datetime, timezone
import unittest

from verify_kind_maas_controller_lease import verify


NOW = datetime(2026, 11, 2, 17, 36, 30, tzinfo=timezone.utc)
NAMESPACE = "maas-system"
LEASE_NAME = "maas-controller.models-as-a-service.opendatahub.io"
IMAGE = "registry.example/maas/controller@sha256:" + "a" * 64


def fixture():
    pod_name = "maas-controller-55d6c5769d-n8927"
    return {
        "metadata": {"name": pod_name, "namespace": NAMESPACE, "uid": "pod-uid-123"},
        "spec": {"containers": [{"name": "manager", "image": IMAGE}]},
        "status": {
            "phase": "Running",
            "conditions": [{"type": "Ready", "status": "True"}],
            "containerStatuses": [
                {
                    "name": "manager",
                    "imageID": IMAGE,
                    "state": {"running": {"startedAt": "2026-11-02T17:36:25Z"}},
                }
            ],
        },
    }, {
        "metadata": {"name": "maas-controller", "namespace": NAMESPACE, "generation": 4},
        "spec": {"replicas": 1, "template": {"spec": {"containers": [{"name": "manager", "image": IMAGE}]}}},
        "status": {"observedGeneration": 4, "readyReplicas": 1, "availableReplicas": 1},
    }, {
        "metadata": {"name": LEASE_NAME, "namespace": NAMESPACE, "uid": "lease-uid-456"},
        "spec": {
            "holderIdentity": pod_name + "_47fcf13d-be0c-42ce-8707-595828d83f8d",
            "renewTime": "2026-11-02T17:36:29Z",
            "leaseDurationSeconds": 15,
        },
    }, (
        '2026-11-02T17:36:26.000000Z I1102 17:36:26.000000 1 leaderelection.go:272] '
        f'"Successfully acquired lease" lock="{NAMESPACE}/{LEASE_NAME}"\n'
    )


class VerifyLeaseTest(unittest.TestCase):
    def setUp(self):
        self.pod, self.deployment, self.lease, self.logs = fixture()

    def run_verification(self):
        return verify(self.pod, self.deployment, self.lease, self.logs, NAMESPACE, LEASE_NAME, now=NOW)

    def test_accepts_process_uuid_distinct_from_pod_uid(self):
        result = self.run_verification()
        self.assertEqual("pass", result["result"])
        self.assertFalse(result["lease_identity_suffix_is_pod_uid"])

    def test_rejects_holder_for_another_pod(self):
        self.lease["spec"]["holderIdentity"] = "different-controller_47fcf13d-be0c-42ce-8707-595828d83f8d"
        with self.assertRaisesRegex(ValueError, "current controller Pod name"):
            self.run_verification()

    def test_rejects_stale_lease(self):
        self.lease["spec"]["renewTime"] = "2026-11-02T17:36:00Z"
        with self.assertRaisesRegex(ValueError, "stale or in the future"):
            self.run_verification()

    def test_requires_current_pod_acquisition_log(self):
        self.logs = "2026-11-02T17:36:20Z Successfully acquired lease lock=\"maas-system/other\"\n"
        with self.assertRaisesRegex(ValueError, "post-start acquisition"):
            self.run_verification()

    def test_rejects_runtime_image_digest_mismatch(self):
        self.pod["status"]["containerStatuses"][0]["imageID"] = "registry.example/maas/controller@sha256:" + "b" * 64
        with self.assertRaisesRegex(ValueError, "runtime imageID"):
            self.run_verification()


if __name__ == "__main__":
    unittest.main()
