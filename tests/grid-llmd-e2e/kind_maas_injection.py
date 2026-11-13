#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Build UID-guarded patches for the temporary Kind MaaS cleanup injection mode."""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any


class ContractError(ValueError):
    """Raised when namespace identity or injection state changed unexpectedly."""


def _labels(namespace: dict[str, Any]) -> dict[str, str]:
    metadata = namespace.get("metadata")
    if not isinstance(metadata, dict) or not isinstance(metadata.get("name"), str):
        raise ContractError("Namespace metadata.name is missing")
    if not isinstance(metadata.get("uid"), str) or not metadata["uid"]:
        raise ContractError("Namespace metadata.uid is missing")
    labels = metadata.get("labels", {})
    if not isinstance(labels, dict):
        raise ContractError("Namespace metadata.labels is not an object")
    if any(not isinstance(key, str) or not isinstance(value, str) for key, value in labels.items()):
        raise ContractError("Namespace labels must contain string keys and values")
    return labels


def _label_state(labels: dict[str, str], key: str) -> tuple[bool, str | None]:
    return key in labels, labels.get(key)


def snapshot(namespace: dict[str, Any]) -> dict[str, Any]:
    labels = _labels(namespace)
    revision_present, revision = _label_state(labels, "istio.io/rev")
    ambient_present, ambient = _label_state(labels, "istio.io/dataplane-mode")
    if revision_present:
        raise ContractError("maas-system uses revision-based Istio injection; refusing to change its injection mode")
    if ambient_present and ambient == "ambient":
        raise ContractError("maas-system uses ambient Istio dataplane; refusing to change its injection mode")

    injection_present, injection = _label_state(labels, "istio-injection")
    if injection_present and injection not in {"enabled", "disabled"}:
        raise ContractError(f"unsupported istio-injection label value: {injection!r}")

    metadata = namespace["metadata"]
    return {
        "namespace": metadata["name"],
        "uid": metadata["uid"],
        "originalInjectionLabelPresent": injection_present,
        "originalInjectionLabel": injection,
        "revisionLabelPresent": revision_present,
        "revisionLabel": revision,
        "dataplaneModeLabelPresent": ambient_present,
        "dataplaneModeLabel": ambient,
        "changed": injection_present and injection == "enabled",
    }


def _verify_control_labels(state: dict[str, Any], labels: dict[str, str]) -> None:
    for key, present_key, value_key in (
        ("istio.io/rev", "revisionLabelPresent", "revisionLabel"),
        ("istio.io/dataplane-mode", "dataplaneModeLabelPresent", "dataplaneModeLabel"),
    ):
        present, value = _label_state(labels, key)
        if present != state[present_key] or value != state[value_key]:
            raise ContractError(f"Namespace label {key} changed after cleanup preparation")


def prepare_patch(state: dict[str, Any]) -> list[dict[str, Any]]:
    if not state["changed"]:
        return []
    return [
        {"op": "test", "path": "/metadata/uid", "value": state["uid"]},
        {"op": "test", "path": "/metadata/labels/istio-injection", "value": "enabled"},
        {"op": "replace", "path": "/metadata/labels/istio-injection", "value": "disabled"},
    ]


def restore_patch(state: dict[str, Any], namespace: dict[str, Any]) -> list[dict[str, Any]]:
    labels = _labels(namespace)
    metadata = namespace["metadata"]
    if metadata["name"] != state["namespace"] or metadata["uid"] != state["uid"]:
        raise ContractError("maas-system Namespace identity changed before restore")
    _verify_control_labels(state, labels)

    present, value = _label_state(labels, "istio-injection")
    if not state["changed"]:
        if present != state["originalInjectionLabelPresent"] or value != state["originalInjectionLabel"]:
            raise ContractError("istio-injection changed although this run did not modify it")
        return []

    if present and value == state["originalInjectionLabel"]:
        return []
    if not present or value != "disabled":
        raise ContractError("istio-injection changed unexpectedly before restore")

    return [
        {"op": "test", "path": "/metadata/uid", "value": state["uid"]},
        {"op": "test", "path": "/metadata/labels/istio-injection", "value": "disabled"},
        {"op": "replace", "path": "/metadata/labels/istio-injection", "value": state["originalInjectionLabel"]},
    ]


def _read_json(path: str | None) -> dict[str, Any]:
    value = json.load(sys.stdin if path is None else open(path, encoding="utf-8"))
    if not isinstance(value, dict):
        raise ContractError("expected a JSON object")
    return value


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("snapshot", "prepare-patch", "restore-patch"))
    parser.add_argument("--state")
    parser.add_argument("--current")
    args = parser.parse_args()
    try:
        if args.action == "snapshot":
            result = snapshot(_read_json(None))
        elif args.action == "prepare-patch":
            if not args.state:
                raise ContractError("--state is required")
            result = prepare_patch(_read_json(args.state))
        else:
            if not args.state or not args.current:
                raise ContractError("--state and --current are required")
            result = restore_patch(_read_json(args.state), _read_json(args.current))
    except (ContractError, OSError, json.JSONDecodeError) as error:
        print(f"Kind MaaS injection contract failed: {error}", file=sys.stderr)
        return 1
    json.dump(result, sys.stdout, sort_keys=True)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
