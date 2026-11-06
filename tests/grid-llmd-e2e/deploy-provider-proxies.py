#!/usr/bin/env python3
"""Render and deploy task-owned mTLS Praxis AI provider hops for two Grid pools."""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import yaml

CLIENT_TOKEN_AUDIENCE = "https://kubernetes.default.svc"
CLIENT_TOKEN_DIRECTORY = "/var/run/grid-llmd-client"
CLIENT_TOKEN_PATH = f"{CLIENT_TOKEN_DIRECTORY}/token"


def run_oc(context: str, *args: str, stdin: str | None = None) -> str:
    command = ["oc", f"--context={context}", *args]
    result = subprocess.run(command, input=stdin, text=True, capture_output=True, check=False)
    if result.returncode:
        raise RuntimeError(f"oc {' '.join(args[:4])} failed: {result.stderr.strip()}")
    return result.stdout


def get_json(context: str, *args: str) -> dict:
    return json.loads(run_oc(context, *args, "-o", "json"))


def require_owned(obj: dict, run_label: str, kind: str) -> None:
    labels = obj.get("metadata", {}).get("labels", {})
    if labels.get("praxis.fast/qualification-run") != run_label:
        raise RuntimeError(f"{kind} is not labeled for the current qualification run")


def current_provider(context: str, name: str, run_label: str) -> dict:
    provider = get_json(context, "get", "inferenceprovider", name)
    require_owned(provider, run_label, f"InferenceProvider/{name}")
    generation = provider.get("metadata", {}).get("generation")
    if provider.get("status", {}).get("observedGeneration") != generation or provider.get("status", {}).get("phase") != "Available":
        raise RuntimeError(f"InferenceProvider/{name} is not current-generation Available")
    if provider.get("spec", {}).get("clusterName") != name:
        raise RuntimeError(f"InferenceProvider/{name} clusterName does not equal its candidate cluster identity")
    return provider


def overlay_candidates(context: str, namespace: str, network_name: str, gateway_name: str, model: str, providers: set[str]) -> tuple[dict, dict[str, dict]]:
    network = get_json(context, "get", "gridnetwork", network_name)
    status_entries = [
        entry
        for entry in network.get("status", {}).get("routingMapStatus", [])
        if entry.get("gatewayName") == gateway_name and entry.get("namespace") == namespace
    ]
    generation = network.get("metadata", {}).get("generation")
    if len(status_entries) != 1:
        raise RuntimeError("GridNetwork has no unique routing map for the task Gateway")
    status = status_entries[0]
    if (
        status.get("phase") != "Distributed"
        or status.get("observedGeneration") != generation
        or not status.get("renderedRevision")
        or status.get("renderedRevision") != status.get("distributedRevision")
        or not status.get("contentDigest")
    ):
        raise RuntimeError("Grid routing map is not current and Distributed")
    config_map = get_json(context, "-n", status.get("namespace", namespace), "get", "configmap", status["configMapName"])
    if config_map.get("metadata", {}).get("resourceVersion") != status.get("configMapResourceVersion"):
        raise RuntimeError("Grid overlay ConfigMap resourceVersion differs from current status")
    if config_map.get("metadata", {}).get("annotations", {}).get("grid.praxis.fast/overlay-revision") != status.get("distributedRevision"):
        raise RuntimeError("Grid overlay annotation revision differs from current status")
    envelope = json.loads((config_map.get("data") or {}).get("routing-overlay.json", "null"))
    if (
        envelope.get("revision", {}).get("value") != status.get("distributedRevision")
        or envelope.get("content_digest", {}).get("value") != status.get("contentDigest")
    ):
        raise RuntimeError("Grid overlay envelope revision or digest differs from current status")
    candidates = [
        item
        for item in envelope.get("overlay", {}).get("candidates", [])
        if item.get("kind") == "inference_model" and item.get("name") == model and item.get("cluster") in providers
    ]
    if len(candidates) != 2 or {item.get("cluster") for item in candidates} != providers:
        raise RuntimeError("current overlay does not contain exactly both task provider/model candidates")
    mapped = {item["cluster"]: item for item in candidates}
    if any(not isinstance(item.get("stable_id"), str) or not item["stable_id"] for item in candidates):
        raise RuntimeError("current overlay candidate is missing the stable_id required by provider_route")
    return status, mapped


