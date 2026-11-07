#!/usr/bin/env python3
"""Regression tests for Kind AIGC payload TLS and manifest rendering."""

import unittest

import yaml

from render_kind_aigc_manifest_bundle import render, render_certificates


TEST_CA = "-----BEGIN CERTIFICATE-----\nYWJj\n-----END CERTIFICATE-----\n"
NAMESPACE = "maas-system"
SERVICES = ("payload-pre-processing", "payload-processing")


def fixture_source() -> str:
    docs = [
        {
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {"name": "payload-processing-plugins"},
            "data": {
                "pre-extproc.yaml": yaml.safe_dump(
                    {"server": {"tls": {"mode": "provided", "cert_path": "/etc/tls/private/tls.crt", "key_path": "/etc/tls/private/tls.key"}}}
                ),
                "extproc.yaml": yaml.safe_dump(
                    {"server": {"tls": {"mode": "provided", "cert_path": "/etc/tls/private/tls.crt", "key_path": "/etc/tls/private/tls.key"}}}
                ),
            },
        }
    ]
    for service in SERVICES:
        secret = f"{service}-tls"
        docs.extend(
            [
                {
                    "apiVersion": "v1",
                    "kind": "Service",
                    "metadata": {
                        "name": service,
                        "annotations": {"service.beta.openshift.io/serving-cert-secret-name": secret},
                    },
                    "spec": {"ports": [{"port": 9004, "appProtocol": "HTTP2"}]},
                },
                {
                    "apiVersion": "apps/v1",
                    "kind": "Deployment",
                    "metadata": {"name": service},
                    "spec": {
                        "template": {
                            "spec": {
                                "containers": [
                                    {
                                        "name": service,
                                        "ports": [{"name": "grpc", "containerPort": 9004}],
                                        "volumeMounts": [{"name": "tls", "mountPath": "/etc/tls/private", "readOnly": True}],
                                    }
                                ],
                                "volumes": [{"name": "tls", "secret": {"secretName": secret}}],
                            }
                        }
                    },
                },
                {
                    "apiVersion": "networking.istio.io/v1",
                    "kind": "DestinationRule",
                    "metadata": {"name": service},
                    "spec": {
                        "host": f"{service}.openshift-ingress.svc.cluster.local",
                        "trafficPolicy": {
                            "tls": {
                                "mode": "SIMPLE",
                                "caCertificates": "/var/run/secrets/kubernetes.io/serviceaccount/service-ca.crt",
                                "sni": f"{service}.openshift-ingress.svc.cluster.local",
                                "subjectAltNames": [f"{service}.openshift-ingress.svc.cluster.local"],
                            }
                        },
                    },
                },
            ]
        )

    patches = []
    for service in SERVICES:
        fqdn = f"{service}.openshift-ingress.svc.cluster.local"
        patches.append(
            {
                "applyTo": "CLUSTER",
                "patch": {
                    "operation": "ADD",
                    "value": {
                        "name": f"{service}-extproc",
                        "type": "STRICT_DNS",
                        "load_assignment": {
                            "endpoints": [
                                {
                                    "lb_endpoints": [
                                        {
                                            "endpoint": {
                                                "address": {"socket_address": {"address": fqdn, "port_value": 9004}}
                                            }
                                        }
                                    ]
                                }
                            ]
                        },
                        "transport_socket": {
                            "name": "envoy.transport_sockets.tls",
                            "typed_config": {
                                "@type": "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext",
                                "sni": fqdn,
                                "common_tls_context": {
                                    "validation_context": {
                                        "trusted_ca": {
                                            "filename": "/var/run/secrets/kubernetes.io/serviceaccount/service-ca.crt"
                                        },
                                        "match_typed_subject_alt_names": [
                                            {"san_type": "DNS", "matcher": {"exact": fqdn}}
                                        ],
                                    }
                                },
                            },
                        },
                    },
                },
            }
        )
    docs.append(
        {
            "apiVersion": "networking.istio.io/v1alpha3",
            "kind": "EnvoyFilter",
            "metadata": {"name": "payload-processing"},
            "spec": {"configPatches": patches},
        }
    )
    return yaml.safe_dump_all(docs, sort_keys=False)


def rendered_resources() -> tuple[dict, list[dict]]:
    bundle = render(
        fixture_source(),
        "run-123",
        "opendatahub",
        "kind-praxis-extproc-run-123",
        payload_namespace=NAMESPACE,
        ca_bundle=TEST_CA,
    )
    return bundle, list(yaml.safe_load_all(bundle["data"]["resources.yaml"]))


