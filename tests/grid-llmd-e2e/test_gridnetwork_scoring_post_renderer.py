#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Regression coverage for the GridNetwork Helm post-render transform."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path


SCRIPT = Path(__file__).with_name("gridnetwork-scoring-post-renderer.py")
ENV = {
    **os.environ,
    "GRID_NETWORK_NAME": "grid-llmd-e2e",
    "GRID_RUN_LABEL": "test-run-20261025",
}
NETWORK = """apiVersion: grid.praxis.fast/v1alpha1
kind: GridNetwork
metadata:
  name: grid-llmd-e2e
  labels:
    praxis.fast/qualification-run: test-run-20261025
spec:
  gridId: smoke
  consumerGateways: []
"""
OTHER_NETWORK = """apiVersion: grid.praxis.fast/v1alpha1
kind: GridNetwork
metadata:
  name: shared-network
  labels:
    praxis.fast/qualification-run: another-run
spec:
  gridId: shared
"""
SERVICE = """apiVersion: v1
kind: Service
metadata:
  name: untouched
spec:
  type: ClusterIP
"""
NETWORK_WITH_GATEWAY = NETWORK.replace(
    "  consumerGateways: []",
    """  consumerGateways:
    - name: task-gateway
      namespace: task-ns
      praxisConfig:
        generate: true
        siteRouter: intelligentRoute
        clusterEndpoints:
        - cluster: provider-a
          address: proxy-a.task-ns.svc:8443
          transport:
            mode: mutual_tls
            sni: site.task.internal
        - cluster: provider-b
          address: proxy-b.task-ns.svc:8443
          transport:
            mode: mutual_tls
            sni: site.task.internal""",
)
ENDPOINTS = (
    '[{"cluster":"provider-a","address":"proxy-a.task-ns.svc:8443",'
    '"transport":{"mode":"mutual_tls","sni":"site.task.internal"}},'
    '{"cluster":"provider-b","address":"proxy-b.task-ns.svc:8443",'
    '"transport":{"mode":"mutual_tls","sni":"site.task.internal"}}]'
)
HOPS = (
    '[{"cluster":"provider-a","transport":{"mode":"mutual_tls","sni":"site.task.internal"}},'
    '{"cluster":"provider-b","transport":{"mode":"mutual_tls","sni":"site.task.internal"}}]'
)


def run(source: str, extra: dict[str, str] | None = None) -> subprocess.CompletedProcess[str]:
    env = {**ENV, **(extra or {})}
    return subprocess.run(
        [sys.executable, str(SCRIPT)],
        input=source,
        text=True,
        capture_output=True,
        env=env,
        check=False,
    )


first = run(SERVICE + "---\n" + NETWORK + "---\n" + OTHER_NETWORK)
if first.returncode != 0:
    raise SystemExit(f"post-renderer failed on a task-owned GridNetwork: {first.stderr}")
for required in (
    "routingPolicy: scoreFirst",
    "strategy: queueDepth",
    "mode: deterministic",
    'metricsRefreshInterval: "10s"',
):
    if required not in first.stdout:
        raise SystemExit(f"post-renderer output omitted {required!r}")
if first.stdout.count("routingPolicy: scoreFirst") != 1:
    raise SystemExit("post-renderer changed a non-task GridNetwork")
if SERVICE not in first.stdout or OTHER_NETWORK not in first.stdout:
    raise SystemExit("post-renderer changed an unrelated resource document")

second = run(first.stdout)
if second.returncode != 0 or second.stdout != first.stdout:
    raise SystemExit("post-renderer is not idempotent on its rendered output")

for label, source in (
    ("missing task network", SERVICE + "---\n" + OTHER_NETWORK),
    ("wrong run ownership", NETWORK.replace("test-run-20261025", "different-run")),
    ("conflicting policy", NETWORK.replace("  gridId: smoke", "  routingPolicy: geographyFirst\n  gridId: smoke")),
    ("duplicate target", NETWORK + "---\n" + NETWORK),
):
    result = run(source)
    if result.returncode == 0 or result.stdout:
        raise SystemExit(f"post-renderer accepted {label} or emitted partial output")

print("GridNetwork scoring post-renderer ownership, policy, preservation, and idempotence tests passed")

hop_env = {
    "GRID_PROVIDER_HOP_ENDPOINTS_JSON": HOPS,
    "GRID_PRAXIS_CLUSTER_ENDPOINTS_JSON": ENDPOINTS,
}
with_hops = run(NETWORK_WITH_GATEWAY, hop_env)
if with_hops.returncode != 0:
    raise SystemExit(f"post-renderer failed to add provider-hop allowlist: {with_hops.stderr}")
if "providerHopEndpoints: [{\"cluster\":\"provider-a\"" not in with_hops.stdout:
    raise SystemExit("post-renderer omitted explicit provider-hop endpoint allowlist")
idempotent_hops = run(with_hops.stdout, hop_env)
if idempotent_hops.returncode != 0 or idempotent_hops.stdout != with_hops.stdout:
    raise SystemExit("post-renderer is not idempotent with the provider-hop allowlist")
bad_hops = run(NETWORK_WITH_GATEWAY, {**hop_env, "GRID_PROVIDER_HOP_ENDPOINTS_JSON": HOPS.replace("site.task.internal", "wrong.task.internal")})
if bad_hops.returncode == 0 or bad_hops.stdout:
    raise SystemExit("post-renderer accepted an SNI-mismatched provider-hop allowlist")

print("GridNetwork provider-hop allowlist matching and mismatch-rejection tests passed")

NETWORK_WITH_TLS = NETWORK.replace(
    "  consumerGateways: []",
    """  consumerGateways: []
  tls:
    siteSecretRef:
      name: site-identity
      namespace: operator-ns
    caSecretRef:
      name: grid-ca
      namespace: operator-ns""",
)
tls_scoped = run(NETWORK_WITH_TLS, {"GRID_TLS_SECRET_NAMESPACE": "task-ns"})
if tls_scoped.returncode != 0:
    raise SystemExit(f"post-renderer failed to scope TLS Secrets to the Gateway namespace: {tls_scoped.stderr}")
if tls_scoped.stdout.count('namespace: "task-ns"') != 2 or "operator-ns" in tls_scoped.stdout:
    raise SystemExit("post-renderer did not rewrite both task GridNetwork TLS Secret references")
tls_idempotent = run(tls_scoped.stdout, {"GRID_TLS_SECRET_NAMESPACE": "task-ns"})
if tls_idempotent.returncode != 0 or tls_idempotent.stdout != tls_scoped.stdout:
    raise SystemExit("post-renderer is not idempotent when scoping TLS Secret references")
bad_tls_namespace = run(NETWORK_WITH_TLS, {"GRID_TLS_SECRET_NAMESPACE": "task/ns"})
if bad_tls_namespace.returncode == 0 or bad_tls_namespace.stdout:
    raise SystemExit("post-renderer accepted an invalid TLS Secret namespace")

print("GridNetwork TLS Secret namespace scoping and validation tests passed")
