#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Exercise early-failure cleanup without contacting a real cluster or Docker daemon."""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).parents[2]
LIFECYCLE = REPO_ROOT / "tests/grid-llmd-e2e/run-kind-lifecycle.sh"


def prepare_cleanup_case(root: Path, *, task_namespace_created: bool = False):
    run_root = root / "run"
    state = run_root / "private-state"
    evidence = run_root / "evidence"
    bootstrap = evidence / "cluster/bootstrap"
    bin_dir = root / "bin"
    for directory in (
        state / "kind",
        state / "charts",
        bootstrap,
        evidence / "logs",
        evidence / "phase-metrics",
        evidence / "source-resolution",
        bin_dir,
    ):
        directory.mkdir(parents=True, exist_ok=True)

    (evidence / "source-resolution/source-locks.json").write_text(
        '{"components":{}}\n', encoding="utf-8"
    )
    names = {
        "run-id.txt": "kind-cleanup-test",
        "cluster-name.txt": "grid-llmd-cleanup-test",
        "registry-name.txt": "grid-llmd-cleanup-test-registry",
        "registry-network-name.txt": "grid-llmd-cleanup-test-network",
    }
    for filename, value in names.items():
        (bootstrap / filename).write_text(value + "\n", encoding="utf-8")
    if task_namespace_created:
        task_result = evidence / "platform/task-namespace/RESULT.json"
        task_result.parent.mkdir(parents=True)
        task_result.write_text('{"status":"created"}\n', encoding="utf-8")

    kubeconfig = state / "kind/kubeconfig"
    kubeconfig.write_text("synthetic test kubeconfig\n", encoding="utf-8")
    lifecycle_values = {
        "E2E_RUN_ID": "kind-cleanup-test",
        "KIND_RUN_ROOT": str(run_root),
        "KIND_RUN_DIR": str(evidence),
        "KIND_PRIVATE_STATE_DIR": str(state),
        "KIND_CLUSTER_NAME": "grid-llmd-cleanup-test",
        "KIND_REGISTRY_CONTAINER": "grid-llmd-cleanup-test-registry",
        "KIND_REGISTRY_NETWORK": "grid-llmd-cleanup-test-network",
        "GRID_RESOURCE_NAMESPACE": "grid-llmd-cleanup-test",
        "GRID_OPERATOR_NAMESPACE": "praxis-grid-cleanup-test",
        "GRID_NETWORK_NAME": "grid-llmd-cleanup-test-network",
        "GRID_SITE_NAME": "grid-llmd-cleanup-test-site",
        "GRID_SITE_SERVER_NAME": "grid-llmd-cleanup-test-site.grid.internal",
        "GRID_TASK_GATEWAY_NAME": "grid-llmd-ingress",
        "GRID_GATEWAY_HTTPROUTE_NAME": "grid-llmd-inference",
        "GRID_GATEWAY_RELEASE_NAME": "grid-gateway",
        "GRID_OPERATOR_RELEASE_NAME": "grid-operator",
        "GRID_PROVIDER_PREFIX": "grid-cleanup-test",
        "POOL_A_GATEWAY_NAME": "grid-llmd-pool-a",
        "POOL_B_GATEWAY_NAME": "grid-llmd-pool-b",
        "KIND_CAPACITY_PROFILE": "gha-standard-4cpu",
    }
    journal = "\n".join(
        f"export {key}={shlex.quote(value)}"
        for key, value in lifecycle_values.items()
    )
    (state / "lifecycle.env").write_text(journal + "\n", encoding="utf-8")

    stubs = {
        "kind": "#!/usr/bin/env bash\n[[ \"$1 $2\" == 'get clusters' ]]\n",
        "docker": (
            "#!/usr/bin/env bash\n"
            "case \"$1 $2\" in\n"
            "  'container inspect'|'network inspect') exit 1 ;;\n"
            "  'network ls') exit 0 ;;\n"
            "  *) exit 2 ;;\n"
            "esac\n"
        ),
        "kubectl": "#!/usr/bin/env bash\nprintf 'https://127.0.0.1:6443'\n",
    }
    for name, content in stubs.items():
        stub = bin_dir / name
        stub.write_text(content, encoding="utf-8")
        stub.chmod(0o700)

    environment = os.environ.copy()
    environment["PATH"] = str(bin_dir) + os.pathsep + environment["PATH"]
    environment["KIND_PRIVATE_STATE_DIR"] = str(state)
    return environment, evidence, kubeconfig


class CleanupRetryTests(unittest.TestCase):
    def test_early_image_failure_can_destroy_run_without_optional_charts(self) -> None:
        with tempfile.TemporaryDirectory(prefix="praxis-kind-cleanup-retry-") as temp:
            root = Path(temp)
            environment, evidence, kubeconfig = prepare_cleanup_case(root)
            result = subprocess.run(
                ["bash", str(LIFECYCLE), "--mode", "cleanup"],
                env=environment,
                capture_output=True,
                check=False,
                text=True,
            )

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            cleanup_result = json.loads(
                (evidence / "cleanup/RESULT.json").read_text(encoding="utf-8")
            )
            destroy_result = json.loads(
                (evidence / "cleanup/destroy-kind/RESULT.json").read_text(encoding="utf-8")
            )
            self.assertEqual(cleanup_result["status"], "PASS")
            self.assertTrue(destroy_result["cluster_removed"])
            self.assertTrue(destroy_result["registry_removed"])
            self.assertTrue(destroy_result["network_removed"])
            self.assertFalse((evidence / "cleanup/RETRY-FAILURE.json").exists())
            self.assertFalse(kubeconfig.exists())

    def test_pre_grid_failure_skips_cleanup_that_requires_grid_crds(self) -> None:
        with tempfile.TemporaryDirectory(prefix="praxis-kind-pre-grid-cleanup-") as temp:
            environment, evidence, kubeconfig = prepare_cleanup_case(
                Path(temp), task_namespace_created=True
            )
            result = subprocess.run(
                ["bash", str(LIFECYCLE), "--mode", "cleanup"],
                env=environment,
                capture_output=True,
                check=False,
                text=True,
            )

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            cleanup_result = json.loads(
                (evidence / "cleanup/RESULT.json").read_text(encoding="utf-8")
            )
            task_result_path = next(
                (evidence / "cleanup").glob("task-namespace-pre-grid-*/RESULT.json")
            )
            task_result = json.loads(task_result_path.read_text(encoding="utf-8"))
            self.assertEqual(cleanup_result["status"], "PASS")
            self.assertFalse(cleanup_result["naturalCleanup"])
            self.assertEqual(task_result["status"], "not-applicable")
            self.assertFalse(task_result["naturalGridWithdrawalVerified"])
            self.assertFalse((evidence / "cleanup/RETRY-FAILURE.json").exists())
            self.assertFalse(kubeconfig.exists())


if __name__ == "__main__":
    unittest.main()
