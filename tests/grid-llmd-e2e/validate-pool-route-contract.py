#!/usr/bin/env python3
"""Fail closed unless mounted Praxis config and both pool routes agree on request shape."""

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

import yaml


def kubectl(context, namespace, *args):
    command = ["kubectl", f"--context={context}"]
    if namespace:
        command.append(f"--namespace={namespace}")
    command.extend(args)
    result = subprocess.run(command, check=False, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"kubectl {' '.join(args[:3])} failed: {result.stderr.strip()}")
    return json.loads(result.stdout) if args[-1:] == ["-o"] else result.stdout


def get_json(context, namespace, *args):
    return json.loads(kubectl(context, namespace, *args, "-o", "json"))


def validate_operator_config_map_mount(network, gateway_ref, mounted_name, config_map, applied_revision):
    """Validate the operator's active base/alternate Praxis config slot and revision."""
    praxis = gateway_ref.get("praxisConfig", {})
    base_name = praxis.get("configMapName")
    if not base_name:
        raise RuntimeError("GridNetwork does not declare the operator Praxis ConfigMap name")
    alternate_name = f"grid-consumer-config-{hashlib.sha256(base_name.encode()).hexdigest()[:16]}"
    if mounted_name not in (base_name, alternate_name):
        raise RuntimeError("Gateway /etc/praxis volume does not mount the current operator-generated Praxis ConfigMap")

    metadata = config_map.get("metadata", {})
    labels = metadata.get("labels", {})
    expected_labels = {
        "app.kubernetes.io/managed-by": "grid-operator",
        "grid.praxis.fast/network": network.get("metadata", {}).get("name"),
        "grid.praxis.fast/gateway": gateway_ref.get("name"),
    }
    if metadata.get("name") != mounted_name or any(labels.get(key) != value for key, value in expected_labels.items()):
        raise RuntimeError("mounted Praxis ConfigMap is not owned by the current GridNetwork Gateway reconciliation")

    content = (config_map.get("data") or {}).get("praxis.yaml")
    if not content:
        raise RuntimeError("mounted Gateway ConfigMap has no praxis.yaml key")
    content_revision = hashlib.sha256(content.encode()).hexdigest()
    if praxis.get("mountReconciliation", {}).get("enabled") is True and applied_revision != content_revision:
        raise RuntimeError("mounted operator Praxis ConfigMap digest does not match the current reconciliation appliedRevision")
    return content, content_revision


def model_extraction_matches(config, requested_header, model):
    chains = config.get("filter_chains", [])
    for chain in chains:
        filters = chain.get("filters", [])
        for router_index, router in enumerate(filters):
            router_name = router.get("filter")
            if router_name not in ("intelligent_route", "grid_site_route"):
                continue
            model_header = str(router.get("model_header", "")).lower()
            if not model_header or model_header != requested_header.lower():
                continue
            for extractor in filters[:router_index]:
                name = extractor.get("filter")
                if name == "model_to_header" and str(extractor.get("header", "")).lower() == model_header:
                    return chain.get("name", ""), name, router_name, model_header
                if name == "json_body_field" and extractor.get("field") == "model" and str(
                    extractor.get("header", "")
                ).lower() == model_header:
                    return chain.get("name", ""), name, router_name, model_header
                if name == "json_body":
                    for extracted in extractor.get("request_extract", []):
                        if extracted.get("pointer") == "/model" and str(extracted.get("header", "")).lower() == model_header:
                            return chain.get("name", ""), name, router_name, model_header
    raise RuntimeError(f"mounted Praxis config does not extract body model {model!r} into its Grid router model header")


def path_matches(match, request_path):
    path = match.get("path") or {}
    value = path.get("value", "/")
    kind = path.get("type", "PathPrefix")
    if kind == "Exact":
        return request_path == value
    if kind == "PathPrefix":
        return request_path.startswith(value) if value == "/" else request_path == value or request_path.startswith(value.rstrip("/") + "/")
    return False


def route_match_without_gateway_header(rule, request_path, model_header, model):
    matches = rule.get("matches") or [{}]
    for match in matches:
        if not path_matches(match, request_path):
            continue
        headers_ok = True
        for header in match.get("headers", []):
            name = str(header.get("name", "")).lower()
            value = header.get("value")
            if name == "x-gateway-model-name":
                headers_ok = False
                break
            if name == model_header.lower() and value == model:
                continue
            if name in ("content-type",) and value in ("application/json",):
                continue
            headers_ok = False
            break
        if headers_ok:
            return True
    return False


