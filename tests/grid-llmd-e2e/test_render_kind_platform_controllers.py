#!/usr/bin/env python3
"""Focused ownership and image-rendering tests for the Kind platform controllers."""

import unittest

from render_kind_aigc_controller import render as render_aigc
from render_kind_maas_controller_deployment import render as render_maas, rewrite_namespaces
import yaml


class KindPlatformControllerRenderTests(unittest.TestCase):
    def test_namespace_rewriter_handles_structured_values(self) -> None:
        self.assertEqual(
            rewrite_namespaces({
                "spec": {"namespace": {"value": "kept"}},
                "apiGroups": ["maas.opendatahub.io"],
                "commonName": "webhook.redhat-ods-applications.svc",
            }),
            {
                "spec": {"namespace": {"value": "kept"}},
                "apiGroups": ["maas.opendatahub.io"],
                "commonName": "webhook.maas-system.svc",
            },
        )

    def test_maas_render_uses_locked_images_kind_flags_and_run_ownership(self) -> None:
        source = """
apiVersion: v1
kind: ServiceAccount
metadata:
  name: maas-controller
  namespace: redhat-ods-applications
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: maas-controller-leader-election-rolebinding
  namespace: redhat-ods-applications
subjects:
- kind: ServiceAccount
  name: maas-controller
  namespace: redhat-ods-applications
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: maas-controller-leader-election-role
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: maas-controller
  namespace: redhat-ods-applications
  labels:
    app.kubernetes.io/managed-by: maas-source
spec:
  template:
    spec:
      containers:
      - name: manager
        image: quay.io/example/maas-controller:latest
        args: [--leader-elect, --metrics-secure=true]
        env:
        - name: GATEWAY_NAMESPACE
          value: redhat-ods-applications
        - name: RELATED_IMAGE_ODH_MAAS_API_IMAGE
          value: latest
        - name: RELATED_IMAGE_ODH_AI_GATEWAY_PAYLOAD_PROCESSING_IMAGE
          value: latest
---
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: maas-validating-webhook-configuration
  annotations:
    cert-manager.io/inject-ca-from: redhat-ods-applications/maas-controller-webhook-server
webhooks: []
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata:
  name: maas-controller-webhook-server
  namespace: redhat-ods-applications
spec:
  commonName: maas-controller-webhook-service.redhat-ods-applications.svc
  dnsNames: [maas-controller-webhook-service.redhat-ods-applications.svc]
---
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: metrics
---
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: maasmodelrefs.maas.opendatahub.io
spec:
  group: maas.opendatahub.io
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: maas-controller-role
rules:
- apiGroups: [maas.opendatahub.io]
  resources: [maasmodelrefs]
  verbs: [get, list, watch]
"""
        docs = list(yaml.safe_load_all(render_maas(source, "run-123", "registry/controller@sha256:abc",
                                                   "registry/api@sha256:def", "registry/extproc@sha256:123")))
        rb = next(doc for doc in docs if doc["kind"] == "RoleBinding")
        self.assertEqual(rb["subjects"][0]["namespace"], "maas-system")
        deployment = next(doc for doc in docs if doc["kind"] == "Deployment")
        self.assertEqual(deployment["metadata"]["namespace"], "maas-system")
        self.assertEqual(deployment["metadata"]["labels"]["praxis.kind/run"], "run-123")
        self.assertEqual(deployment["metadata"]["labels"]["app.kubernetes.io/managed-by"], "maas-source")
        manager = deployment["spec"]["template"]["spec"]["containers"][0]
        self.assertEqual(manager["image"], "registry/controller@sha256:abc")
        self.assertIn("--metrics-secure=true", manager["args"])
        self.assertIn("ValidatingWebhookConfiguration", [doc["kind"] for doc in docs])
        certificate = next(doc for doc in docs if doc["kind"] == "Certificate")
        self.assertEqual(certificate["metadata"]["namespace"], "maas-system")
        self.assertEqual(certificate["spec"]["commonName"], "maas-controller-webhook-service.maas-system.svc")
        crd = next(doc for doc in docs if doc["kind"] == "CustomResourceDefinition")
        self.assertEqual(crd["spec"]["group"], "maas.opendatahub.io")
        cluster_role = next(doc for doc in docs if doc["kind"] == "ClusterRole")
        self.assertEqual(cluster_role["rules"][0]["apiGroups"], ["maas.opendatahub.io"])
        self.assertNotIn("ServiceMonitor", [doc["kind"] for doc in docs])

    def test_maas_render_rejects_missing_manager(self) -> None:
        with self.assertRaisesRegex(ValueError, "expected exactly one maas-controller Deployment"):
            render_maas("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: c\n", "run-123", "a", "b", "c")

    def test_aigc_render_pins_controller_and_extproc_payload_image(self) -> None:
        source = """
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ai-gateway-controller
  namespace: opendatahub
  labels:
    app.kubernetes.io/managed-by: aigc-source
spec:
  template:
    spec:
      containers:
      - name: manager
        image: quay.io/example/aigc:latest
        env:
        - name: RELATED_IMAGE_ODH_PRAXIS_EXTPROC_IMAGE
          value: latest
      volumes:
      - name: metrics-certs
        secret:
          secretName: ai-gateway-controller-metrics-tls
---
apiVersion: v1
kind: Service
metadata:
  name: ai-gateway-controller-metrics
  namespace: opendatahub
  annotations:
    service.beta.openshift.io/serving-cert-secret-name: ai-gateway-controller-metrics-tls
spec:
  ports:
  - port: 8443
---
apiVersion: monitoring.coreos.com/v1
kind: ServiceMonitor
metadata:
  name: metrics
"""
        docs = list(yaml.safe_load_all(render_aigc(source, "run-123", "registry/aigc@sha256:abc",
                                                   "registry/extproc@sha256:def")))
        deployment = next(doc for doc in docs if doc["kind"] == "Deployment")
        manager = deployment["spec"]["template"]["spec"]["containers"][0]
        self.assertEqual(manager["image"], "registry/aigc@sha256:abc")
        self.assertEqual(manager["env"][0]["value"], "registry/extproc@sha256:def")
        self.assertEqual(deployment["metadata"]["labels"]["praxis.kind/run"], "run-123")
        self.assertEqual(deployment["metadata"]["labels"]["app.kubernetes.io/managed-by"], "aigc-source")
        certificate = next(doc for doc in docs if doc["kind"] == "Certificate")
        self.assertEqual(certificate["spec"]["secretName"], "ai-gateway-controller-metrics-tls")
        self.assertEqual(certificate["spec"]["issuerRef"]["name"], "rhai-ca-issuer")
        service = next(doc for doc in docs if doc["kind"] == "Service")
        self.assertNotIn("service.beta.openshift.io/serving-cert-secret-name", service["metadata"].get("annotations", {}))
        self.assertEqual([doc["kind"] for doc in docs], ["Certificate", "Deployment", "Service"])

    def test_aigc_kind_manifest_bundle_is_mounted_before_controller_rollout(self) -> None:
        source = """
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ai-gateway-controller
  namespace: opendatahub
spec:
  template:
    spec:
      containers:
      - name: manager
        args: [--leader-elect]
        env:
        - name: RELATED_IMAGE_ODH_PRAXIS_EXTPROC_IMAGE
          value: latest
        volumeMounts: []
      volumes:
      - name: metrics-certs
        secret:
          secretName: ai-gateway-controller-metrics-tls
---
apiVersion: v1
kind: Service
metadata:
  name: ai-gateway-controller-metrics
  namespace: opendatahub
spec:
  ports: [{port: 8443}]
"""
        bundle = {
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "praxis-extproc-kind-run-123"},
            "immutable": True,
            "data": {"kustomization.yaml": "resources: [resources.yaml]\n", "resources.yaml": "kind: List\nitems: []\n"},
        }
        docs = list(yaml.safe_load_all(render_aigc(source, "run-123", "registry/aigc@sha256:abc",
                                                   "registry/extproc@sha256:def", bundle)))
        self.assertEqual([doc["kind"] for doc in docs], ["Certificate", "ConfigMap", "Deployment", "Service"])
        deployment = next(doc for doc in docs if doc["kind"] == "Deployment")
        manager = deployment["spec"]["template"]["spec"]["containers"][0]
        self.assertIn("--manifest-path=/config/manifests/kind-native-praxis-extproc", manager["args"])
        self.assertIn({"name": "kind-praxis-manifests", "mountPath": "/config/manifests/kind-native-praxis-extproc", "readOnly": True}, manager["volumeMounts"])
        self.assertIn({"name": "kind-praxis-manifests", "configMap": {"name": "praxis-extproc-kind-run-123"}}, deployment["spec"]["template"]["spec"]["volumes"])
        self.assertEqual(docs[1]["metadata"]["namespace"], "opendatahub")

    def test_aigc_kind_manifest_bundle_requires_immutable_configmap(self) -> None:
        source = """
apiVersion: apps/v1
kind: Deployment
metadata: {name: ai-gateway-controller, namespace: opendatahub}
spec:
  template:
    spec:
      containers:
      - name: manager
        env: [{name: RELATED_IMAGE_ODH_PRAXIS_EXTPROC_IMAGE, value: latest}]
      volumes:
      - name: metrics-certs
        secret: {secretName: ai-gateway-controller-metrics-tls}
---
apiVersion: v1
kind: Service
metadata: {name: ai-gateway-controller-metrics, namespace: opendatahub}
spec: {ports: [{port: 8443}]}
"""
        bundle = {"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "bundle"}, "immutable": False,
                  "data": {"kustomization.yaml": "resources: [resources.yaml]\n", "resources.yaml": "kind: List\n"}}
        with self.assertRaisesRegex(ValueError, "must be immutable"):
            render_aigc(source, "run-123", "registry/aigc@sha256:abc", "registry/extproc@sha256:def", bundle)


if __name__ == "__main__":
    unittest.main()
