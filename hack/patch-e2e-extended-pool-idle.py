#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Patch live EnvoyFilter/praxis-extproc-clusters upstream idle_timeout for e2e-extended.

A second EnvoyFilter MERGE on ADD-created clusters is ignored by istiod (same
constraint as upstream TLS — see hack/apply-e2e-extended-tls.sh). Pool matrix
cells must patch the baseline clusters EnvoyFilter directly.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

HTTP_OPTIONS_KEY = "envoy.extensions.upstreams.http.v3.HttpProtocolOptions"
HTTP_OPTIONS_TYPE = "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions"
TARGET_CLUSTERS = ("praxis-pre-processing-grpc", "praxis-processing-grpc")


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


def idle_timeout_from_manifest(path: Path) -> str:
    text = path.read_text()
    values = set(re.findall(r"idle_timeout:\s*(\S+)", text))
    if len(values) != 1:
        raise SystemExit(f"{path}: expected exactly one idle_timeout value, found {values!r}")
    return values.pop()


def set_idle_timeout(cluster_value: dict, idle_timeout: str) -> None:
    typed = cluster_value.setdefault("typed_extension_protocol_options", {})
    http = typed.setdefault(HTTP_OPTIONS_KEY, {})
    http["@type"] = HTTP_OPTIONS_TYPE
    http.setdefault("explicit_http_config", {"http2_protocol_options": {}})
    http["common_http_protocol_options"] = {"idle_timeout": idle_timeout}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--context", default="kind-praxis-e2e")
    parser.add_argument("--namespace", default="istio-system")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--idle-timeout", help='Envoy duration string, e.g. "300s"')
    group.add_argument(
        "--manifest",
        type=Path,
        help="pool-configs/*.yaml; reads the unique idle_timeout: value",
    )
    args = parser.parse_args()

    idle_timeout = args.idle_timeout or idle_timeout_from_manifest(args.manifest)

    ef = kubectl_json(
        args.context, "-n", args.namespace, "get", "envoyfilter", "praxis-extproc-clusters"
    )
    patches = ef.get("spec", {}).get("configPatches") or []
    touched = 0
    for patch in patches:
        if patch.get("applyTo") != "CLUSTER":
            continue
        value = (patch.get("patch") or {}).get("value") or {}
        if value.get("name") not in TARGET_CLUSTERS:
            continue
        set_idle_timeout(value, idle_timeout)
        touched += 1

    if touched != len(TARGET_CLUSTERS):
        print(
            f"error: expected to patch {len(TARGET_CLUSTERS)} clusters, touched {touched}",
            file=sys.stderr,
        )
        return 1

    # Drop status / managedFields; keep resourceVersion for update validation.
    ef.pop("status", None)
    md = ef.setdefault("metadata", {})
    for key in ("uid", "creationTimestamp", "generation", "managedFields"):
        md.pop(key, None)
    kubectl_apply(args.context, ef)
    print(f"patched praxis-extproc-clusters idle_timeout={idle_timeout} on {touched} clusters")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