def current_route(route, gateway_name, gateway_namespace):
    generation = route.get("metadata", {}).get("generation")
    for parent in route.get("status", {}).get("parents", []):
        ref = parent.get("parentRef", {})
        if ref.get("name") != gateway_name or ref.get("namespace", gateway_namespace) != gateway_namespace:
            continue
        conditions = parent.get("conditions", [])
        ready = all(
            any(
                condition.get("type") == condition_type
                and condition.get("status") == "True"
                and condition.get("observedGeneration") == generation
                for condition in conditions
            )
            for condition_type in ("Accepted", "ResolvedRefs")
        )
        if ready:
            return True
    return False


def validate_intelligent_provider_hops(network, gateway_ref):
    """Fail closed unless score-based routing and the explicit mTLS hop allowlist agree."""
    spec = network.get("spec", {})
    praxis = gateway_ref.get("praxisConfig", {})
    if praxis.get("siteRouter") != "intelligentRoute":
        raise RuntimeError("score-based provider attribution requires GridNetwork praxisConfig.siteRouter=intelligentRoute")
    if (spec.get("signalTransport") or {}).get("mode") == "poll":
        raise RuntimeError("Poll signal transport disables the current operator's metric-derived overlay scores")
    endpoints = praxis.get("clusterEndpoints") or []
    clusters = [entry.get("cluster") for entry in endpoints]
    if len(endpoints) != 2 or any(not cluster for cluster in clusters) or len(set(clusters)) != 2:
        raise RuntimeError("intelligentRoute requires exactly two current provider endpoint clusters")
    if any(
        (entry.get("transport") or {}).get("mode") != "mutual_tls"
        or not (entry.get("transport") or {}).get("sni")
        for entry in endpoints
    ):
        raise RuntimeError("provider_route attribution requires verified mutual_tls transport and SNI for both provider endpoints")
    hops = gateway_ref.get("providerHopEndpoints") or []
    endpoint_pairs = {
        (entry["cluster"], entry["transport"]["sni"])
        for entry in endpoints
    }
    hop_pairs = {
        (entry.get("cluster"), (entry.get("transport") or {}).get("sni"))
        for entry in hops
        if (entry.get("transport") or {}).get("mode") == "mutual_tls"
    }
    if len(hops) != 2 or len(hop_pairs) != 2 or hop_pairs != endpoint_pairs:
        raise RuntimeError("providerHopEndpoints must exactly match the two generated Praxis mutual_tls clusters and SNI values")
    return endpoints


def validate_grid_serving_mount(deployment, gateway_namespace, grid_network, gateway_name, container_name):
    """Prove the serving ConfigMap, runtime path, and health probe are on the task Gateway."""
    pod_spec = deployment.get("spec", {}).get("template", {}).get("spec", {})
    container = next((item for item in pod_spec.get("containers", []) if item.get("name") == container_name), None)
    if container is None:
        raise RuntimeError(f"task Gateway Deployment has no {container_name} container")
    serving_path = "/etc/praxis/grid-serving"
    serving_mount = next(
        (item for item in container.get("volumeMounts", []) if item.get("mountPath") == serving_path),
        None,
    )
    if serving_mount is None:
        raise RuntimeError("task Gateway does not mount the operator grid-serving ConfigMap")
    expected_map = f"grid-serving-{grid_network}-{gateway_name}"
    serving_volume = next((item for item in pod_spec.get("volumes", []) if item.get("name") == serving_mount.get("name")), None)
    if ((serving_volume or {}).get("configMap") or {}).get("name") != expected_map:
        raise RuntimeError(f"task Gateway serving mount does not reference {expected_map}")
    serving_env = next((item for item in container.get("env", []) if item.get("name") == "GRID_SERVING_CONFIG"), None)
    if (serving_env or {}).get("value") != f"{serving_path}/serving-config.json":
        raise RuntimeError("task Gateway GRID_SERVING_CONFIG does not point to the mounted serving-config.json")
    probe = container.get("readinessProbe", {})
    probe_text = json.dumps(probe)
    if "/healthy" not in probe_text or "/ready" in probe_text:
        raise RuntimeError("gridSiteRoute Gateway readiness must probe Praxis /healthy, never /ready")
    return expected_map