def gateway_backend(context: str, namespace: str, gateway_name: str, pool: str) -> str:
    services = get_json(
        context,
        "-n",
        namespace,
        "get",
        "services",
        "-l",
        f"gateway.networking.k8s.io/gateway-name={gateway_name}",
    ).get("items", [])
    matches = [service for service in services if any(port.get("port") == 8080 for port in service.get("spec", {}).get("ports", []))]
    if len(matches) != 1:
        raise RuntimeError(f"pool {pool} Gateway has no unique generated Service on port 8080")
    service = matches[0]
    port = next(item["port"] for item in service["spec"]["ports"] if item.get("port") == 8080)
    return f"{service['metadata']['name']}.{namespace}.svc.cluster.local:{port}"


def make_provider_config(
    site_name: str,
    provider: str,
    candidate_id: str,
    model: str,
    request_path: str,
    pool: str,
    backend: str,
    credential_namespace: str,
    credential_name: str,
) -> str:
    config = {
        "insecure_options": {"allow_private_endpoints": True, "allow_private_upstreams": True},
        "listeners": [
            {
                "name": "provider",
                "address": "0.0.0.0:8443",
                "filter_chains": ["provider-inference"],
                "tls": {
                    "certificates": [{"cert_path": "/etc/praxis/tls/tls.crt", "key_path": "/etc/praxis/tls/tls.key"}],
                    "client_ca": {"ca_path": "/etc/praxis/ca-source/ca.crt"},
                    "client_cert_mode": "require",
                },
            }
        ],
        "filter_chains": [
            {
                "name": "provider-inference",
                "filters": [
                    {"filter": "peer_identity_trust", "trusted_peers": [{"organization": site_name}]},
                    {"filter": "json_body_field", "field": "model", "header": "X-Model"},
                    {
                        "filter": "provider_route",
                        "provider_id": provider,
                        "model_header": "X-Model",
                        "emit_demo_attribution": True,
                        "routes": [
                            {
                                "candidate_id": candidate_id,
                                "model": model,
                                "paths": [request_path],
                                "cluster": f"{pool}-backend",
                                "credential": {
                                    "strategy": "bearer_token",
                                    "secretRef": {
                                        "name": credential_name,
                                        "namespace": credential_namespace,
                                        "key": "token",
                                    },
                                },
                            }
                        ],
                    },
                    {
                        "filter": "credential_inject",
                        "credentials": [
                            {
                                "name": credential_name,
                                "namespace": credential_namespace,
                                "key": "token",
                                "strategy": "bearer_token",
                                "file": CLIENT_TOKEN_PATH,
                            }
                        ],
                    },
                    {
                        "filter": "load_balancer",
                        "clusters": [{"name": f"{pool}-backend", "endpoints": [backend]}],
                    },
                ],
            }
        ],
        "admin": {"metrics_address": "127.0.0.1:9901"},
        "shutdown_timeout_secs": 5,
    }
    return yaml.safe_dump(config, sort_keys=False)


