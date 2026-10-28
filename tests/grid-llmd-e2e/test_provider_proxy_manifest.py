#!/usr/bin/env python3
"""Contract checks for the mTLS provider-proxy manifest generator."""

import importlib.util
import hashlib
from pathlib import Path

import yaml


SCRIPT = Path(__file__).with_name("deploy-provider-proxies.py")
SPEC = importlib.util.spec_from_file_location("provider_proxy", SCRIPT)
PROXY = importlib.util.module_from_spec(SPEC)
assert SPEC and SPEC.loader
SPEC.loader.exec_module(PROXY)


config_text = PROXY.make_provider_config(
    "grid-site",
    "provider-pool-a",
    "stable-candidate-a",
    "grid-test-model",
    "/publishers/grid-task/models/grid-test-model/v1/chat/completions",
    "pool-a",
    "pool-a-gateway.grid-task.svc.cluster.local:8080",
)
config = yaml.safe_load(config_text)
filters = config["filter_chains"][0]["filters"]
assert config["admin"] == {"metrics_address": "127.0.0.1:9901"}
assert config["listeners"][0]["tls"]["client_cert_mode"] == "require"
assert config["listeners"][0]["tls"]["client_ca"]["ca_path"] == "/etc/praxis/ca-source/ca.crt"
assert filters[0] == {"filter": "peer_identity_trust", "trusted_peers": [{"organization": "grid-site"}]}
route = next(item for item in filters if item["filter"] == "provider_route")
assert route["provider_id"] == "provider-pool-a"
assert route["emit_demo_attribution"] is True
assert route["routes"] == [
    {
        "candidate_id": "stable-candidate-a",
        "model": "grid-test-model",
        "paths": ["/publishers/grid-task/models/grid-test-model/v1/chat/completions"],
        "cluster": "pool-a-backend",
    }
]
backend = next(item for item in filters if item["filter"] == "load_balancer")["clusters"][0]
assert backend["endpoints"] == ["pool-a-gateway.grid-task.svc.cluster.local:8080"]
assert not any("credential" in key.lower() for key in config)

resources = PROXY.resources(
    "run-20261027",
    "grid-task",
    "pool-a",
    "provider-pool-a",
    "registry.local/grid-task/praxis-ai@sha256:" + "a" * 64,
    "grid-site-identity",
    "grid-ca",
    config_text,
)
config_map, deployment, service = resources
assert config_map["metadata"]["labels"]["praxis.fast/component"] == "provider-proxy"
assert deployment["spec"]["replicas"] == 1
pod_spec = deployment["spec"]["template"]["spec"]
assert pod_spec["automountServiceAccountToken"] is False
assert deployment["spec"]["template"]["metadata"]["annotations"]["praxis.fast/config-sha256"] == hashlib.sha256(config_text.encode()).hexdigest()
container = pod_spec["containers"][0]
assert container["image"].endswith("@sha256:" + "a" * 64)
assert container["args"] == ["--config", "/etc/praxis/praxis.yaml"]
assert container["readinessProbe"]["exec"]["command"][-1] == "http://127.0.0.1:9901/healthy"
mounts = {item["name"]: item["mountPath"] for item in container["volumeMounts"]}
assert mounts["ca"] == "/etc/praxis/ca-source"
assert pod_spec["volumes"][2]["secret"]["secretName"] == "grid-ca"
assert service["spec"]["ports"][0]["port"] == 8443

print("Provider proxy mTLS, candidate, backend, attribution, and image-pin contract passed")