def validate_gateway_overlay_mount(context, gateway_namespace, deployment, container_name, gateway_pod, overlay_status):
    """Prove the live Gateway Pod consumes the exact current Distributed overlay."""
    pod_spec = deployment.get("spec", {}).get("template", {}).get("spec", {})
    container = next((item for item in pod_spec.get("containers", []) if item.get("name") == container_name), None)
    if container is None:
        raise RuntimeError(f"task Gateway Deployment has no {container_name} container")
    overlay_mount = next(
        (item for item in container.get("volumeMounts", []) if item.get("mountPath") == "/etc/praxis/routing"),
        None,
    )
    if overlay_mount is None:
        raise RuntimeError("task Gateway does not mount the Grid routing overlay at /etc/praxis/routing")
    overlay_volume = next((item for item in pod_spec.get("volumes", []) if item.get("name") == overlay_mount.get("name")), None)
    overlay_config_map_name = ((overlay_volume or {}).get("configMap") or {}).get("name")
    if overlay_config_map_name != overlay_status.get("configMapName"):
        raise RuntimeError("task Gateway overlay mount does not reference the current Distributed ConfigMap")
    overlay_config_map = get_json(context, gateway_namespace, "get", "configmap", overlay_config_map_name)
    expected_overlay = (overlay_config_map.get("data") or {}).get("routing-overlay.json")
    if not isinstance(expected_overlay, str) or not expected_overlay:
        raise RuntimeError("current Distributed ConfigMap has no routing-overlay.json")
    mounted_overlay = kubectl(
        context,
        gateway_namespace,
        "exec",
        gateway_pod,
        "-c",
        container_name,
        "--",
        "cat",
        "/etc/praxis/routing/routing-overlay.json",
    )
    if mounted_overlay != expected_overlay:
        raise RuntimeError("Gateway Pod has not consumed the current Distributed overlay contents")
    envelope = json.loads(mounted_overlay)
    if (
        envelope.get("revision", {}).get("value") != overlay_status.get("distributedRevision")
        or envelope.get("content_digest", {}).get("value") != overlay_status.get("contentDigest")
    ):
        raise RuntimeError("Gateway Pod overlay revision or digest differs from current Distributed status")
    return {
        "configMap": overlay_config_map_name,
        "resourceVersion": overlay_config_map.get("metadata", {}).get("resourceVersion"),
        "revision": envelope["revision"]["value"],
        "contentDigest": envelope["content_digest"]["value"],
        "mountedSha256": hashlib.sha256(mounted_overlay.encode()).hexdigest(),
    }


def validate_serving_candidates(config, expected_clusters):
    candidates = config.get("candidates") or []
    observed = [entry.get("cluster") for entry in candidates if entry.get("kind") == "inference_model"]
    if len(candidates) != 2 or len(observed) != 2 or sorted(observed) != sorted(expected_clusters):
        raise RuntimeError("mounted grid-serving config candidates do not match the two current provider endpoint clusters")
    return sorted(observed)


def validate_grid_site_router_clusters(config, expected_clusters):
    routers = [
        item
        for chain in config.get("filter_chains", [])
        for item in chain.get("filters", [])
        if item.get("filter") == "grid_site_route"
    ]
    observed = (routers[0].get("clusters") if len(routers) == 1 else None) or []
    if len(routers) != 1 or sorted(observed) != sorted(expected_clusters):
        raise RuntimeError("mounted grid_site_route clusters do not match the two current provider endpoint clusters")
    return sorted(observed)


