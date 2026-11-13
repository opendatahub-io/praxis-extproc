#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Validate ownership boundaries in a rendered Praxis Gateway chart."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import yaml


class RenderContractError(ValueError):
    """Raised when rendered chart resources violate the ownership contract."""


def validate_resources(resources: list[dict], release: str, service: str, run_label: str) -> dict:
    forbidden = [
        f"{item.get('kind')}/{item.get('metadata', {}).get('name', '<unnamed>')}"
        for item in resources
        if item.get("kind") in {"Gateway", "HTTPRoute"}
    ]
    if forbidden:
        raise RenderContractError("Gateway API resources belong to the task Gateway helper: " + ", ".join(forbidden))

    secrets = [item for item in resources if item.get("kind") == "Secret"]
    expected_secret = f"{service}-metrics-token"
    if len(secrets) != 1 or secrets[0].get("metadata", {}).get("name") != expected_secret:
        names = [item.get("metadata", {}).get("name", "<unnamed>") for item in secrets]
        raise RenderContractError(
            f"expected exactly chart-owned {expected_secret} ServiceAccount token Secret, found {names}"
        )

    secret = secrets[0]
    metadata = secret.get("metadata", {})
    labels = metadata.get("labels", {})
    annotations = metadata.get("annotations", {})
    expected_sa = f"{service}-metrics-scraper"
    if secret.get("type") != "kubernetes.io/service-account-token":
        raise RenderContractError("metrics token Secret has an unexpected type")
    if metadata.get("namespace"):
        raise RenderContractError("rendered token Secret must use the Helm release namespace")
    if labels.get("app.kubernetes.io/managed-by") != "Helm":
        raise RenderContractError("metrics token Secret is not owned by Helm")
    if labels.get("app.kubernetes.io/instance") != release:
        raise RenderContractError("metrics token Secret is not owned by the expected Helm release")
    if labels.get("praxis.fast/qualification-run") != run_label:
        raise RenderContractError("metrics token Secret is missing the current run ownership label")
    if annotations.get("kubernetes.io/service-account.name") != expected_sa:
        raise RenderContractError("metrics token Secret references an unexpected ServiceAccount")
    if secret.get("data") or secret.get("stringData"):
        raise RenderContractError("rendered metrics token Secret must not contain credential bytes")

    accounts = [
        item
        for item in resources
        if item.get("kind") == "ServiceAccount" and item.get("metadata", {}).get("name") == expected_sa
    ]
    if len(accounts) != 1 or accounts[0].get("automountServiceAccountToken") is not False:
        raise RenderContractError("metrics scraper ServiceAccount must be unique with token automount disabled")

    return {
        "status": "PASS",
        "release": release,
        "service": service,
        "runLabel": run_label,
        "forbiddenGatewayApiResources": [],
        "chartOwnedSecret": {"name": expected_secret, "type": secret["type"], "credentialBytesPresent": False},
        "metricsScraperServiceAccount": {"name": expected_sa, "automountServiceAccountToken": False},
        "resourceCount": len(resources),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, required=True)
    parser.add_argument("--release", required=True)
    parser.add_argument("--service", required=True)
    parser.add_argument("--run-label", required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    args = parser.parse_args()
    try:
        resources = [item for item in yaml.safe_load_all(args.manifest.read_text(encoding="utf-8")) if item]
        summary = validate_resources(resources, args.release, args.service, args.run_label)
        args.evidence.parent.mkdir(parents=True, exist_ok=True)
        args.evidence.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except (OSError, yaml.YAMLError, RenderContractError) as error:
        print(f"Gateway chart render ownership validation failed: {error}", file=sys.stderr)
        return 1
    print("Praxis Gateway chart resource ownership contract passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
