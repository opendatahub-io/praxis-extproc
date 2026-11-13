#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("verify-kind-maas-api.sh")
IMAGE = "registry.local/maas-api@sha256:" + "a" * 64
RUN_ID = "kind-readiness-test"


class MaaSAPIReadinessWaitTests(unittest.TestCase):
    def test_waits_for_default_config_service_and_route_without_mutating(self):
        with tempfile.TemporaryDirectory(prefix="maas-api-readiness-test-") as temp:
            root = Path(temp)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            state = root / "state"
            state.mkdir()
            kubeconfig = root / "kubeconfig"
            kubeconfig.write_text("test-only kubeconfig\n", encoding="utf-8")
            kubeconfig.chmod(0o600)
            kubectl = bin_dir / "kubectl"
            kubectl.write_text(
                """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
index = next(i for i, value in enumerate(args) if value in {"config", "get", "logs"})
args = args[index:]
operation, *rest = args
state = Path(os.environ["MOCK_STATE"])
image = os.environ["MOCK_IMAGE"]
uid = "config-uid"

def emit(value):
    print(json.dumps(value))

def counter(name):
    path = state / name
    value = int(path.read_text() if path.exists() else "0") + 1
    path.write_text(str(value))
    return value

if operation == "config":
    emit({"clusters": [{"cluster": {"server": "https://kind.example:6443"}}]})
elif operation == "logs":
    print("mock sanitized controller log")
elif rest and rest[0] == "--raw=/readyz":
    print("ok")
elif rest[:2] == ["namespace", "maas-system"]:
    emit({"metadata": {"labels": {"praxis.kind/run": "kind-readiness-test"}}})
elif rest[:2] == ["config", "default"]:
    ready = os.environ.get("MOCK_CONFIG_READY", "delayed") != "never" and counter("config") >= 3
    emit({
        "metadata": {"uid": uid, "generation": 1},
        "status": {"conditions": [{"type": "Ready", "status": "True" if ready else "False",
                                    "observedGeneration": 1, "reason": "Ready" if ready else "Reconciling"}]},
    })
elif rest[:2] == ["service", "maas-api"]:
    if counter("service") < 2:
        print("service not yet created", file=sys.stderr)
        sys.exit(1)
    emit({"metadata": {"ownerReferences": [{"kind": "Config", "name": "default", "uid": uid}]}})
elif rest[:2] == ["aitenant", "models-as-a-service"]:
    emit({"metadata": {"name": "models-as-a-service", "namespace": "ai-tenants", "uid": "tenant-uid", "generation": 1},
          "status": {"conditions": [{"type": "Ready", "status": "True"}]}})
elif rest[:2] == ["maastenantconfig", "default-tenant"]:
    emit({"metadata": {"name": "default-tenant", "namespace": "models-as-a-service", "uid": "tenant-config-uid", "generation": 1},
          "status": {"conditions": [{"type": "Ready", "status": "True"}]}})
elif rest[:2] == ["deployment", "maas-api"]:
    emit({"metadata": {"uid": "deployment-uid", "generation": 1,
                        "ownerReferences": [{"kind": "Config", "name": "default", "uid": uid}]},
          "spec": {"replicas": 1, "template": {"spec": {"containers": [{"name": "maas-api", "image": image}]} }},
          "status": {"observedGeneration": 1, "readyReplicas": 1, "availableReplicas": 1}})
elif rest and rest[0] == "pods":
    emit({"items": [{"status": {"containerStatuses": [{"name": "maas-api", "ready": True,
                                                           "imageID": "containerd://image@" + image.rsplit("@", 1)[1]}]}}]})
elif rest and rest[0] == "endpointslices":
    emit({"items": [{"ports": [{"port": 8443}], "endpoints": [{"conditions": {"ready": True}}]}]})
elif rest[:2] == ["httproute", "maas-api-route"]:
    if counter("route") < 2:
        print("route not yet accepted", file=sys.stderr)
        sys.exit(1)
    emit({"metadata": {"ownerReferences": [{"kind": "Config", "name": "default", "uid": uid}]},
          "spec": {"parentRefs": [{"name": "maas-default-gateway"}]},
          "status": {"parents": [{"conditions": [{"type": "Accepted", "status": "True"},
                                                   {"type": "ResolvedRefs", "status": "True"}]}]}})
elif rest[:1] == ["events"]:
    emit({"items": []})
else:
    print("unexpected read-only kubectl request: " + " ".join(rest), file=sys.stderr)
    sys.exit(2)
""",
                encoding="utf-8",
            )
            kubectl.chmod(0o755)

            env = os.environ.copy()
            env["PATH"] = f"{bin_dir}:{env['PATH']}"
            env["MOCK_STATE"] = str(state)
            env["MOCK_IMAGE"] = IMAGE
            evidence = root / "evidence"
            result = subprocess.run(
                [
                    "bash",
                    str(SCRIPT),
                    "--context",
                    "kind-test",
                    "--kubeconfig",
                    str(kubeconfig),
                    "--expected-api-server",
                    "https://kind.example:6443",
                    "--run-id",
                    RUN_ID,
                    "--image",
                    IMAGE,
                    "--evidence-dir",
                    str(evidence),
                    "--timeout-seconds",
                    "30",
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=40,
                check=False,
            )

            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("MaaS controller-owned API and route passed", result.stdout)
            config_polls = (evidence / "config-readiness-polls.tsv").read_text(encoding="utf-8").splitlines()
            service_polls = (evidence / "service-readiness-polls.tsv").read_text(encoding="utf-8").splitlines()
            route_polls = (evidence / "route-readiness-polls.tsv").read_text(encoding="utf-8").splitlines()
            self.assertGreaterEqual(len(config_polls), 2)
            self.assertIn("ready=false", config_polls[0])
            self.assertIn("ready=true", config_polls[-1])
            self.assertEqual(len(service_polls), 1)
            self.assertEqual(len(route_polls), 1)
            full_result = json.loads((evidence / "RESULT.json").read_text())
            self.assertEqual(full_result["status"], "PASS")
            self.assertEqual(full_result["readinessScope"], "platform-ready")
            self.assertTrue(full_result["configReady"])

            # Bootstrap may finish API transport setup after the controller
            # observes Config/default even while a required AIGC EnvoyFilter
            # keeps the tenant aggregate NotReady.
            for path in state.iterdir():
                path.unlink()
            env["MOCK_CONFIG_READY"] = "never"
            bootstrap_evidence = root / "bootstrap-evidence"
            bootstrap_result = subprocess.run(
                [
                    "bash", str(SCRIPT),
                    "--context", "kind-test",
                    "--kubeconfig", str(kubeconfig),
                    "--expected-api-server", "https://kind.example:6443",
                    "--run-id", RUN_ID,
                    "--image", IMAGE,
                    "--mode", "api-bootstrap",
                    "--evidence-dir", str(bootstrap_evidence),
                    "--timeout-seconds", "10",
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=20,
                check=False,
            )
            self.assertEqual(bootstrap_result.returncode, 0, bootstrap_result.stdout + bootstrap_result.stderr)
            self.assertIn("scope=api-bootstrap", bootstrap_result.stdout)
            bootstrap_result_json = json.loads((bootstrap_evidence / "RESULT.json").read_text())
            self.assertEqual(bootstrap_result_json["readinessScope"], "api-bootstrap")
            self.assertFalse(bootstrap_result_json["configReady"])
            bootstrap_polls = (bootstrap_evidence / "config-readiness-polls.tsv").read_text().splitlines()
            self.assertEqual(len(bootstrap_polls), 1)
            self.assertIn("observed=true", bootstrap_polls[0])
            self.assertIn("ready=false", bootstrap_polls[0])

            # Full platform readiness must still reject that same NotReady Config.
            for path in state.iterdir():
                path.unlink()
            blocked_evidence = root / "blocked-platform-evidence"
            blocked_result = subprocess.run(
                [
                    "bash", str(SCRIPT),
                    "--context", "kind-test",
                    "--kubeconfig", str(kubeconfig),
                    "--expected-api-server", "https://kind.example:6443",
                    "--run-id", RUN_ID,
                    "--image", IMAGE,
                    "--mode", "platform-ready",
                    "--evidence-dir", str(blocked_evidence),
                    "--timeout-seconds", "1",
                ],
                env=env,
                capture_output=True,
                text=True,
                timeout=10,
                check=False,
            )
            self.assertEqual(blocked_result.returncode, 2, blocked_result.stdout + blocked_result.stderr)
            self.assertIn("did not reach current-generation Ready", blocked_result.stderr)
            self.assertEqual(json.loads((blocked_evidence / "RESULT.json").read_text())["status"], "FAILED")
            self.assertTrue((blocked_evidence / "maas-controller-logs-failure.txt").is_file())


if __name__ == "__main__":
    unittest.main()