def resources(
    run_label: str,
    namespace: str,
    pool: str,
    provider: str,
    image: str,
    identity_secret: str,
    ca_secret: str,
    config_text: str,
    client_service_account: str,
) -> list[dict]:
    name = f"praxis-provider-{pool}"
    service_name = name
    labels = {
        "app.kubernetes.io/part-of": "grid-llmd-e2e",
        "app.kubernetes.io/managed-by": "grid-llmd-e2e-run",
        "app.kubernetes.io/name": "praxis-provider-proxy",
        "praxis.fast/qualification-run": run_label,
        "praxis.fast/component": "provider-proxy",
        "praxis.fast/provider-pool": pool,
        "praxis.fast/provider-name": provider,
    }
    cm = {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {"name": name, "namespace": namespace, "labels": labels},
        "data": {"praxis.yaml": config_text},
    }
    deployment = {
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": namespace, "labels": labels},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app.kubernetes.io/name": "praxis-provider-proxy", "praxis.fast/provider-pool": pool}},
            "template": {
                "metadata": {
                    "labels": labels,
                    "annotations": {
                        "praxis.fast/config-sha256": hashlib.sha256(config_text.encode()).hexdigest(),
                    },
                },
                "spec": {
                    "automountServiceAccountToken": False,
                    "serviceAccountName": client_service_account,
                    "containers": [
                        {
                            "name": "praxis-ai",
                            "image": image,
                            "imagePullPolicy": "Always",
                            "command": ["praxis-ai"],
                            "args": ["--config", "/etc/praxis/praxis.yaml"],
                            "ports": [{"name": "https", "containerPort": 8443, "protocol": "TCP"}],
                            "readinessProbe": {"exec": {"command": ["wget", "-qO-", "http://127.0.0.1:9901/healthy"]}, "initialDelaySeconds": 3, "periodSeconds": 5, "timeoutSeconds": 2, "failureThreshold": 12},
                            "livenessProbe": {"exec": {"command": ["wget", "-qO-", "http://127.0.0.1:9901/healthy"]}, "initialDelaySeconds": 10, "periodSeconds": 10, "timeoutSeconds": 2, "failureThreshold": 6},
                            "resources": {"requests": {"cpu": "25m", "memory": "128Mi"}, "limits": {"cpu": "500m", "memory": "512Mi"}},
                            "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}, "seccompProfile": {"type": "RuntimeDefault"}},
                            "volumeMounts": [
                                {"name": "config", "mountPath": "/etc/praxis/praxis.yaml", "subPath": "praxis.yaml", "readOnly": True},
                                {"name": "identity", "mountPath": "/etc/praxis/tls", "readOnly": True},
                                {"name": "ca", "mountPath": "/etc/praxis/ca-source", "readOnly": True},
                                {"name": "provider-token", "mountPath": CLIENT_TOKEN_DIRECTORY, "readOnly": True},
                            ],
                        }
                    ],
                    "volumes": [
                        {"name": "config", "configMap": {"name": name, "defaultMode": 292}},
                        {"name": "identity", "secret": {"secretName": identity_secret, "defaultMode": 292, "items": [{"key": "tls.crt", "path": "tls.crt"}, {"key": "tls.key", "path": "tls.key"}]}},
                        {"name": "ca", "secret": {"secretName": ca_secret, "defaultMode": 292, "items": [{"key": "ca.crt", "path": "ca.crt"}]}},
                        {
                            "name": "provider-token",
                            "projected": {
                                "defaultMode": 420,
                                "sources": [
                                    {
                                        "serviceAccountToken": {
                                            "audience": CLIENT_TOKEN_AUDIENCE,
                                            "expirationSeconds": 3600,
                                            "path": "token",
                                        }
                                    }
                                ],
                            },
                        },
                    ],
                },
            },
        },
    }
    service = {
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": service_name, "namespace": namespace, "labels": labels},
        "spec": {
            "type": "ClusterIP",
            "selector": {"app.kubernetes.io/name": "praxis-provider-proxy", "praxis.fast/provider-pool": pool},
            "ports": [{"name": "https", "port": 8443, "targetPort": "https", "protocol": "TCP"}],
        },
    }
    return [cm, deployment, service]


