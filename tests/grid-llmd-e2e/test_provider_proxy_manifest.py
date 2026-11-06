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
    "ab" * 32,
    "provider-pool-a",
    "stable-candidate-a",
    "grid-test-model",
    "/publishers/grid-task/models/grid-test-model/v1/chat/completions",
    "pool-a",
    "pool-a-gateway.grid-task.svc.cluster.local:8080",
    "grid-task",
    "grid-llmd-client",
)
config = yaml.safe_load(config_text)
filters = config["filter_chains"][0]["filters"]
assert config["admin"] == {"metrics_address": "127.0.0.1:9901"}
assert config["listeners"][0]["tls"]["client_cert_mode"] == "require"
assert config["listeners"][0]["tls"]["client_ca"]["ca_path"] == "/etc/praxis/ca-source/ca.crt"
assert filters[0] == {"filter": "peer_identity_trust", "trusted_peers": [{"cert_digest": "ab" * 32}]}
route = next(item for item in filters if item["filter"] == "provider_route")
assert route["provider_id"] == "provider-pool-a"
assert route["emit_demo_attribution"] is True
assert route["routes"] == [
    {
        "candidate_id": "stable-candidate-a",
        "model": "grid-test-model",
        "paths": ["/publishers/grid-task/models/grid-test-model/v1/chat/completions"],
        "cluster": "pool-a-backend",
        "credential": {
            "strategy": "bearer_token",
            "secretRef": {"name": "grid-llmd-client", "namespace": "grid-task", "key": "token"},
        },
    }
]
path_rewrite = next(item for item in filters if item["filter"] == "path_rewrite")
assert path_rewrite == {"filter": "path_rewrite", "strip_prefix": "/publishers/grid-task/models/grid-test-model"}
trusted_model_header = next(item for item in filters if item["filter"] == "headers")
assert trusted_model_header["request_set"] == [
    {"name": "X-Gateway-Model-Name", "value": "publishers/grid-task/models/grid-test-model"}
]
credential_inject = next(item for item in filters if item["filter"] == "credential_inject")
assert credential_inject["credentials"] == [
    {
        "name": "grid-llmd-client",
        "namespace": "grid-task",
        "key": "token",
        "strategy": "bearer_token",
        "file": "/var/run/grid-llmd-client/token",
    }
]
filter_order = [item["filter"] for item in filters]
assert filter_order.index("provider_route") < filter_order.index("path_rewrite") < filter_order.index("headers") < filter_order.index("credential_inject") < filter_order.index("load_balancer")
backend = next(item for item in filters if item["filter"] == "load_balancer")["clusters"][0]
assert backend["endpoints"] == ["pool-a-gateway.grid-task.svc.cluster.local:8080"]
assert "provider-secret" not in config_text

try:
    PROXY.make_provider_config(
        "ab" * 32,
        "provider-pool-a",
        "stable-candidate-a",
        "grid-test-model",
        "/publishers/grid-task/models/other-model/v1/chat/completions",
        "pool-a",
        "pool-a-gateway.grid-task.svc.cluster.local:8080",
        "grid-task",
        "grid-llmd-client",
    )
except RuntimeError as error:
    assert "configured publisher model" in str(error)
else:
    raise AssertionError("provider proxy must reject a publisher path for a different model")

VALIDATOR_SCRIPT = Path(__file__).with_name("validate-pool-route-contract.py")
VALIDATOR_SPEC = importlib.util.spec_from_file_location("pool_route_contract", VALIDATOR_SCRIPT)
VALIDATOR = importlib.util.module_from_spec(VALIDATOR_SPEC)
assert VALIDATOR_SPEC and VALIDATOR_SPEC.loader
VALIDATOR_SPEC.loader.exec_module(VALIDATOR)
pool_route = {
    "matches": [
        {
            "path": {"type": "Exact", "value": "/v1/chat/completions"},
            "headers": [
                {
                    "type": "Exact",
                    "name": "X-Gateway-Model-Name",
                    "value": "publishers/grid-task/models/grid-test-model",
                }
            ],
        }
    ]
}
assert VALIDATOR.route_match_with_gateway_model_header(
    pool_route, "/v1/chat/completions", "publishers/grid-task/models/grid-test-model"
)
assert not VALIDATOR.route_match_with_gateway_model_header(
    pool_route, "/publishers/grid-task/models/grid-test-model/v1/chat/completions", "publishers/grid-task/models/grid-test-model"
)
assert not VALIDATOR.route_match_with_gateway_model_header(
    pool_route, "/v1/chat/completions", "publishers/grid-task/models/other-model"
)

