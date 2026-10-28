#!/usr/bin/env python3
"""Focused checks for score-based provider-hop attribution preflight."""

import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("validate-pool-route-contract.py")
SPEC = importlib.util.spec_from_file_location("pool_route_contract", SCRIPT)
CONTRACT = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(CONTRACT)


def valid_network():
    return {
        "metadata": {"generation": 4},
        "spec": {"signalTransport": {"mode": "gossip"}},
        "status": {
            "praxisConfigStatus": [
                {"gatewayName": "grid-gateway", "namespace": "grid-task", "configMapName": "praxis", "phase": "Rendered", "observedGeneration": 4}
            ],
            "mountReconciliationStatus": [
                {"gatewayName": "grid-gateway", "namespace": "grid-task", "deploymentName": "grid-gateway", "phase": "Ready", "observedGeneration": 4, "appliedRevision": "sha256:abc", "reason": ""}
            ],
        },
    }


def valid_gateway_ref():
    return {
        "name": "grid-gateway",
        "namespace": "grid-task",
        "praxisConfig": {
            "generate": True,
            "configMapName": "praxis",
            "siteRouter": "intelligentRoute",
            "clusterEndpoints": [
                {"cluster": "pool-a", "address": "proxy-a.grid-task.svc:8443", "transport": {"mode": "mutual_tls", "sni": "grid-site.internal"}},
                {"cluster": "pool-b", "address": "proxy-b.grid-task.svc:8443", "transport": {"mode": "mutual_tls", "sni": "grid-site.internal"}},
            ],
            "mountReconciliation": {"enabled": True, "deploymentName": "grid-gateway"},
        },
        "providerHopEndpoints": [
            {"cluster": "pool-a", "transport": {"mode": "mutual_tls", "sni": "grid-site.internal"}},
            {"cluster": "pool-b", "transport": {"mode": "mutual_tls", "sni": "grid-site.internal"}},
        ],
    }


class SiteAttributionContractTest(unittest.TestCase):
    def test_intelligent_route_with_gossip_and_matching_mtls_hops_passes(self):
        endpoints = CONTRACT.validate_intelligent_provider_hops(valid_network(), valid_gateway_ref())
        self.assertEqual([entry["cluster"] for entry in endpoints], ["pool-a", "pool-b"])

    def test_grid_site_route_is_rejected_for_score_based_provider_attribution(self):
        gateway = valid_gateway_ref()
        gateway["praxisConfig"]["siteRouter"] = "gridSiteRoute"
        with self.assertRaisesRegex(RuntimeError, "requires GridNetwork praxisConfig.siteRouter=intelligentRoute"):
            CONTRACT.validate_intelligent_provider_hops(valid_network(), gateway)

    def test_poll_is_rejected_when_overlay_scores_are_required(self):
        network = valid_network()
        network["spec"]["signalTransport"]["mode"] = "poll"
        with self.assertRaisesRegex(RuntimeError, "Poll signal transport disables"):
            CONTRACT.validate_intelligent_provider_hops(network, valid_gateway_ref())

    def test_provider_hop_sni_must_match_generated_mtls_endpoint(self):
        gateway = valid_gateway_ref()
        gateway["providerHopEndpoints"][1]["transport"]["sni"] = "other-site.internal"
        with self.assertRaisesRegex(RuntimeError, "must exactly match"):
            CONTRACT.validate_intelligent_provider_hops(valid_network(), gateway)

    def test_provider_hop_allowlist_must_cover_both_mtls_clusters(self):
        gateway = valid_gateway_ref()
        gateway["providerHopEndpoints"].pop()
        with self.assertRaisesRegex(RuntimeError, "must exactly match"):
            CONTRACT.validate_intelligent_provider_hops(valid_network(), gateway)

    def test_serving_config_must_bind_both_current_clusters(self):
        config = {
            "candidates": [
                {"kind": "inference_model", "cluster": "pool-a"},
                {"kind": "inference_model", "cluster": "pool-b"},
            ]
        }
        self.assertEqual(CONTRACT.validate_serving_candidates(config, ["pool-a", "pool-b"]), ["pool-a", "pool-b"])
        with self.assertRaisesRegex(RuntimeError, "do not match the two current provider endpoint clusters"):
            CONTRACT.validate_serving_candidates(config, ["pool-a", "pool-c"])

    def test_grid_serving_mount_and_healthy_probe_pass(self):
        deployment = {
            "spec": {
                "template": {
                    "spec": {
                        "containers": [
                            {
                                "name": "praxis",
                                "env": [{"name": "GRID_SERVING_CONFIG", "value": "/etc/praxis/grid-serving/serving-config.json"}],
                                "volumeMounts": [{"name": "grid-serving", "mountPath": "/etc/praxis/grid-serving"}],
                                "readinessProbe": {"exec": {"command": ["/healthcheck", "http://127.0.0.1:9901/healthy"]}},
                            }
                        ],
                        "volumes": [{"name": "grid-serving", "configMap": {"name": "grid-serving-grid-grid-gateway"}}],
                    }
                }
            }
        }
        self.assertEqual(
            CONTRACT.validate_grid_serving_mount(deployment, "grid-task", "grid", "grid-gateway", "praxis"),
            "grid-serving-grid-grid-gateway",
        )

    def test_ready_probe_is_rejected_when_provider_hops_make_healthy_required(self):
        deployment = {
            "spec": {
                "template": {
                    "spec": {
                        "containers": [
                            {
                                "name": "praxis",
                                "env": [{"name": "GRID_SERVING_CONFIG", "value": "/etc/praxis/grid-serving/serving-config.json"}],
                                "volumeMounts": [{"name": "grid-serving", "mountPath": "/etc/praxis/grid-serving"}],
                                "readinessProbe": {"httpGet": {"path": "/ready", "port": 9901}},
                            }
                        ],
                        "volumes": [{"name": "grid-serving", "configMap": {"name": "grid-serving-grid-grid-gateway"}}],
                    }
                }
            }
        }
        with self.assertRaisesRegex(RuntimeError, "must probe Praxis /healthy"):
            CONTRACT.validate_grid_serving_mount(deployment, "grid-task", "grid", "grid-gateway", "praxis")


if __name__ == "__main__":
    unittest.main()