class KindAIGCManifestBundleTests(unittest.TestCase):
    def test_patches_only_kind_owned_payload_tls_contract(self) -> None:
        bundle, docs = rendered_resources()
        self.assertTrue(bundle["immutable"])
        self.assertEqual(bundle["metadata"]["labels"]["praxis.kind/run"], "run-123")
        deployments = {d["metadata"]["name"]: d for d in docs if d["kind"] == "Deployment"}
        services = {d["metadata"]["name"]: d for d in docs if d["kind"] == "Service"}
        for name in SERVICES:
            self.assertEqual(
                deployments[name]["spec"]["template"]["metadata"]["annotations"][
                    "traffic.sidecar.istio.io/excludeInboundPorts"
                ],
                "9004",
            )
            self.assertNotIn("service.beta.openshift.io/serving-cert-secret-name", services[name]["metadata"].get("annotations", {}))
            self.assertEqual(services[name]["spec"]["ports"][0]["appProtocol"], "HTTP2")
        self.assertEqual(yaml.safe_load(bundle["data"]["kustomization.yaml"])["resources"], ["resources.yaml"])

    def test_certificates_cover_exact_service_dns_and_use_run_issuer(self) -> None:
        certificates = render_certificates("run-123", NAMESPACE)
        expected_services = set(SERVICES) | {
            f"{service}-{pool}" for service in SERVICES for pool in ("pool-a", "pool-b")
        }
        self.assertEqual(
            {cert["metadata"]["name"] for cert in certificates},
            {f"{name}-tls" for name in expected_services},
        )
        by_secret = {cert["spec"]["secretName"]: cert for cert in certificates}
        for name in expected_services:
            cert = by_secret[f"{name}-tls"]
            self.assertEqual(cert["metadata"]["namespace"], NAMESPACE)
            self.assertEqual(cert["metadata"]["labels"]["praxis.kind/run"], "run-123")
            self.assertEqual(cert["spec"]["dnsNames"], [f"{name}.{NAMESPACE}.svc.cluster.local"])
            self.assertEqual(cert["spec"]["secretTemplate"]["labels"]["praxis.kind/run"], "run-123")
            self.assertNotIn("metadata", cert["spec"]["secretTemplate"])
            self.assertEqual(
                cert["spec"]["issuerRef"],
                {"name": "rhai-ca-issuer", "kind": "ClusterIssuer", "group": "cert-manager.io"},
            )

    def test_tenant_pool_certificates_use_pool_service_names(self) -> None:
        certificates = render_certificates("run-123", NAMESPACE)
        by_name = {cert["metadata"]["name"]: cert for cert in certificates}
        for pool in ("pool-a", "pool-b"):
            for service in SERVICES:
                full_service = f"{service}-{pool}"
                cert = by_name[f"{full_service}-tls"]
                self.assertEqual(cert["spec"]["secretName"], f"{full_service}-tls")
                self.assertEqual(
                    cert["spec"]["dnsNames"],
                    [f"{full_service}.{NAMESPACE}.svc.cluster.local"],
                )

    def test_envoy_and_destination_rules_use_matching_dns_sni_and_ca_trust(self) -> None:
        _, docs = rendered_resources()
        rules = {d["metadata"]["name"]: d for d in docs if d["kind"] == "DestinationRule"}
        envoy = next(d for d in docs if d["kind"] == "EnvoyFilter")
        clusters = {
            p["patch"]["value"]["name"]: p["patch"]["value"]
            for p in envoy["spec"]["configPatches"]
        }
        for service in SERVICES:
            fqdn = f"{service}.{NAMESPACE}.svc.cluster.local"
            tls = rules[service]["spec"]["trafficPolicy"]["tls"]
            self.assertEqual(rules[service]["spec"]["host"], fqdn)
            self.assertEqual(tls["mode"], "SIMPLE")
            self.assertEqual(tls["sni"], fqdn)
            self.assertEqual(tls["subjectAltNames"], [fqdn])
            self.assertEqual(tls["credentialName"], "payload-extproc-serving-ca")
            self.assertEqual(rules[service]["spec"]["workloadSelector"]["matchLabels"], {
                "gateway.networking.k8s.io/gateway-name": "maas-default-gateway"
            })
            cluster = clusters[f"{service}-extproc"]
            self.assertEqual(
                cluster["load_assignment"]["endpoints"][0]["lb_endpoints"][0]["endpoint"]["address"]["socket_address"]["address"],
                fqdn,
            )
            context = cluster["transport_socket"]["typed_config"]
            self.assertEqual(context["sni"], fqdn)
            validation = context["common_tls_context"]["validation_context"]
            self.assertEqual(validation["match_typed_subject_alt_names"], [{"san_type": "DNS", "matcher": {"exact": fqdn}}])
            self.assertEqual(validation["trusted_ca"], {"inline_string": TEST_CA})
        self.assertNotIn("service-ca.crt", yaml.safe_dump(docs))

    def test_rejects_wrong_secret_reference_and_openshift_cert_target(self) -> None:
        source = fixture_source().replace(
            "secretName: payload-pre-processing-tls",
            "secretName: other-secret",
            1,
        )
        with self.assertRaisesRegex(ValueError, "TLS volume must reference payload-pre-processing-tls"):
            render(source, "run-123", "opendatahub", "bundle", payload_namespace=NAMESPACE, ca_bundle=TEST_CA)

        source = fixture_source().replace(
            "payload-pre-processing-tls",
            "unrelated-secret",
            1,
        )
        with self.assertRaisesRegex(ValueError, "annotation has unexpected Secret"):
            render(source, "run-123", "opendatahub", "bundle", payload_namespace=NAMESPACE, ca_bundle=TEST_CA)

    def test_requires_both_processor_deployments(self) -> None:
        source = fixture_source().split("---", 1)[0]
        with self.assertRaisesRegex(ValueError, "Deployments"):
            render(source, "run-123", "opendatahub", "bundle", payload_namespace=NAMESPACE, ca_bundle=TEST_CA)


if __name__ == "__main__":
    unittest.main()
