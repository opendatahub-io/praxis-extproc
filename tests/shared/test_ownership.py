#!/usr/bin/env python3
"""Ownership and cleanup safety regressions using mocked API responses."""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path
from unittest.mock import patch


MODULE_PATH = Path(__file__).with_name("ownership.py")
SPEC = importlib.util.spec_from_file_location("ownership", MODULE_PATH)
assert SPEC and SPEC.loader
ownership = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ownership)


class OwnershipTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="ownership-test-")
        self.root = Path(self.temp.name)
        self.inventory = self.root / "inventory.json"
        self.resource = {
            "kind": "ExternalProvider",
            "namespace": "task-tenant",
            "name": "provider-a",
            "uid": "uid-123",
        }
        self.inventory.write_text(
            json.dumps({"run_id": "run-1", "scenario": "grid-maas", "resources": [self.resource]}),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    @staticmethod
    def completed(stdout: str = "", returncode: int = 0, stderr: str = "") -> subprocess.CompletedProcess:
        return subprocess.CompletedProcess(["oc"], returncode, stdout, stderr)

    def test_run_labels_are_complete_and_scenario_scoped(self) -> None:
        labels = ownership.labels_for("run-1", "grid-maas")
        self.assertEqual(labels["praxis.fast/qualification-run"], "run-1")
        self.assertEqual(labels["praxis.fast/qualification-scenario"], "grid-maas")
        self.assertEqual(labels["app.kubernetes.io/managed-by"], "praxis-extproc-e2e")

    def test_shared_platform_resources_are_not_in_cleanup_allowlist(self) -> None:
        for kind in ("AITenant", "MaaSTenantConfig", "DataScienceCluster", "GatewayClass", "Namespace/openshift-ingress"):
            self.assertNotIn(kind, ownership.ALLOWED_KINDS)

    def test_plan_only_does_not_call_the_cluster(self) -> None:
        args = Namespace(
            inventory=self.inventory,
            run_id="run-1",
            scenario="grid-maas",
            apply=False,
            expected_api=None,
            context=None,
        )
        with patch.object(ownership, "run_oc") as run_oc:
            result = ownership.cleanup(args)
        run_oc.assert_not_called()
        self.assertEqual(result["results"][0]["status"], "PLAN_ONLY")

    def test_cleanup_stops_when_live_uid_or_ownership_is_ambiguous(self) -> None:
        labels = ownership.labels_for("run-1", "grid-maas")
        live = {"metadata": {"uid": "different-uid", "labels": labels}}
        args = Namespace(
            inventory=self.inventory,
            run_id="run-1",
            scenario="grid-maas",
            apply=True,
            expected_api="https://api.example.invalid:6443",
            context="task-context",
        )
        with patch.object(
            ownership,
            "run_oc",
            side_effect=[self.completed("https://api.example.invalid:6443\n"), self.completed("ok\n"), self.completed(json.dumps(live))],
        ) as run_oc:
            with self.assertRaisesRegex(ownership.OwnershipError, "UID changed"):
                ownership.cleanup(args)
        self.assertEqual(run_oc.call_count, 3)

    def test_cleanup_requests_delete_only_after_matching_uid_and_all_labels(self) -> None:
        labels = ownership.labels_for("run-1", "grid-maas")
        live = {"metadata": {"uid": "uid-123", "labels": labels}}
        args = Namespace(
            inventory=self.inventory,
            run_id="run-1",
            scenario="grid-maas",
            apply=True,
            expected_api="https://api.example.invalid:6443",
            context="task-context",
        )
        with patch.object(
            ownership,
            "run_oc",
            side_effect=[
                self.completed("https://api.example.invalid:6443\n"),
                self.completed("ok\n"),
                self.completed(json.dumps(live)),
                self.completed(),
                self.completed(returncode=1, stderr="Error from server (NotFound): not found"),
            ],
        ) as run_oc:
            result = ownership.cleanup(args)
        self.assertEqual(result["results"][0]["status"], "DELETED_VERIFIED")
        delete_call = run_oc.call_args_list[-2].args[0]
        self.assertEqual(delete_call[0:3], ["delete", "ExternalProvider", "provider-a"])


if __name__ == "__main__":
    unittest.main()
