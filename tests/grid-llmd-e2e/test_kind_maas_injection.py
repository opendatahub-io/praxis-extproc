# SPDX-License-Identifier: Apache-2.0
import unittest

from kind_maas_injection import ContractError, prepare_patch, restore_patch, snapshot


def namespace(labels=None, uid="namespace-uid"):
    return {
        "apiVersion": "v1",
        "kind": "Namespace",
        "metadata": {"name": "maas-system", "uid": uid, "labels": labels or {}},
    }


class KindMaaSInjectionContractTests(unittest.TestCase):
    def test_enabled_injection_is_disabled_with_uid_and_value_tests(self):
        state = snapshot(namespace({"app": "maas", "istio-injection": "enabled"}))

        self.assertTrue(state["changed"])
        self.assertEqual(
            prepare_patch(state),
            [
                {"op": "test", "path": "/metadata/uid", "value": "namespace-uid"},
                {"op": "test", "path": "/metadata/labels/istio-injection", "value": "enabled"},
                {"op": "replace", "path": "/metadata/labels/istio-injection", "value": "disabled"},
            ],
        )

    def test_restore_requires_original_namespace_and_disabled_current_value(self):
        state = snapshot(namespace({"istio-injection": "enabled"}))

        self.assertEqual(
            restore_patch(state, namespace({"istio-injection": "disabled"})),
            [
                {"op": "test", "path": "/metadata/uid", "value": "namespace-uid"},
                {"op": "test", "path": "/metadata/labels/istio-injection", "value": "disabled"},
                {"op": "replace", "path": "/metadata/labels/istio-injection", "value": "enabled"},
            ],
        )
        self.assertEqual(restore_patch(state, namespace({"istio-injection": "enabled"})), [])

    def test_absent_or_disabled_label_is_preserved_without_patch(self):
        for labels in ({}, {"istio-injection": "disabled"}):
            with self.subTest(labels=labels):
                state = snapshot(namespace(labels))
                self.assertFalse(state["changed"])
                self.assertEqual(prepare_patch(state), [])
                self.assertEqual(restore_patch(state, namespace(labels)), [])

    def test_rejects_namespace_replacement_and_unexpected_label_changes(self):
        state = snapshot(namespace({"istio-injection": "enabled"}))

        with self.assertRaisesRegex(ContractError, "identity changed"):
            restore_patch(state, namespace({"istio-injection": "disabled"}, uid="replacement"))
        with self.assertRaisesRegex(ContractError, "changed after cleanup preparation"):
            restore_patch(state, namespace({"istio-injection": "enabled"}, uid="namespace-uid").copy() | {
                "metadata": {"name": "maas-system", "uid": "namespace-uid", "labels": {"istio-injection": "enabled", "istio.io/rev": "canary"}}
            })

    def test_rejects_revision_and_ambient_injection_modes(self):
        with self.assertRaisesRegex(ContractError, "revision-based"):
            snapshot(namespace({"istio.io/rev": "stable", "istio-injection": "enabled"}))
        with self.assertRaisesRegex(ContractError, "ambient"):
            snapshot(namespace({"istio.io/dataplane-mode": "ambient", "istio-injection": "enabled"}))

    def test_rejects_unknown_injection_label_value(self):
        with self.assertRaisesRegex(ContractError, "unsupported istio-injection"):
            snapshot(namespace({"istio-injection": "automatic"}))


if __name__ == "__main__":
    unittest.main()