def current_overlay_candidates(context, network_namespace, network, gateway_name, gateway_namespace, model, clusters):
    generation = network.get("metadata", {}).get("generation")
    matches = [
        status
        for status in network.get("status", {}).get("routingMapStatus", [])
        if status.get("gatewayName") == gateway_name and status.get("namespace") == gateway_namespace
    ]
    if len(matches) != 1:
        raise RuntimeError("GridNetwork does not report exactly one routing map for the task Gateway")
    status = matches[0]
    if (
        status.get("phase") != "Distributed"
        or status.get("observedGeneration") != generation
        or not status.get("renderedRevision")
        or status.get("renderedRevision") != status.get("distributedRevision")
        or not status.get("contentDigest")
        or not status.get("configMapName")
        or not status.get("configMapResourceVersion")
    ):
        raise RuntimeError("Grid overlay is not Distributed at the current generation and matching rendered revision")
    config_map = get_json(context, status.get("namespace") or network_namespace, "get", "configmap", status["configMapName"])
    if config_map.get("metadata", {}).get("resourceVersion") != status.get("configMapResourceVersion"):
        raise RuntimeError("Grid routing ConfigMap resourceVersion differs from current routing-map status")
    data = config_map.get("data") or {}
    raw = data.get("routing-overlay.json")
    if not isinstance(raw, str):
        raise RuntimeError("current Grid routing ConfigMap has no routing-overlay.json")
    try:
        envelope = json.loads(raw)
    except json.JSONDecodeError as error:
        raise RuntimeError("current Grid routing overlay is not valid JSON") from error
    if (
        envelope.get("revision", {}).get("value") != status.get("distributedRevision")
        or envelope.get("content_digest", {}).get("value") != status.get("contentDigest")
        or config_map.get("metadata", {}).get("annotations", {}).get("grid.praxis.fast/overlay-revision")
        != status.get("distributedRevision")
    ):
        raise RuntimeError("Grid overlay envelope revision/digest do not match the Distributed status")
    candidates = [
        item
        for item in envelope.get("overlay", {}).get("candidates", [])
        if item.get("kind") == "inference_model" and item.get("name") == model and item.get("cluster") in clusters
    ]
    if len(candidates) != 2 or {item.get("cluster") for item in candidates} != set(clusters):
        raise RuntimeError("current Grid overlay does not contain exactly the two requested model/provider candidates")
    if any(not item.get("stable_id") for item in candidates):
        raise RuntimeError("current Grid overlay candidate is missing its stable ID required by provider_route")
    return status, candidates


