#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render MaaS controller configuration that is supported for this Kind profile."""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Any

import yaml


ROLE_BINDING = "maas-controller-leader-election-rolebinding"
ROLE_NAME = "maas-controller-leader-election-role"
SERVICE_ACCOUNT = "maas-controller"


def render(source: str, namespace: str) -> tuple[str, bool]:
    """Move the exact MaaS leader-election ServiceAccount subject to its rendered namespace."""
    documents = list(yaml.safe_load_all(source))
    matches = [
        document
        for document in documents
        if isinstance(document, dict)
        and document.get("kind") == "RoleBinding"
        and document.get("metadata", {}).get("name") == ROLE_BINDING
    ]
    if len(matches) != 1:
        raise ValueError(f"expected exactly one {ROLE_BINDING}, found {len(matches)}")

    binding: dict[str, Any] = matches[0]
    metadata = binding.get("metadata", {})
    if metadata.get("namespace") != namespace:
        raise ValueError(
            f"{ROLE_BINDING} metadata namespace must be {namespace!r}, "
            f"found {metadata.get('namespace')!r}"
        )
    if binding.get("roleRef") != {
        "apiGroup": "rbac.authorization.k8s.io",
        "kind": "Role",
        "name": ROLE_NAME,
    }:
        raise ValueError(f"{ROLE_BINDING} does not reference the expected namespaced Role")

    subjects = binding.get("subjects")
    if not isinstance(subjects, list) or len(subjects) != 1:
        raise ValueError(f"{ROLE_BINDING} must have exactly one ServiceAccount subject")
    subject = subjects[0]
    if not isinstance(subject, dict) or subject.get("kind") != "ServiceAccount":
        raise ValueError(f"{ROLE_BINDING} subject must be a ServiceAccount")
    if subject.get("name") != SERVICE_ACCOUNT:
        raise ValueError(f"{ROLE_BINDING} subject must be {SERVICE_ACCOUNT}")

    changed = subject.get("namespace") != namespace
    subject["namespace"] = namespace
    output = "\n---\n".join(
        yaml.safe_dump(document, sort_keys=False).rstrip()
        for document in documents
        if document is not None
    )
    return f"{output}\n", changed


def configure_monitoring_namespace(source: str, monitoring_namespace: str) -> tuple[str, bool]:
    """Set the controller's supported monitoring flag, including empty to skip optional ODH observability."""
    documents = list(yaml.safe_load_all(source))
    deployments = [
        document
        for document in documents
        if isinstance(document, dict)
        and document.get("kind") == "Deployment"
        and document.get("metadata", {}).get("name") == "maas-controller"
    ]
    if len(deployments) != 1:
        raise ValueError(f"expected exactly one maas-controller Deployment, found {len(deployments)}")

    spec = deployments[0].get("spec", {}).get("template", {}).get("spec", {})
    containers = spec.get("containers")
    if not isinstance(containers, list):
        raise ValueError("maas-controller Deployment must have a containers list")
    managers = [container for container in containers if isinstance(container, dict) and container.get("name") == "manager"]
    if len(managers) != 1:
        raise ValueError(f"expected exactly one manager container, found {len(managers)}")

    args = managers[0].get("args")
    if not isinstance(args, list):
        raise ValueError("maas-controller manager must have an args list")
    matches = [index for index, arg in enumerate(args) if isinstance(arg, str) and arg.startswith("--monitoring-namespace=")]
    separate = [index for index, arg in enumerate(args) if arg == "--monitoring-namespace"]
    if len(matches) + len(separate) != 1:
        raise ValueError("maas-controller manager must have exactly one --monitoring-namespace argument")

    changed = False
    if matches:
        index = matches[0]
        replacement = f"--monitoring-namespace={monitoring_namespace}"
        changed = args[index] != replacement
        args[index] = replacement
    else:
        index = separate[0]
        if index + 1 >= len(args) or not isinstance(args[index + 1], str):
            raise ValueError("--monitoring-namespace must have a value")
        replacement = f"--monitoring-namespace={monitoring_namespace}"
        changed = args[index] != replacement or args[index + 1] != ""
        args[index:index + 2] = [replacement]

    output = "\n---\n".join(
        yaml.safe_dump(document, sort_keys=False).rstrip()
        for document in documents
        if document is not None
    )
    return f"{output}\n", changed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--namespace", default="maas-system")
    parser.add_argument("--monitoring-namespace", default=None,
                        help="Set the supported MaaS monitoring namespace; an empty value skips optional observability")
    args = parser.parse_args()

    if args.input.resolve() == args.output.resolve():
        parser.error("input and output must be different paths")
    rendered, changed = render(args.input.read_text(encoding="utf-8"), args.namespace)
    monitoring_changed = False
    if args.monitoring_namespace is not None:
        rendered, monitoring_changed = configure_monitoring_namespace(rendered, args.monitoring_namespace)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(rendered, encoding="utf-8")
    print(
        f"validated {ROLE_BINDING} subject namespace={args.namespace}; "
        f"namespace correction={'yes' if changed else 'no'}; "
        f"monitoring namespace={'unchanged' if args.monitoring_namespace is None else repr(args.monitoring_namespace)}; "
        f"monitoring correction={'yes' if monitoring_changed else 'no'}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