run_label = "run-kind-grid-llmd"
llmisvc = {
    "metadata": {"name": "pool-a", "namespace": "grid-task", "labels": {"praxis.fast/qualification-run": run_label}},
    "spec": {"router": {"gateway": {"refs": [{"name": "grid-llmd-pool-a", "namespace": "maas-system"}]}}},
}
gateway = {
    "metadata": {
        "name": "grid-llmd-pool-a",
        "namespace": "maas-system",
        "uid": "gateway-uid-a",
        "generation": 3,
        "labels": {"praxis.fast/qualification-run": run_label},
    },
    "status": {
        "conditions": [
            {"type": "Accepted", "status": "True", "observedGeneration": 3},
            {"type": "Programmed", "status": "True", "observedGeneration": 3},
        ],
        "listeners": [
            {
                "name": "inference",
                "attachedRoutes": 1,
                "conditions": [
                    {"type": kind, "status": "True", "observedGeneration": 3}
                    for kind in ("Accepted", "Programmed", "ResolvedRefs")
                ],
            }
        ],
    },
}
gateway_service = {
    "metadata": {
        "name": "grid-llmd-pool-a-istio",
        "namespace": "maas-system",
        "uid": "gateway-service-uid-a",
        "labels": {
            "praxis.fast/qualification-run": run_label,
            "gateway.networking.k8s.io/gateway-name": "grid-llmd-pool-a",
        },
        "ownerReferences": [{"kind": "Gateway", "uid": "gateway-uid-a"}],
    },
    "spec": {"ports": [{"port": 8080}]},
}
gateway_endpoint_slice = {
    "metadata": {"labels": {"kubernetes.io/service-name": "grid-llmd-pool-a-istio"}},
    "endpoints": [{"conditions": {"ready": True}, "addresses": ["10.0.0.12"], "targetRef": {"kind": "Pod", "uid": "gateway-pod-uid-a"}}],
}
target = PROXY.resolve_gateway_backend_objects(
    llmisvc, gateway, [gateway_service], [gateway_endpoint_slice], run_label, "grid-llmd-pool-a", "pool-a"
)
assert target["gatewayNamespace"] == "maas-system"
assert target["endpoint"] == "grid-llmd-pool-a-istio.maas-system.svc.cluster.local:8080"
assert target["gatewayUID"] == "gateway-uid-a"
assert target["serviceUID"] == "gateway-service-uid-a"
assert target["readyEndpoints"][0]["targetRef"]["uid"] == "gateway-pod-uid-a"

for invalid_llmisvc, invalid_gateway, invalid_services, invalid_slices, message in (
    (
        {**llmisvc, "spec": {"router": {"gateway": {"refs": [{"name": "unused-task-gateway", "namespace": "grid-task"}]}}}},
        gateway,
        [gateway_service],
        [gateway_endpoint_slice],
        "does not select exactly",
    ),
    (
        llmisvc,
        {**gateway, "status": {**gateway["status"], "conditions": [gateway["status"]["conditions"][0], {"type": "Programmed", "status": "True", "observedGeneration": 2}]}},
        [gateway_service],
        [gateway_endpoint_slice],
        "current Programmed",
    ),
    (
        llmisvc,
        gateway,
        [{**gateway_service, "metadata": {**gateway_service["metadata"], "ownerReferences": [{"kind": "Gateway", "uid": "other-gateway"}]}}],
        [gateway_endpoint_slice],
        "no unique run-owned listener Service",
    ),
    (
        llmisvc,
        gateway,
        [gateway_service],
        [{**gateway_endpoint_slice, "endpoints": [{**gateway_endpoint_slice["endpoints"][0], "conditions": {"ready": False}}]}],
        "no Ready EndpointSlice",
    ),
):
    try:
        PROXY.resolve_gateway_backend_objects(
            invalid_llmisvc, invalid_gateway, invalid_services, invalid_slices, run_label, "grid-llmd-pool-a", "pool-a"
        )
    except RuntimeError as error:
        assert message in str(error), str(error)
    else:
        raise AssertionError(f"gateway target resolver accepted invalid state: {message}")

