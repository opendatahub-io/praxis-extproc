#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Contract tests for the live payload Service/Gateway TLS checker."""

import copy
import unittest

from verify_kind_payload_tls_config import check


CA = "-----BEGIN CERTIFICATE-----\nY2E=\n-----END CERTIFICATE-----\n"
NAMESPACE = "maas-system"
BASE_SERVICES = ("payload-pre-processing", "payload-processing")
TLS_TYPE = "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext"


def inputs(pool=None):
    services = tuple(f"{service}-{pool}" if pool else service for service in BASE_SERVICES)
    gateway = f"grid-llmd-{pool}" if pool else "maas-default-gateway"
    filter_name = f"payload-processing-{pool}" if pool else "payload-processing"
    rules = []
    patches = []
    clusters = []
    for service in services:
        processor = service.removesuffix(f"-{pool}") if pool else service
        fqdn = f"{service}.{NAMESPACE}.svc.cluster.local"
        rules.append(
            {
                "metadata": {"name": service},
                "spec": {
                    "host": fqdn,
                    "workloadSelector": {
                        "matchLabels": {"gateway.networking.k8s.io/gateway-name": gateway}
                    },
                    "trafficPolicy": {
                        "tls": {
                            "mode": "SIMPLE",
                            "credentialName": "payload-extproc-serving-ca",
                            "sni": fqdn,
                            "subjectAltNames": [fqdn],
                        }
                    },
                },
            }
        )
        cluster = {
            "name": f"{processor}-extproc",
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
                "typed_config": {
                    "@type": TLS_TYPE,
                    "sni": fqdn,
                    "common_tls_context": {
                        "validation_context": {
                            "trusted_ca": {"inline_string": CA},
                            "match_typed_subject_alt_names": [
                                {"san_type": "DNS", "matcher": {"exact": fqdn}}
                            ],
                        }
                    },
                }
            },
        }
        patches.append({"applyTo": "CLUSTER", "patch": {"value": cluster}})
        clusters.append(cluster)
    envoy_filter = {
        "metadata": {"name": filter_name},
        "spec": {
            "workloadSelector": {
                "labels": {"gateway.networking.k8s.io/gateway-name": gateway}
            },
            "configPatches": patches,
        },
    }
    xds = {"configs": [{"dynamic_active_clusters": [{"cluster": cluster} for cluster in clusters]}]}
    return rules, envoy_filter, xds


class PayloadTlsConfigTests(unittest.TestCase):
    def test_accepts_matching_service_dns_sni_san_and_ca_in_active_xds(self):
        result = check(*inputs(), CA, NAMESPACE)
        self.assertEqual(result["status"], "PASS")
        self.assertEqual({item["dnsName"] for item in result["services"]}, {
            f"{service}.{NAMESPACE}.svc.cluster.local" for service in BASE_SERVICES
        })

    def test_accepts_pool_scoped_tenant_services_and_gateway(self):
        rules, envoy_filter, xds = inputs("pool-a")
        result = check(rules, envoy_filter, xds, CA, NAMESPACE, "pool-a")
        self.assertEqual(result["gateway"], "grid-llmd-pool-a")
        self.assertEqual(
            {item["dnsName"] for item in result["services"]},
            {f"{service}-pool-a.{NAMESPACE}.svc.cluster.local" for service in BASE_SERVICES},
        )

    def test_rejects_tenant_filter_scoped_to_another_gateway(self):
        rules, envoy_filter, xds = inputs("pool-a")
        envoy_filter["spec"]["workloadSelector"]["labels"]["gateway.networking.k8s.io/gateway-name"] = "maas-default-gateway"
        with self.assertRaisesRegex(ValueError, "not scoped to Gateway"):
            check(rules, envoy_filter, xds, CA, NAMESPACE, "pool-a")

    def test_rejects_destination_rule_sni_drift(self):
        rules, envoy_filter, xds = inputs()
        rules[0]["spec"]["trafficPolicy"]["tls"]["sni"] = "wrong.svc.cluster.local"
        with self.assertRaisesRegex(ValueError, "SNI/SAN"):
            check(rules, envoy_filter, xds, CA, NAMESPACE)

    def test_rejects_active_xds_ca_drift(self):
        rules, envoy_filter, xds = inputs()
        xds = copy.deepcopy(xds)
        xds["configs"][0]["dynamic_active_clusters"][0]["cluster"]["transport_socket"]["typed_config"][
            "common_tls_context"
        ]["validation_context"]["trusted_ca"] = {"inline_string": "wrong CA"}
        with self.assertRaisesRegex(ValueError, "does not trust the run CA"):
            check(rules, envoy_filter, xds, CA, NAMESPACE)

    def test_rejects_missing_active_gateway_cluster(self):
        rules, envoy_filter, _ = inputs()
        with self.assertRaisesRegex(ValueError, "exactly one active"):
            check(rules, envoy_filter, {"configs": []}, CA, NAMESPACE)


if __name__ == "__main__":
    unittest.main()
