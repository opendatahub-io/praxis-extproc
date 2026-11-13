#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Contract tests for the Kind MaaS controller render."""

from __future__ import annotations

import importlib.util
import sys
import unittest
from pathlib import Path

import yaml


SCRIPT = Path(__file__).with_name("render-kind-maas-controller.py")
SPEC = importlib.util.spec_from_file_location("render_kind_maas_controller", SCRIPT)
assert SPEC and SPEC.loader
module = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = module
SPEC.loader.exec_module(module)


def manifest(subject_namespace: str = "opendatahub", binding_namespace: str = "maas-system") -> str:
    return yaml.safe_dump_all(
        [
            {
                "apiVersion": "v1",
                "kind": "ServiceAccount",
                "metadata": {"name": "maas-controller", "namespace": "maas-system"},
            },
            {
                "apiVersion": "rbac.authorization.k8s.io/v1",
                "kind": "RoleBinding",
                "metadata": {
                    "name": "maas-controller-leader-election-rolebinding",
                    "namespace": binding_namespace,
                },
                "roleRef": {
                    "apiGroup": "rbac.authorization.k8s.io",
                    "kind": "Role",
                    "name": "maas-controller-leader-election-role",
                },
                "subjects": [
                    {"kind": "ServiceAccount", "name": "maas-controller", "namespace": subject_namespace}
                ],
            },
            {
                "apiVersion": "apps/v1",
                "kind": "Deployment",
                "metadata": {"name": "maas-controller", "namespace": "maas-system"},
                "spec": {
                    "template": {
                        "spec": {
                            "containers": [
                                {"name": "manager", "args": ["--leader-elect", "--monitoring-namespace=opendatahub"]}
                            ]
                        }
                    }
                },
            },
        ],
        sort_keys=False,
    )


def manifest_with_binding_update(**updates: object) -> str:
    documents = list(yaml.safe_load_all(manifest()))
    binding = next(doc for doc in documents if doc.get("kind") == "RoleBinding")
    binding.update(updates)
    return yaml.safe_dump_all(documents, sort_keys=False)


class KindMaaSControllerRenderTests(unittest.TestCase):
    def test_corrects_only_the_subject_namespace_and_keeps_other_documents(self) -> None:
        rendered, changed = module.render(manifest(), "maas-system")
        docs = list(yaml.safe_load_all(rendered))
        binding = next(doc for doc in docs if doc.get("kind") == "RoleBinding")
        self.assertTrue(changed)
        self.assertEqual(binding["metadata"]["namespace"], "maas-system")
        self.assertEqual(binding["subjects"][0]["namespace"], "maas-system")
        self.assertEqual(sum(doc.get("kind") == "ServiceAccount" for doc in docs), 1)

    def test_is_idempotent_when_namespace_transform_already_matches(self) -> None:
        rendered, changed = module.render(manifest("maas-system"), "maas-system")
        rerendered, changed_again = module.render(rendered, "maas-system")
        self.assertFalse(changed)
        self.assertFalse(changed_again)
        self.assertEqual(rerendered, rendered)

    def test_rejects_binding_left_in_another_namespace(self) -> None:
        with self.assertRaisesRegex(ValueError, "metadata namespace"):
            module.render(manifest(binding_namespace="opendatahub"), "maas-system")

    def test_rejects_missing_or_duplicate_binding(self) -> None:
        with self.assertRaisesRegex(ValueError, "found 0"):
            module.render("kind: ServiceAccount\nmetadata:\n  name: maas-controller\n", "maas-system")
        with self.assertRaisesRegex(ValueError, "found 2"):
            module.render(manifest() + "---\n" + manifest(), "maas-system")

    def test_rejects_unexpected_subject_or_role_reference(self) -> None:
        documents = list(yaml.safe_load_all(manifest()))
        binding = next(doc for doc in documents if doc.get("kind") == "RoleBinding")
        binding["subjects"][0]["name"] = "other"
        wrong_subject = yaml.safe_dump_all(documents, sort_keys=False)
        with self.assertRaisesRegex(ValueError, "subject must be maas-controller"):
            module.render(wrong_subject, "maas-system")
        wrong_role = manifest_with_binding_update(
            roleRef={"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": "another-role"}
        )
        with self.assertRaisesRegex(ValueError, "expected namespaced Role"):
            module.render(wrong_role, "maas-system")

    def test_empty_monitoring_namespace_skips_optional_observability_for_kind(self) -> None:
        rendered, changed = module.configure_monitoring_namespace(manifest(), "")
        deployment = next(doc for doc in yaml.safe_load_all(rendered) if doc.get("kind") == "Deployment")
        args = deployment["spec"]["template"]["spec"]["containers"][0]["args"]
        self.assertTrue(changed)
        self.assertIn("--monitoring-namespace=", args)
        self.assertNotIn("--monitoring-namespace=opendatahub", args)

    def test_monitoring_namespace_is_idempotent_and_can_be_explicit(self) -> None:
        first, changed = module.configure_monitoring_namespace(manifest(), "observability")
        second, changed_again = module.configure_monitoring_namespace(first, "observability")
        deployment = next(doc for doc in yaml.safe_load_all(second) if doc.get("kind") == "Deployment")
        args = deployment["spec"]["template"]["spec"]["containers"][0]["args"]
        self.assertTrue(changed)
        self.assertFalse(changed_again)
        self.assertIn("--monitoring-namespace=observability", args)

    def test_rejects_missing_or_duplicate_monitoring_namespace_argument(self) -> None:
        documents = list(yaml.safe_load_all(manifest()))
        deployment = next(doc for doc in documents if doc.get("kind") == "Deployment")
        args = deployment["spec"]["template"]["spec"]["containers"][0]["args"]
        args[1] = "--different-flag"
        source = yaml.safe_dump_all(documents, sort_keys=False)
        with self.assertRaisesRegex(ValueError, "exactly one --monitoring-namespace"):
            module.configure_monitoring_namespace(source, "")
        args[1:] = ["--monitoring-namespace=opendatahub", "--monitoring-namespace=other"]
        source = yaml.safe_dump_all(documents, sort_keys=False)
        with self.assertRaisesRegex(ValueError, "exactly one --monitoring-namespace"):
            module.configure_monitoring_namespace(source, "")


if __name__ == "__main__":
    unittest.main()