resources = PROXY.resources(
    "run-20261027",
    "grid-task",
    "pool-a",
    "provider-pool-a",
    "registry.local/grid-task/praxis-ai@sha256:" + "a" * 64,
    "grid-site-identity",
    "grid-ca",
    config_text,
    "grid-llmd-client",
)
config_map, deployment, service = resources
assert config_map["metadata"]["labels"]["praxis.fast/component"] == "provider-proxy"
assert deployment["spec"]["replicas"] == 1
pod_spec = deployment["spec"]["template"]["spec"]
assert pod_spec["automountServiceAccountToken"] is False
assert pod_spec["serviceAccountName"] == "grid-llmd-client"
assert pod_spec["securityContext"] == {"fsGroup": 101, "fsGroupChangePolicy": "OnRootMismatch"}
assert deployment["spec"]["template"]["metadata"]["annotations"]["praxis.fast/config-sha256"] == hashlib.sha256(config_text.encode()).hexdigest()
container = pod_spec["containers"][0]
assert container["image"].endswith("@sha256:" + "a" * 64)
assert container["args"] == ["--config", "/etc/praxis/praxis.yaml"]
assert container["readinessProbe"]["exec"]["command"][-1] == "http://127.0.0.1:9901/healthy"
mounts = {item["name"]: item["mountPath"] for item in container["volumeMounts"]}
assert mounts["ca"] == "/etc/praxis/ca-source"
assert pod_spec["volumes"][2]["secret"]["secretName"] == "grid-ca"
assert pod_spec["volumes"][1]["secret"]["defaultMode"] == 288
assert pod_spec["volumes"][2]["secret"]["defaultMode"] == 288
provider_token = next(item for item in pod_spec["volumes"] if item["name"] == "provider-token")["projected"]
assert provider_token["defaultMode"] == 420
assert provider_token["sources"] == [
    {"serviceAccountToken": {"audience": "https://kubernetes.default.svc", "expirationSeconds": 3600, "path": "token"}}
]
assert {item["name"]: item for item in container["volumeMounts"]}["provider-token"] == {
    "name": "provider-token",
    "mountPath": "/var/run/grid-llmd-client",
    "readOnly": True,
}
assert service["spec"]["ports"][0]["port"] == 8443

sans = PROXY.validate_site_server_name(
    "X509v3 Subject Alternative Name:\n    DNS:grid-llmd-site.grid.internal, DNS:grid-llmd-site",
    "grid-llmd-site.grid.internal",
)
assert sans == ["grid-llmd-site.grid.internal", "grid-llmd-site"]
try:
    PROXY.validate_site_server_name("X509v3 Subject Alternative Name:\n    DNS:other.internal", "grid-llmd-site.grid.internal")
except RuntimeError as error:
    assert "does not match" in str(error)
else:
    raise AssertionError("provider-hop SNI must match the current GridSite certificate SAN")

grid_endpoints, provider_hops = PROXY.make_grid_endpoint_inventories(
    [
        {"provider": "provider-pool-b", "pool": "pool-b", "service": "praxis-provider-pool-b", "readyEndpointCount": 1},
        {"provider": "provider-pool-a", "pool": "pool-a", "service": "praxis-provider-pool-a", "readyEndpointCount": 1},
    ],
    "grid-task",
    "grid-llmd-site.grid.internal",
)
assert grid_endpoints == [
    {
        "cluster": "provider-pool-a",
        "address": "praxis-provider-pool-a.grid-task.svc.cluster.local:8443",
        "transport": {"mode": "mutual_tls", "sni": "grid-llmd-site.grid.internal"},
    },
    {
        "cluster": "provider-pool-b",
        "address": "praxis-provider-pool-b.grid-task.svc.cluster.local:8443",
        "transport": {"mode": "mutual_tls", "sni": "grid-llmd-site.grid.internal"},
    },
]
assert provider_hops == [
    {"cluster": "provider-pool-a", "transport": {"mode": "mutual_tls", "sni": "grid-llmd-site.grid.internal"}},
    {"cluster": "provider-pool-b", "transport": {"mode": "mutual_tls", "sni": "grid-llmd-site.grid.internal"}},
]
for invalid_proxies in (
    [{"provider": "provider-pool-a", "pool": "pool-a"}],
    [
        {"provider": "provider-pool-a", "pool": "pool-a", "readyEndpointCount": 1},
        {"provider": "provider-pool-a", "pool": "pool-b", "readyEndpointCount": 1},
    ],
    [
        {"provider": "provider-pool-a", "pool": "pool-a", "readyEndpointCount": 1},
        {"provider": "provider-pool-b", "pool": "pool-b", "readyEndpointCount": 0},
    ],
):
    try:
        PROXY.make_grid_endpoint_inventories(invalid_proxies, "grid-task", "grid-llmd-site.grid.internal")
    except RuntimeError:
        pass
    else:
        raise AssertionError("Grid endpoint inventory must reject incomplete, duplicate, or unready provider proxies")

print("Provider proxy mTLS, candidate, LLMInferenceService-selected Gateway, attribution, and image-pin contract passed")