def check_live_deployment(
    context: str,
    namespace: str,
    pool: str,
    provider: str,
    run_label: str,
    image: str,
    config_text: str,
    client_service_account: str,
    evidence: Path,
) -> dict:
    name = f"praxis-provider-{pool}"
    deployment = get_json(context, "-n", namespace, "get", "deployment", name)
    require_owned(deployment, run_label, f"Deployment/{name}")
    generation = deployment.get("metadata", {}).get("generation")
    if deployment.get("status", {}).get("observedGeneration") != generation or deployment.get("status", {}).get("updatedReplicas") != 1 or deployment.get("status", {}).get("availableReplicas") != 1:
        raise RuntimeError(f"provider proxy Deployment/{name} has not converged to one available replica")
    pod_spec = deployment.get("spec", {}).get("template", {}).get("spec", {})
    projected_token = [
        source.get("serviceAccountToken", {})
        for volume in pod_spec.get("volumes", [])
        if volume.get("name") == "provider-token"
        for source in volume.get("projected", {}).get("sources", [])
    ]
    token_mounts = [
        mount
        for container in pod_spec.get("containers", [])
        for mount in container.get("volumeMounts", [])
        if mount.get("name") == "provider-token"
    ]
    if (
        pod_spec.get("automountServiceAccountToken") is not False
        or pod_spec.get("serviceAccountName") != client_service_account
        or projected_token != [{"audience": CLIENT_TOKEN_AUDIENCE, "expirationSeconds": 3600, "path": "token"}]
        or len(token_mounts) != 1
        or token_mounts[0].get("mountPath") != CLIENT_TOKEN_DIRECTORY
        or token_mounts[0].get("readOnly") is not True
    ):
        raise RuntimeError(f"provider proxy {pool} does not use the scoped read-only TokenRequest projection")
    pods = get_json(context, "-n", namespace, "get", "pods", "-l", f"app.kubernetes.io/name=praxis-provider-proxy,praxis.fast/provider-pool={pool}").get("items", [])
    active = [pod for pod in pods if not pod.get("metadata", {}).get("deletionTimestamp")]
    if len(active) != 1 or active[0].get("status", {}).get("phase") != "Running":
        raise RuntimeError(f"provider proxy {pool} does not have exactly one active Running Pod")
    statuses = active[0].get("status", {}).get("containerStatuses", [])
    if len(statuses) != 1 or statuses[0].get("name") != "praxis-ai" or statuses[0].get("ready") is not True:
        raise RuntimeError(f"provider proxy {pool} Pod is not fully Ready")
    expected_digest = image.rsplit("@", 1)[1]
    if expected_digest not in statuses[0].get("imageID", ""):
        raise RuntimeError(f"provider proxy {pool} runtime imageID does not match the pinned digest")
    mounted = run_oc(context, "-n", namespace, "exec", active[0]["metadata"]["name"], "-c", "praxis-ai", "--", "cat", "/etc/praxis/praxis.yaml")
    if mounted != config_text:
        raise RuntimeError(f"provider proxy {pool} mounted Praxis config differs from its current ConfigMap")
    service = get_json(context, "-n", namespace, "get", "service", name)
    endpoints = get_json(context, "-n", namespace, "get", "endpointslices", "-l", f"kubernetes.io/service-name={name}").get("items", [])
    ready_addresses = [address for item in endpoints for endpoint in item.get("endpoints", []) if endpoint.get("conditions", {}).get("ready") is True for address in endpoint.get("addresses", [])]
    if len(ready_addresses) != 1 or not any(port.get("port") == 8443 for item in endpoints for port in item.get("ports", [])):
        raise RuntimeError(f"provider proxy {pool} Service has no unique Ready EndpointSlice address on 8443")
    config_path = evidence / f"provider-proxy-{pool}-mounted-praxis.yaml"
    config_path.write_text(mounted, encoding="utf-8")
    return {
        "pool": pool,
        "provider": provider,
        "deployment": name,
        "pod": active[0]["metadata"]["name"],
        "podUID": active[0]["metadata"].get("uid"),
        "image": image,
        "imageID": statuses[0].get("imageID"),
        "service": service.get("metadata", {}).get("name"),
        "readyEndpointCount": len(ready_addresses),
        "mountedConfigSha256": hashlib.sha256(mounted.encode()).hexdigest(),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--context", required=True)
    parser.add_argument("--expected-api-server", required=True)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--operator-namespace", required=True)
    parser.add_argument("--run-label", required=True)
    parser.add_argument("--network", required=True)
    parser.add_argument("--gateway", required=True)
    parser.add_argument("--site", required=True)
    parser.add_argument("--site-server-name", required=True)
    parser.add_argument("--identity-secret", default="grid-site-identity")
    parser.add_argument("--ca-secret", default="grid-ca")
    parser.add_argument("--client-service-account", default=os.environ.get("GRID_CLIENT_SERVICE_ACCOUNT", "grid-llmd-client"))
    parser.add_argument("--client-role", default=os.environ.get("GRID_CLIENT_ROLE_NAME", "grid-llmd-client-inference-read"))
    parser.add_argument("--client-role-binding", default=os.environ.get("GRID_CLIENT_ROLE_BINDING_NAME", "grid-llmd-client-inference-read"))
    parser.add_argument("--provider-prefix", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--model-namespace", required=True)
    parser.add_argument("--pool-a-gateway", required=True)
    parser.add_argument("--pool-b-gateway", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--apply", action="store_true")
    parser.add_argument("--evidence-dir", required=True)
    args = parser.parse_args()

    if not re.fullmatch(r".+@sha256:[0-9a-f]{64}", args.image):
        raise RuntimeError("provider proxy image must be pinned to an immutable SHA-256 digest")
    evidence = Path(args.evidence_dir)
    evidence.mkdir(parents=True, exist_ok=True)
    os.chmod(evidence, 0o700)
    server = run_oc(args.context, "whoami", "--show-server").strip()
    ready = run_oc(args.context, "get", "--raw=/readyz").strip()
    if server != args.expected_api_server or ready != "ok":
        raise RuntimeError("explicit API identity or /readyz check failed")
    namespace = get_json(args.context, "get", "namespace", args.namespace)
    require_owned(namespace, args.run_label, f"Namespace/{args.namespace}")
    if not re.fullmatch(r"[a-z0-9]([-a-z0-9]*[a-z0-9])?", args.client_service_account):
        raise RuntimeError("client ServiceAccount name is not DNS-safe")
    client_identity = get_json(args.context, "-n", args.namespace, "get", "serviceaccount", args.client_service_account)
    require_owned(client_identity, args.run_label, f"ServiceAccount/{args.client_service_account}")
    if client_identity.get("automountServiceAccountToken") is not False:
        raise RuntimeError("client ServiceAccount must keep automount disabled; only explicit token projection is allowed")
    role_binding = get_json(args.context, "-n", args.namespace, "get", "rolebinding", args.client_role_binding)
    require_owned(role_binding, args.run_label, f"RoleBinding/{args.client_role_binding}")
    client_role = get_json(args.context, "-n", args.namespace, "get", "role", args.client_role)
    require_owned(client_role, args.run_label, f"Role/{args.client_role}")
    subjects = role_binding.get("subjects", [])
    if (
        role_binding.get("roleRef", {}).get("kind") != "Role"
        or role_binding.get("roleRef", {}).get("name") != args.client_role
        or subjects != [{"kind": "ServiceAccount", "name": args.client_service_account, "namespace": args.namespace}]
    ):
        raise RuntimeError("client ServiceAccount does not have the expected task-scoped RoleBinding")
    task_secret_meta = []
    for secret_name, required_keys in ((args.identity_secret, {"tls.crt", "tls.key"}), (args.ca_secret, {"ca.crt"})):
        secret = get_json(args.context, "-n", args.namespace, "get", "secret", secret_name)
        source_secret = get_json(args.context, "-n", args.operator_namespace, "get", "secret", secret_name)
        keys = set((secret.get("data") or {}).keys())
        source_data = source_secret.get("data") or {}
        task_data = secret.get("data") or {}
        if not required_keys.issubset(keys) or any(task_data.get(key) != source_data.get(key) for key in required_keys):
            raise RuntimeError(f"task TLS Secret {secret_name} is missing required keys")
        task_secret_meta.append({"name": secret_name, "namespace": args.namespace, "type": secret.get("type"), "keys": sorted(keys), "valuesRecorded": False})
    (evidence / "provider-proxy-tls-secret-metadata.json").write_text(json.dumps(task_secret_meta, indent=2) + "\n", encoding="utf-8")

    # Inspect only the public leaf certificate to confirm the client identity's organization.
    identity = get_json(args.context, "-n", args.namespace, "get", "secret", args.identity_secret)
    cert_bytes = base64.b64decode(identity["data"]["tls.crt"])
    with tempfile.TemporaryDirectory(prefix="provider-proxy-cert-") as temporary:
        cert_file = Path(temporary) / "tls.crt"
        cert_file.write_bytes(cert_bytes)
        subject = subprocess.run(["openssl", "x509", "-in", str(cert_file), "-noout", "-subject", "-nameopt", "RFC2253"], capture_output=True, text=True, check=True).stdout.strip()
    if not re.search(rf"(?:^|,)O={re.escape(args.site)}(?:,|$)", subject.removeprefix("subject=")):
        raise RuntimeError("current GridSite certificate organization does not match provider peer_identity_trust")
    (evidence / "provider-proxy-peer-certificate-subject.txt").write_text(subject + "\n", encoding="utf-8")

    providers = {f"{args.provider_prefix}-pool-a", f"{args.provider_prefix}-pool-b"}
    provider_objects = {name: current_provider(args.context, name, args.run_label) for name in providers}
    overlay_status, candidates = overlay_candidates(args.context, args.namespace, args.network, args.gateway, args.model, providers)
    if any(candidate.get("site") != args.site for candidate in candidates.values()):
        raise RuntimeError("current overlay candidate site does not match the task GridSite")
    backends = {
        "pool-a": gateway_backend(args.context, args.namespace, args.pool_a_gateway, "pool-a"),
        "pool-b": gateway_backend(args.context, args.namespace, args.pool_b_gateway, "pool-b"),
    }
    request_path = f"/publishers/{args.model_namespace}/models/{args.model}/v1/chat/completions"
    provider_to_pool = {f"{args.provider_prefix}-pool-a": "pool-a", f"{args.provider_prefix}-pool-b": "pool-b"}
    summaries = []
    for provider, pool in provider_to_pool.items():
        candidate = candidates[provider]
        config_text = make_provider_config(
            args.site,
            provider,
            candidate["stable_id"],
            args.model,
            request_path,
            pool,
            backends[pool],
            args.namespace,
            args.client_service_account,
        )
        manifest = resources(
            args.run_label,
            args.namespace,
            pool,
            provider,
            args.image,
            args.identity_secret,
            args.ca_secret,
            config_text,
            args.client_service_account,
        )
        manifest_path = evidence / f"provider-proxy-{pool}.yaml"
        manifest_path.write_text("---\n".join(yaml.safe_dump(item, sort_keys=False) for item in manifest), encoding="utf-8")
        os.chmod(manifest_path, 0o600)
        run_oc(args.context, "apply", "--dry-run=server", "-f", str(manifest_path))
        if args.apply:
            for kind, name in (("configmap", f"praxis-provider-{pool}"), ("deployment", f"praxis-provider-{pool}"), ("service", f"praxis-provider-{pool}")):
                try:
                    existing = get_json(args.context, "-n", args.namespace, "get", kind, name)
                except RuntimeError as error:
                    if "NotFound" not in str(error):
                        raise
                else:
                    require_owned(existing, args.run_label, f"{kind}/{name}")
            run_oc(args.context, "apply", "-f", str(manifest_path))
            run_oc(args.context, "-n", args.namespace, "rollout", "status", "deployment", f"praxis-provider-{pool}", "--timeout=300s")
            live = check_live_deployment(
                args.context,
                args.namespace,
                pool,
                provider,
                args.run_label,
                args.image,
                config_text,
                args.client_service_account,
                evidence,
            )
        else:
            live = {"pool": pool, "provider": provider, "deployment": f"praxis-provider-{pool}", "apply": False}
        summaries.append(
            {
                **live,
                "candidateStableId": candidate["stable_id"],
                "site": candidate["site"],
                "backendEndpoint": backends[pool],
                "requestPath": request_path,
                "providerResponseHeader": "x-ai-demo-provider-gateway",
                "backendResponseHeader": "x-ai-inference-provider",
                "backendAuth": "projected task ServiceAccount TokenRequest; token bytes not recorded",
            }
        )
    report = {
        "status": "PASS" if args.apply else "RENDERED",
        "apiServer": server,
        "readyz": ready,
        "gridNetwork": args.network,
        "gridGeneration": overlay_status.get("observedGeneration"),
        "overlayRevision": overlay_status.get("distributedRevision"),
        "overlayContentDigest": overlay_status.get("contentDigest"),
        "overlayConfigMapResourceVersion": overlay_status.get("configMapResourceVersion"),
        "providerProxies": summaries,
        "tlsSecretValuesRecorded": False,
        "inferenceTrafficSent": False,
    }
    (evidence / "RESULT.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:
        print(json.dumps({"status": "BLOCKED", "detail": str(error), "inferenceTrafficSent": False}, sort_keys=True), file=sys.stderr)
        sys.exit(1)