def validate_provider_proxy_configs(
    context, namespace, run_label, site_name, model, request_path, overlay_candidates, expected_backends
):
    selector = f"praxis.fast/qualification-run={run_label},praxis.fast/component=provider-proxy"
    config_maps = get_json(context, namespace, "get", "configmaps", "-l", selector).get("items", [])
    if len(config_maps) != 2:
        raise RuntimeError("exactly two run-owned provider-proxy ConfigMaps are required")
    candidates_by_cluster = {candidate.get("cluster"): candidate for candidate in overlay_candidates}
    observed_pools = set()
    summaries = []
    for config_map in config_maps:
        labels = config_map.get("metadata", {}).get("labels", {})
        pool = labels.get("praxis.fast/provider-pool", "")
        provider_name = labels.get("praxis.fast/provider-name", "")
        if pool not in expected_backends or pool in observed_pools:
            raise RuntimeError("provider-proxy ConfigMaps do not map one-to-one to the two task pool Gateways")
        observed_pools.add(pool)
        config_text = (config_map.get("data") or {}).get("praxis.yaml")
        if not isinstance(config_text, str) or not config_text:
            raise RuntimeError(f"provider proxy for {pool} has no praxis.yaml")
        proxy_config = yaml.safe_load(config_text)
        chains = proxy_config.get("filter_chains", [])
        provider_chain = next(
            (chain for chain in chains if any(item.get("filter") == "provider_route" for item in chain.get("filters", []))),
            None,
        )
        if provider_chain is None:
            raise RuntimeError(f"provider proxy for {pool} has no top-level provider_route filter")
        filters = provider_chain.get("filters", [])
        if not filters or filters[0].get("filter") != "peer_identity_trust":
            raise RuntimeError(f"provider proxy for {pool} must place peer_identity_trust first")
        trust = filters[0].get("trusted_peers", [])
        if not any(peer.get("organization") == site_name for peer in trust):
            raise RuntimeError(f"provider proxy for {pool} does not trust the current GridSite identity")
        routes = [item for item in filters if item.get("filter") == "provider_route"]
        if len(routes) != 1:
            raise RuntimeError(f"provider proxy for {pool} must have exactly one provider_route")
        route = routes[0]
        if route.get("model_header", "").lower() != "x-model" or route.get("emit_demo_attribution") is not True:
            raise RuntimeError(f"provider proxy for {pool} does not emit the source-defined provider attribution headers")
        if route.get("provider_id") != provider_name:
            raise RuntimeError(f"provider proxy for {pool} attribution identity does not match its task InferenceProvider")
        proxy_routes = route.get("routes", [])
        if len(proxy_routes) != 1:
            raise RuntimeError(f"provider proxy for {pool} must authorize exactly one current overlay candidate")
        proxy_route = proxy_routes[0]
        candidate = candidates_by_cluster.get(provider_name)
        if candidate is None or proxy_route.get("candidate_id") != candidate.get("stable_id"):
            raise RuntimeError(f"provider proxy for {pool} candidate ID does not match the current Distributed overlay")
        if (
            proxy_route.get("model") != model
            or request_path not in proxy_route.get("paths", [])
            or proxy_route.get("cluster") not in (f"{pool}-backend", f"{pool}-gateway")
        ):
            raise RuntimeError(f"provider proxy for {pool} model, canonical path, or backend cluster does not match the pool contract")
        backend_cluster = proxy_route["cluster"]
        load_balancers = [
            cluster
            for item in filters
            if item.get("filter") == "load_balancer"
            for cluster in item.get("clusters", [])
            if cluster.get("name") == backend_cluster
        ]
        if len(load_balancers) != 1:
            raise RuntimeError(f"provider proxy for {pool} does not define exactly one backend load-balancer cluster")
        endpoints = load_balancers[0].get("endpoints", [])
        if endpoints != [expected_backends[pool]]:
            raise RuntimeError(f"provider proxy for {pool} backend endpoint is not the current task pool Gateway Service")
        summaries.append(
            {
                "pool": pool,
                "provider": provider_name,
                "candidateStableId": candidate.get("stable_id"),
                "site": candidate.get("site"),
                "providerResponseHeader": "x-ai-demo-provider-gateway",
                "backendResponseHeader": "x-ai-inference-provider",
                "backendCluster": backend_cluster,
                "backendEndpoint": endpoints[0],
                "peerTrustOrganization": site_name,
            }
        )
    if observed_pools != set(expected_backends):
        raise RuntimeError("provider proxy mappings do not cover both task pool backends")
    return summaries


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--context", required=True)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--grid-network", required=True)
    parser.add_argument("--gateway-name", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--model-header", required=True)
    parser.add_argument("--request-path", required=True)
    parser.add_argument("--evidence-dir", required=True)
    parser.add_argument(
        "--pool-model-resource",
        action="append",
        required=True,
        help="inferencepool-name=namespace/llminferenceservice/name",
    )
    args = parser.parse_args()

    evidence_dir = Path(args.evidence_dir)
    evidence_dir.mkdir(parents=True, exist_ok=True)
    report = {
        "status": "FAIL",
        "requestPath": args.request_path,
        "bodyModel": args.model,
        "clientModelHeader": args.model_header,
        "clientSuppliesGatewayModelHeader": False,
        "pools": [],
    }
    try:
        if args.model_header.lower() == "x-gateway-model-name":
            raise RuntimeError("the client model header must not spoof the pool Gateway's X-Gateway-Model-Name")

        network = get_json(args.context, "", "get", "gridnetwork", args.grid_network)
        gateway_ref = next(
            (
                entry
                for entry in network.get("spec", {}).get("consumerGateways", [])
                if entry.get("name") == args.gateway_name
            ),
            None,
        )
        if gateway_ref is None:
            raise RuntimeError("GridNetwork has no consumerGateway entry for the requested Praxis Gateway")
        gateway_namespace = gateway_ref.get("namespace", args.namespace)
        praxis_config = gateway_ref.get("praxisConfig", {})
        if praxis_config.get("generate") is not True:
            raise RuntimeError("the task Gateway does not use current operator-generated Praxis config")
        mount = praxis_config.get("mountReconciliation", {})
        deployment_name = mount.get("deploymentName")
        container_name = mount.get("containerName", "praxis")
        mount_reconciliation_enabled = mount.get("enabled") is True
        if not deployment_name:
            deployments = get_json(
                args.context,
                gateway_namespace,
                "get",
                "deployments",
                "-l",
                f"app.kubernetes.io/instance={gateway_ref.get('name')}",
            ).get("items", [])
            if len(deployments) != 1:
                raise RuntimeError("direct-mounted task Gateway must resolve to exactly one Deployment by its Helm instance label")
            deployment_name = deployments[0].get("metadata", {}).get("name")
        if not deployment_name:
            raise RuntimeError("task Gateway Deployment name is unavailable")
        serving_endpoints = validate_intelligent_provider_hops(network, gateway_ref)
        endpoint_clusters = sorted(entry["cluster"] for entry in serving_endpoints)

        deployment = get_json(args.context, gateway_namespace, "get", "deployment", deployment_name)
        network_generation = network.get("metadata", {}).get("generation")
        current_config_status = any(
            item.get("gatewayName") == args.gateway_name
            and item.get("namespace") == gateway_namespace
            and item.get("configMapName") == praxis_config.get("configMapName")
            and item.get("phase") == "Rendered"
            and item.get("observedGeneration") == network_generation
            for item in network.get("status", {}).get("praxisConfigStatus", [])
        )
        if not current_config_status:
            raise RuntimeError("operator Praxis ConfigMap is not Rendered at the current GridNetwork generation")
        mount_status = [
            item
            for item in network.get("status", {}).get("mountReconciliationStatus", [])
            if item.get("gatewayName") == args.gateway_name
            and item.get("namespace") == gateway_namespace
            and item.get("observedGeneration") == network_generation
            and not item.get("reason")
        ]
        if mount_reconciliation_enabled:
            if not any(
                item.get("deploymentName") == deployment_name
                and item.get("phase") == "Ready"
                and item.get("deploymentGeneration") == deployment.get("metadata", {}).get("generation")
                and bool(item.get("appliedRevision"))
                for item in mount_status
            ):
                raise RuntimeError("operator Gateway mount reconciliation is not Ready at the current GridNetwork generation")
            operator_mount_mode = "operator-reconciled"
        else:
            if not any(item.get("phase") == "RequirementsRendered" and item.get("requirementsRevision") for item in mount_status):
                raise RuntimeError("operator did not publish current Gateway mount requirements for the directly mounted Praxis config")
            operator_mount_mode = "operator-generated-config-direct-mounted"
        template = deployment.get("spec", {}).get("template", {}).get("spec", {})
        container = next((item for item in template.get("containers", []) if item.get("name") == container_name), None)
        mount_point = next(
            (item for item in (container or {}).get("volumeMounts", []) if item.get("mountPath") == "/etc/praxis"),
            None,
        )
        if mount_point is None:
            raise RuntimeError("Gateway deployment does not mount Praxis config at /etc/praxis")
        volume = next((item for item in template.get("volumes", []) if item.get("name") == mount_point.get("name")), None)
        config_map_name = ((volume or {}).get("configMap") or {}).get("name")
        config_map = get_json(args.context, gateway_namespace, "get", "configmap", config_map_name)
        applied_revision = next(
            (
                item.get("appliedRevision")
                for item in mount_status
                if item.get("deploymentName") == deployment_name
                and item.get("phase") == "Ready"
                and item.get("deploymentGeneration") == deployment.get("metadata", {}).get("generation")
            ),
            None,
        )
        expected_config, config_sha = validate_operator_config_map_mount(
            network, gateway_ref, config_map_name, config_map, applied_revision
        )

        selector = deployment.get("spec", {}).get("selector", {}).get("matchLabels", {})
        selector_arg = ",".join(f"{key}={value}" for key, value in sorted(selector.items()))
        pods = get_json(args.context, gateway_namespace, "get", "pods", "-l", selector_arg)
        ready_pod = next(
            (
                pod
                for pod in pods.get("items", [])
                if pod.get("status", {}).get("phase") == "Running"
                and any(status.get("name") == container_name and status.get("ready") for status in pod.get("status", {}).get("containerStatuses", []))
            ),
            None,
        )
        if ready_pod is None:
            raise RuntimeError("no ready task Gateway Pod is available to inspect its mounted Praxis config")
        mounted_config = kubectl(
            args.context,
            gateway_namespace,
            "exec",
            ready_pod["metadata"]["name"],
            "-c",
            container_name,
            "--",
            "cat",
            "/etc/praxis/praxis.yaml",
        )
        if mounted_config != expected_config:
            raise RuntimeError("Gateway Pod's mounted praxis.yaml differs from its declared ConfigMap")
        config = yaml.safe_load(mounted_config)
        chain_name, extractor, router, route_header = model_extraction_matches(config, args.model_header, args.model)
        if router != "intelligent_route":
            raise RuntimeError(f"mounted Praxis router {router!r} cannot satisfy score-based provider-hop attribution")
        route_clusters = sorted(
            item.get("name")
            for chain in config.get("filter_chains", [])
            for item in chain.get("filters", [])
            if item.get("filter") == "load_balancer"
            for item in item.get("clusters", [])
        )
        expected_hop_clusters = sorted(endpoint_clusters)
        route_filters = [
            item
            for chain in config.get("filter_chains", [])
            for item in chain.get("filters", [])
            if item.get("filter") == "intelligent_route"
        ]
        if len(route_filters) != 1 or sorted(route_filters[0].get("provider_hop_clusters", [])) != expected_hop_clusters:
            raise RuntimeError("mounted intelligent_route provider_hop_clusters do not match the two verified mTLS provider clusters")
        load_balancer_clusters = [
            item
            for chain in config.get("filter_chains", [])
            for filter_item in chain.get("filters", [])
            if filter_item.get("filter") == "load_balancer"
            for item in filter_item.get("clusters", [])
        ]
        lb_by_name = {item.get("name"): item for item in load_balancer_clusters}
        if len(load_balancer_clusters) != 2 or set(lb_by_name) != set(expected_hop_clusters):
            raise RuntimeError("mounted Praxis load_balancer must contain exactly the two current provider-hop clusters")
        for endpoint in serving_endpoints:
            cluster = lb_by_name.get(endpoint.get("cluster"), {})
            transport = cluster.get("tls", {})
            if (
                endpoint.get("address") not in cluster.get("endpoints", [])
                or transport.get("verify") is not True
                or transport.get("sni") != (endpoint.get("transport") or {}).get("sni")
                or not (transport.get("ca") or {}).get("ca_path")
                or not (transport.get("client_cert") or {}).get("cert_path")
                or not (transport.get("client_cert") or {}).get("key_path")
            ):
                raise RuntimeError(f"mounted Praxis load_balancer cluster {endpoint.get('cluster')} does not use the declared verified mTLS endpoint")
        report.update(
            {
                "gatewayNamespace": gateway_namespace,
                "gatewayDeployment": deployment_name,
                "gatewayPod": ready_pod["metadata"]["name"],
                "praxisConfigMap": config_map_name,
                "praxisConfigSha256": config_sha,
                "praxisFilterChain": chain_name,
                "modelExtractor": extractor,
                "gridRouter": router,
                "gridRouterModelHeader": route_header,
                "providerHopClusters": expected_hop_clusters,
                "attributionOracle": ["x-ai-demo-provider-gateway", "x-ai-inference-provider"],
                "signalTransport": (network.get("spec", {}).get("signalTransport") or {}).get("mode"),
                "siteRouter": praxis_config.get("siteRouter"),
                "providerEndpointClusters": sorted(entry["cluster"] for entry in serving_endpoints),
                "gatewayLoadBalancerClusters": route_clusters,
                "operatorMountMode": operator_mount_mode,
            }
        )

        route_list = get_json(args.context, args.namespace, "get", "httproutes.gateway.networking.k8s.io")
        expected_backends = {}
        for mapping in args.pool_model_resource:
            pool_name, resource = mapping.split("=", 1)
            backend_pool_name = pool_name if pool_name.endswith("-inference-pool") else f"{pool_name}-inference-pool"
            pool_label = backend_pool_name.removesuffix("-inference-pool")
            parts = resource.split("/")
            if len(parts) == 2:
                model_namespace, model_name = parts
            elif len(parts) == 3 and parts[1].lower().startswith("llminferenceservice"):
                model_namespace, model_name = parts[0], parts[2]
            elif len(parts) == 3:
                model_namespace, model_name = parts[0], parts[2]
            else:
                raise RuntimeError(f"invalid LLMInferenceService mapping for {pool_name}")
            llmisvc = get_json(args.context, model_namespace, "get", "llminferenceservice", model_name)
            uid = llmisvc.get("metadata", {}).get("uid")
            pool_gateway = next(
                iter(llmisvc.get("spec", {}).get("router", {}).get("gateway", {}).get("refs", [])),
                None,
            )
            if not uid or pool_gateway is None:
                raise RuntimeError(f"LLMInferenceService {model_namespace}/{model_name} has no UID or pool Gateway ref")
            gateway = pool_gateway.get("name")
            pool_gateway_namespace = pool_gateway.get("namespace", model_namespace)
            owned_routes = [
                route
                for route in route_list.get("items", [])
                if any(ref.get("uid") == uid for ref in route.get("metadata", {}).get("ownerReferences", []))
            ]
            candidates = []
            for route in owned_routes:
                if not current_route(route, gateway, pool_gateway_namespace):
                    continue
                for rule in route.get("spec", {}).get("rules", []):
                    backend_ok = any(
                        backend.get("kind") == "InferencePool"
                        and backend.get("name") == backend_pool_name
                        and backend.get("group", "") in ("inference.networking.k8s.io", "inference.networking.x-k8s.io")
                        for backend in rule.get("backendRefs", [])
                    )
                    if not backend_ok or not route_match_without_gateway_header(
                        rule, args.request_path, args.model_header, args.model
                    ):
                        continue
                    rewrite = next(
                        (
                            item.get("urlRewrite", {}).get("path", {}).get("replacePrefixMatch")
                            for item in rule.get("filters", [])
                            if item.get("type") == "URLRewrite"
                        ),
                        None,
                    )
                    if rewrite == "/v1/chat/completions":
                        candidates.append(route)
            expected_path = f"/publishers/{model_namespace}/models/{args.model}/v1/chat/completions"
            if args.request_path != expected_path or not candidates:
                raise RuntimeError(
                    f"pool {pool_name} has no current accepted route for the request path without X-Gateway-Model-Name and the expected rewrite"
                )
            pool_services = get_json(
                args.context,
                pool_gateway_namespace,
                "get",
                "services",
                "-l",
                f"gateway.networking.k8s.io/gateway-name={gateway}",
            ).get("items", [])
            gateway_services = [
                service
                for service in pool_services
                if any(port.get("port") == 8080 for port in service.get("spec", {}).get("ports", []))
            ]
            if len(gateway_services) != 1:
                raise RuntimeError(f"pool {pool_name} does not have exactly one current Gateway Service on port 8080")
            service = gateway_services[0]
            service_port = next(port["port"] for port in service["spec"]["ports"] if port.get("port") == 8080)
            expected_backends[pool_label] = f"{service['metadata']['name']}.{pool_gateway_namespace}.svc.cluster.local:{service_port}"
            report["pools"].append(
                {
                    "pool": pool_name,
                    "poolLabel": pool_label,
                    "llmInferenceService": f"{model_namespace}/{model_name}",
                    "route": candidates[0].get("metadata", {}).get("name"),
                    "routeGeneration": candidates[0].get("metadata", {}).get("generation"),
                    "poolGateway": f"{pool_gateway_namespace}/{gateway}",
                    "requestPathMatchedWithoutGatewayModelHeader": True,
                    "rewriteTarget": "/v1/chat/completions",
                    "backendInferencePool": backend_pool_name,
                    "gatewayServiceEndpoint": expected_backends[pool_label],
                }
            )

        if len(report["pools"]) != 2 or len({entry["pool"] for entry in report["pools"]}) != 2:
            raise RuntimeError("route contract requires exactly two distinct pool routes")
        overlay_status, overlay_candidates = current_overlay_candidates(
            args.context,
            args.namespace,
            network,
            args.gateway_name,
            gateway_namespace,
            args.model,
            endpoint_clusters,
        )
        gateway_overlay = validate_gateway_overlay_mount(
            args.context,
            gateway_namespace,
            deployment,
            container_name,
            ready_pod["metadata"]["name"],
            overlay_status,
        )
        site_name = gateway_ref.get("siteName") or gateway_ref.get("localSiteName") or gateway_ref.get("name")
        provider_proxy_contracts = validate_provider_proxy_configs(
            args.context,
            args.namespace,
            network.get("metadata", {}).get("labels", {}).get("praxis.fast/qualification-run", ""),
            site_name,
            args.model,
            args.request_path,
            overlay_candidates,
            expected_backends,
        )
        report.update(
            {
                "providerProxyContracts": provider_proxy_contracts,
                "overlayRevision": overlay_status.get("distributedRevision"),
                "overlayContentDigest": overlay_status.get("contentDigest"),
                "overlayConfigMapResourceVersion": overlay_status.get("configMapResourceVersion"),
                "gatewayMountedOverlay": gateway_overlay,
            }
        )
        report["status"] = "PASS"
    except Exception as error:  # the hook is a fail-closed pre-traffic gate
        report["detail"] = str(error)
        print(json.dumps(report, sort_keys=True))
        evidence_dir.mkdir(parents=True, exist_ok=True)
        (evidence_dir / "pool-route-contract.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        return 1

    report["detail"] = "mounted intelligentRoute, exact mTLS provider-hop allowlist, provider-owned attribution configs, current overlay candidate mapping, and both current pool HTTPRoutes agree on the canonical publisher path; no client X-Gateway-Model-Name is used"
    (evidence_dir / "pool-route-contract.json").write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    print(json.dumps(report, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
