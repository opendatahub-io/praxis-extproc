#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Patch live EnvoyFilter/praxis-extproc upstream TLS validation for e2e-extended."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path


def kubectl_json(ctx: str, *args: str) -> dict:
    out = subprocess.check_output(["kubectl", "--context", ctx, *args, "-o", "json"], text=True)
    return json.loads(out)


def kubectl_apply(ctx: str, doc: dict) -> None:
    subprocess.run(
        ["kubectl", "--context", ctx, "apply", "-f", "-"],
        input=json.dumps(doc),
        text=True,
        check=True,
    )


def set_validation(
    cluster_value: dict,
    ca_pem: str,
    client_crt: str | None,
    client_key: str | None,
    expected_san: str,
) -> None:
    ts = cluster_value.setdefault("transport_socket", {})
    ts["name"] = "envoy.transport_sockets.tls"
    typed = ts.setdefault("typed_config", {})
    typed["@type"] = "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext"
    typed["sni"] = expected_san
    common = typed.setdefault("common_tls_context", {})
    common["alpn_protocols"] = ["h2"]
    # Explicit SAN match — default VERIFY_TRUST_CHAIN alone does not reject wrong SAN.
    common["validation_context"] = {
        "trust_chain_verification": "VERIFY_TRUST_CHAIN",
        "trusted_ca": {"inline_string": ca_pem if ca_pem.endswith("\n") else ca_pem + "\n"},
        "match_typed_subject_alt_names": [
            {
                "san_type": "DNS",
                "matcher": {"exact": expected_san},
            }
        ],
    }
    if client_crt and client_key:
        common["tls_certificates"] = [
            {
                "certificate_chain": {
                    "inline_string": client_crt if client_crt.endswith("\n") else client_crt + "\n"
                },
                "private_key": {
                    "inline_string": client_key if client_key.endswith("\n") else client_key + "\n"
                },
            }
        ]
    else:
        common.pop("tls_certificates", None)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--context", default="kind-praxis-e2e")
    parser.add_argument("--namespace", default="istio-system")
    parser.add_argument("--ca-file", required=True, type=Path)
    parser.add_argument("--client-crt", type=Path)
    parser.add_argument("--client-key", type=Path)
    args = parser.parse_args()

    ca_pem = args.ca_file.read_text()
    client_crt = args.client_crt.read_text() if args.client_crt else None
    client_key = args.client_key.read_text() if args.client_key else None

    ef = kubectl_json(
        args.context, "-n", args.namespace, "get", "envoyfilter", "praxis-extproc"
    )
    patches = ef.get("spec", {}).get("configPatches") or []
    touched = 0
    san_by_cluster = {
        "praxis-pre-processing-grpc": "payload-pre-processing.istio-system.svc.cluster.local",
        "praxis-processing-grpc": "payload-processing.istio-system.svc.cluster.local",
    }
    for patch in patches:
        if patch.get("applyTo") != "CLUSTER":
            continue
        value = (patch.get("patch") or {}).get("value") or {}
        name = value.get("name", "")
        if name not in san_by_cluster:
            continue
        set_validation(value, ca_pem, client_crt, client_key, san_by_cluster[name])
        touched += 1

    if touched != 2:
        print(f"expected to patch 2 clusters, touched={touched}", file=sys.stderr)
        return 1

    # Drop status/resourceVersion noise that can confuse apply.
    ef.pop("status", None)
    md = ef.setdefault("metadata", {})
    for key in ("resourceVersion", "uid", "creationTimestamp", "generation", "managedFields"):
        md.pop(key, None)

    kubectl_apply(args.context, ef)
    print(f"patched praxis-extproc upstream TLS validation on {touched} clusters")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
