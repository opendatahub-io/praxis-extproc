#!/usr/bin/env python3
"""Create run ownership labels and perform ownership-checked cleanup."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


RUN_RE = re.compile(r"^[A-Za-z0-9]([A-Za-z0-9._-]{0,61}[A-Za-z0-9])?$")
SCENARIOS = {"grid-llmd", "grid-maas"}
ALLOWED_KINDS = {
    "Namespace",
    "Deployment",
    "Service",
    "Secret",
    "ConfigMap",
    "ServiceAccount",
    "Role",
    "RoleBinding",
    "Job",
    "BuildConfig",
    "ImageStream",
    "Gateway",
    "HTTPRoute",
    "ReferenceGrant",
    "GridNetwork",
    "GridSite",
    "InferenceProvider",
    "InferencePool",
    "LLMInferenceService",
    "ExternalProvider",
    "ExternalModel",
    "MaaSModelRef",
    "MaaSSubscription",
    "MaaSAuthPolicy",
    "EnvoyFilter",
    "ServiceEntry",
    "DestinationRule",
    "EndpointSlice",
    "ServiceMonitor",
}


class OwnershipError(RuntimeError):
    """Raised when an object is not demonstrably owned by the run."""


def validate_identity(run_id: str, scenario: str) -> None:
    if not RUN_RE.fullmatch(run_id):
        raise OwnershipError("run ID is not a valid Kubernetes label value")
    if scenario not in SCENARIOS:
        raise OwnershipError("scenario must be grid-llmd or grid-maas")


def labels_for(run_id: str, scenario: str) -> dict[str, str]:
    validate_identity(run_id, scenario)
    return {
        "app.kubernetes.io/managed-by": "praxis-extproc-e2e",
        "app.kubernetes.io/part-of": f"{scenario}-e2e",
        "praxis.fast/qualification-run": run_id,
        "praxis.fast/qualification-scenario": scenario,
    }


def object_is_owned(document: dict, run_id: str, scenario: str) -> bool:
    metadata = document.get("metadata", {})
    labels = metadata.get("labels", {})
    expected = labels_for(run_id, scenario)
    return all(labels.get(key) == value for key, value in expected.items())


def run_oc(arguments: list[str], context: str | None) -> subprocess.CompletedProcess:
    command = ["oc"]
    if context:
        command.append(f"--context={context}")
    command.extend(arguments)
    env = os.environ.copy()
    return subprocess.run(command, env=env, check=False, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)


def require_target(expected_api: str, context: str | None) -> None:
    server = run_oc(["whoami", "--show-server"], context)
    if server.returncode != 0 or server.stdout.strip() != expected_api:
        raise OwnershipError("cleanup target API does not match EXPECTED_API_SERVER")
    ready = run_oc(["get", "--raw=/readyz"], context)
    if ready.returncode != 0 or "ok" not in ready.stdout.lower():
        raise OwnershipError("cleanup target /readyz is not healthy")


def load_inventory(path: Path, run_id: str, scenario: str) -> list[dict]:
    document = json.loads(path.read_text(encoding="utf-8"))
    if document.get("run_id") != run_id or document.get("scenario") != scenario:
        raise OwnershipError("ownership inventory run ID or scenario does not match")
    resources = document.get("resources")
    if not isinstance(resources, list):
        raise OwnershipError("ownership inventory must contain a resources array")
    seen: set[tuple[str, str | None, str]] = set()
    for item in resources:
        if not isinstance(item, dict) or item.get("kind") not in ALLOWED_KINDS:
            raise OwnershipError("ownership inventory contains an unsupported resource kind")
        name = item.get("name")
        namespace = item.get("namespace")
        uid = item.get("uid")
        if not isinstance(name, str) or not name or not isinstance(uid, str) or not uid:
            raise OwnershipError("every resource inventory entry needs name and UID")
        if item["kind"] != "Namespace" and (not isinstance(namespace, str) or not namespace):
            raise OwnershipError("namespaced resource inventory entry has no namespace")
        identity = (item["kind"], namespace, name)
        if identity in seen:
            raise OwnershipError("ownership inventory contains a duplicate resource")
        seen.add(identity)
    return resources


def current_object(item: dict, context: str | None) -> tuple[str, dict | None]:
    args = ["get", item["kind"], item["name"], "-o", "json"]
    if item["kind"] != "Namespace":
        args.extend(["-n", item["namespace"]])
    result = run_oc(args, context)
    if result.returncode == 0:
        try:
            return "present", json.loads(result.stdout)
        except json.JSONDecodeError as error:
            raise OwnershipError("live resource response was not valid JSON") from error
    if "NotFound" in result.stderr:
        return "absent", None
    raise OwnershipError(f"could not safely read {item['kind']}/{item['name']}")


def cleanup(args: argparse.Namespace) -> dict:
    validate_identity(args.run_id, args.scenario)
    resources = load_inventory(args.inventory, args.run_id, args.scenario)
    if args.apply:
        if not args.expected_api:
            raise OwnershipError("EXPECTED_API_SERVER is required for cleanup apply")
        require_target(args.expected_api, args.context)
    outcomes: list[dict] = []
    for item in reversed(resources):
        identity = {key: item[key] for key in ("kind", "namespace", "name", "uid") if key in item}
        if not args.apply:
            outcomes.append({**identity, "status": "PLAN_ONLY"})
            continue
        state, live = current_object(item, args.context)
        if state == "absent":
            outcomes.append({**identity, "status": "ALREADY_ABSENT"})
            continue
        assert live is not None
        metadata = live.get("metadata", {})
        if metadata.get("uid") != item["uid"] or not object_is_owned(live, args.run_id, args.scenario):
            raise OwnershipError(f"live ownership or UID changed for {item['kind']}/{item['name']}; cleanup stopped")
        delete_args = ["delete", item["kind"], item["name"], "--timeout=120s"]
        if item["kind"] != "Namespace":
            delete_args.extend(["-n", item["namespace"]])
        deleted = run_oc(delete_args, args.context)
        if deleted.returncode != 0:
            raise OwnershipError(f"delete failed for owned object {item['kind']}/{item['name']}")
        state_after, _ = current_object(item, args.context)
        if state_after != "absent":
            raise OwnershipError(f"owned object remains after deletion: {item['kind']}/{item['name']}")
        outcomes.append({**identity, "status": "DELETED_VERIFIED"})
    return {
        "schema": "praxis-extproc.cleanup-result/v1",
        "captured_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "run_id": args.run_id,
        "scenario": args.scenario,
        "apply": args.apply,
        "results": outcomes,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    labels_parser = subparsers.add_parser("labels")
    labels_parser.add_argument("--run-id", required=True)
    labels_parser.add_argument("--scenario", required=True)
    labels_parser.add_argument("--output", type=Path)
    cleanup_parser = subparsers.add_parser("cleanup")
    cleanup_parser.add_argument("--inventory", type=Path, required=True)
    cleanup_parser.add_argument("--run-id", required=True)
    cleanup_parser.add_argument("--scenario", required=True)
    cleanup_parser.add_argument("--context")
    cleanup_parser.add_argument("--expected-api", default=os.getenv("EXPECTED_API_SERVER"))
    cleanup_parser.add_argument("--apply", action="store_true")
    cleanup_parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if args.command == "labels":
            result = labels_for(args.run_id, args.scenario)
            rendered = json.dumps(result, indent=2, sort_keys=True) + "\n"
            if args.output:
                args.output.parent.mkdir(parents=True, exist_ok=True)
                args.output.write_text(rendered, encoding="utf-8")
                os.chmod(args.output, 0o600)
            else:
                print(rendered, end="")
            return 0
        result = cleanup(args)
        if args.output.exists():
            raise OwnershipError("cleanup result already exists")
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.chmod(args.output, 0o600)
        print(f"cleanup result recorded: {args.output}")
        return 0
    except (OSError, json.JSONDecodeError, OwnershipError) as error:
        print(f"ownership cleanup blocked: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
