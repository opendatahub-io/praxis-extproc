#!/usr/bin/env python3
"""Change llm-d-sim fake metrics through its live admin API and restore them."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

SCHEMA = "praxis-extproc.grid-llmd-simulator-hook/v1"
METRICS = ("running-requests", "waiting-requests", "kv-cache-usage")
OWNER_LABELS = {
    "app.kubernetes.io/managed-by": "grid-llmd-e2e-run",
    "app.kubernetes.io/part-of": "grid-llmd-e2e",
}


class HookError(RuntimeError):
    pass


def utc_now() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def digest(value: str) -> str:
    return hashlib.sha256(value.encode("utf-8")).hexdigest()


def safe_name(value: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "-", value).strip("-") or "resource"


def run(command: list[str], description: str) -> str:
    result = subprocess.run(command, text=True, capture_output=True, check=False)
    if result.returncode:
        raise HookError(f"{description} failed with exit status {result.returncode}")
    return result.stdout


def kubectl_base(context: str) -> list[str]:
    command = ["kubectl", "--context", context]
    ca_file = os.environ.get("KUBE_CA_FILE", "")
    if ca_file:
        command += ["--certificate-authority", ca_file, "--insecure-skip-tls-verify=false"]
    return command


def kubectl(context: str, *args: str, description: str) -> str:
    command = kubectl_base(context)
    command.extend(args)
    return run(command, description)


def kubectl_json(context: str, *args: str, description: str) -> dict[str, Any]:
    try:
        value = json.loads(kubectl(context, *args, description=description))
    except json.JSONDecodeError as error:
        raise HookError(f"{description} returned invalid JSON") from error
    if not isinstance(value, dict):
        raise HookError(f"{description} did not return a JSON object")
    return value


def parse_control_reference(raw: str, namespace: str) -> dict[str, str]:
    reference, separator, key_pool = raw.partition("#")
    if not separator:
        raise HookError("controls must use configmap/name#data-key@pool")
    data_key, at, pool = key_pool.partition("@")
    parts = reference.split("/")
    if len(parts) == 2:
        ref_namespace, kind, name = namespace, parts[0], parts[1]
    elif len(parts) == 3:
        ref_namespace, kind, name = parts
    else:
        raise HookError("simulator control has an invalid resource path")
    if kind not in ("configmap", "configmaps") or not name or not data_key or not at or not pool:
        raise HookError("controls must use configmap/name#data-key@pool")
    if ref_namespace != namespace:
        raise HookError("simulator controls must stay in the task-owned namespace")
    return {
        "namespace": ref_namespace,
        "name": name,
        "dataKey": data_key,
        "pool": pool,
        "identity": f"{ref_namespace}/configmap/{name}#{data_key}",
    }


def parse_control_list(raw: str, namespace: str) -> list[dict[str, str]]:
    controls = [parse_control_reference(item.strip(), namespace) for item in raw.split(",") if item.strip()]
    if len(controls) < 2 or len({item["pool"] for item in controls}) != len(controls):
        raise HookError("signal qualification requires one distinct simulator control per pool")
    if len({item["identity"] for item in controls}) != len(controls):
        raise HookError("simulator control resources must be distinct")
    return controls


def parse_fake_metrics(config: str) -> dict[str, int | float]:
    in_section = False
    values: dict[str, int | float] = {}
    for line in config.splitlines():
        if line == "fake-metrics:":
            in_section = True
            continue
        if in_section and line and not line[0].isspace():
            in_section = False
        if not in_section:
            continue
        match = re.fullmatch(r"\s{2}(running-requests|waiting-requests|kv-cache-usage):\s*(-?\d+(?:\.\d+)?)\s*", line)
        if match:
            key, raw = match.groups()
            if key in values:
                raise HookError(f"simulator config repeats {key}")
            number = float(raw) if "." in raw else int(raw)
            if number < 0:
                raise HookError(f"simulator fake metric {key} must be non-negative")
            values[key] = number
    if set(values) != set(METRICS):
        raise HookError("simulator config must define numeric running, waiting, and KV-cache fake metrics")
    return values


def replace_fake_metric(config: str, key: str, value: int | float) -> str:
    if key not in METRICS or value < 0:
        raise HookError("unsupported simulator fake-metric update")
    pattern = re.compile(rf"(?m)^(\s{{2}}{re.escape(key)}:\s*)-?\d+(?:\.\d+)?(\s*)$")
    updated, count = pattern.subn(lambda match: f"{match.group(1)}{value}{match.group(2)}", config)
    if count != 1:
        raise HookError(f"expected one generated fake-metrics value for {key}")
    parse_fake_metrics(updated)
    return updated


def selected_fake_metric(environment: dict[str, str] | None = None) -> str:
    values = os.environ if environment is None else environment
    queue_signal = values.get("EPP_QUEUE_METRIC", "llm_d_epp_average_queue_size")
    kv_signal = values.get("EPP_KV_CACHE_METRIC", "llm_d_epp_average_kv_cache_utilization")
    selected = values.get("GRID_SIGNAL_METRIC", queue_signal)
    if selected == queue_signal:
        return "waiting-requests"
    if selected == kv_signal:
        return "kv-cache-usage"
    raise HookError("GRID_SIGNAL_METRIC must match the configured EPP queue or KV-cache metric")


def parse_pressure_value(fake_metric: str, environment: dict[str, str] | None = None) -> int | float:
    values = os.environ if environment is None else environment
    if fake_metric == "waiting-requests":
        raw = values.get("GRID_LLMD_PRESSURE_SIGNAL_VALUE", values.get("GRID_LLMD_PRESSURE_WAITING_REQUESTS", "20"))
        maximum = 1_000_000
        setting = "GRID_LLMD_PRESSURE_SIGNAL_VALUE/GRID_LLMD_PRESSURE_WAITING_REQUESTS"
    elif fake_metric == "kv-cache-usage":
        raw = values.get("GRID_LLMD_PRESSURE_SIGNAL_VALUE", values.get("GRID_LLMD_PRESSURE_KV_CACHE_USAGE", "0.95"))
        maximum = 1
        setting = "GRID_LLMD_PRESSURE_SIGNAL_VALUE/GRID_LLMD_PRESSURE_KV_CACHE_USAGE"
    else:
        raise HookError("unsupported EPP signal fake metric")
    if not re.fullmatch(r"\d+(?:\.\d+)?", raw):
        raise HookError(f"{setting} must be a positive numeric value")
    number: int | float = float(raw) if "." in raw else int(raw)
    if not 0 < number <= maximum:
        raise HookError(f"{setting} is outside the allowed range")
    return number


def metrics_match(actual: dict[str, Any], expected: dict[str, int | float]) -> bool:
    for key in METRICS:
        value = actual.get(key)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or abs(value - expected[key]) > 1e-9:
            return False
    return True


def write_private_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path.parent, 0o700)
    temporary = path.with_suffix(path.suffix + ".tmp")
    descriptor = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")
    os.replace(temporary, path)
    os.chmod(path, 0o600)


def read_private_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise HookError(f"cannot read saved simulator baseline {path.name}") from error
    if not isinstance(value, dict):
        raise HookError("saved simulator baseline is not a JSON object")
    return value


def verify_api(context: str, namespace: str, evidence: Path) -> tuple[str, bool, str]:
    expected = os.environ.get("EXPECTED_API_SERVER", "")
    run_label = os.environ.get("GRID_RUN_LABEL", "")
    if not expected or not run_label:
        raise HookError("EXPECTED_API_SERVER and GRID_RUN_LABEL are required")
    config = kubectl_json(
        context, "config", "view", "--minify", "--output=json",
        description="read selected kube context metadata",
    )
    clusters = config.get("clusters", [])
    if len(clusters) != 1:
        raise HookError("selected kube context does not resolve to one API server")
    cluster = clusters[0].get("cluster", {})
    server = cluster.get("server", "")
    if server != expected:
        raise HookError("selected kube context API does not match EXPECTED_API_SERVER")
    ready = kubectl(context, "get", "--raw=/readyz", description="verify API readiness")
    if "ok" not in ready:
        raise HookError("API /readyz did not return ok")
    insecure = cluster.get("insecure-skip-tls-verify") is True and not os.environ.get("KUBE_CA_FILE")
    if insecure and not (
        os.environ.get("CLUSTER_ENV") == "ephemeral-dev"
        and os.environ.get("ALLOW_INSECURE_DEV_API") == "true"
    ):
        raise HookError("API TLS bypass requires explicit ephemeral-dev authorization")
    tls_mode = "explicit-ephemeral-dev-unverified" if insecure else "kubeconfig-verified"
    api_ca_verified = not insecure
    namespace_obj = kubectl_json(
        context, "get", "namespace", namespace, "--output=json",
        description="verify task namespace ownership",
    )
    labels = namespace_obj.get("metadata", {}).get("labels", {})
    if labels.get("app.kubernetes.io/part-of") != "grid-llmd-e2e" or labels.get("praxis.fast/qualification-run") != run_label:
        raise HookError("simulator hook namespace is not owned by this run")
    write_private_json(evidence / f"simulator-hook-api-{safe_name(Path(sys.argv[0]).name)}.json", {
        "capturedAt": utc_now(),
        "apiServer": server,
        "context": context,
        "readyz": "ok",
        "apiTlsMode": tls_mode,
        "runLabel": run_label,
        "namespace": namespace,
    })
    return server, api_ca_verified, run_label


def read_control(context: str, reference: dict[str, str], run_label: str) -> dict[str, Any]:
    obj = kubectl_json(
        context, "get", "configmap", reference["name"], "--namespace", reference["namespace"], "--output=json",
        description=f"read simulator control {reference['name']}",
    )
    metadata = obj.get("metadata", {})
    labels = metadata.get("labels", {})
    if any(labels.get(key) != value for key, value in OWNER_LABELS.items()):
        raise HookError(f"simulator control {reference['name']} is not task-owned")
    if labels.get("praxis.fast/qualification-run") != run_label or labels.get("praxis.fast/simulator-pool") != reference["pool"]:
        raise HookError(f"simulator control {reference['name']} run/pool labels do not match")
    config = obj.get("data", {}).get(reference["dataKey"])
    if not isinstance(config, str):
        raise HookError(f"simulator control {reference['name']} lacks its declared data key")
    return {
        **reference,
        "uid": metadata.get("uid", ""),
        "resourceVersion": metadata.get("resourceVersion", ""),
        "config": config,
        "sha256": digest(config),
        "metrics": parse_fake_metrics(config),
    }


def baseline_document(path: Path, server: str, run_label: str, controls: list[dict[str, Any]]) -> dict[str, Any]:
    by_identity = {item["identity"]: item for item in controls}
    if path.exists():
        saved = read_private_json(path)
        if saved.get("schema") != "praxis-extproc.grid-llmd-simulator-control-baseline/v1":
            raise HookError("saved simulator baseline schema is unsupported")
        if saved.get("apiServer") != server or saved.get("runLabel") != run_label:
            raise HookError("saved simulator baseline belongs to another cluster or run")
        original = saved.get("controls", {})
        if set(original) != set(by_identity):
            raise HookError("saved simulator baseline control set differs from this run")
        for identity, current in by_identity.items():
            previous = original[identity]
            if previous.get("uid") != current["uid"]:
                raise HookError(f"simulator control {current['name']} UID changed since baseline")
            parse_fake_metrics(previous.get("config", ""))
        return saved
    saved = {
        "schema": "praxis-extproc.grid-llmd-simulator-control-baseline/v1",
        "capturedAt": utc_now(),
        "apiServer": server,
        "runLabel": run_label,
        "controls": {
            item["identity"]: {
                "namespace": item["namespace"],
                "name": item["name"],
                "dataKey": item["dataKey"],
                "pool": item["pool"],
                "uid": item["uid"],
                "resourceVersion": item["resourceVersion"],
                "sha256": item["sha256"],
                "config": item["config"],
                "metrics": item["metrics"],
            }
            for item in controls
        },
    }
    write_private_json(path, saved)
    return saved


def write_control(context: str, current: dict[str, Any], desired: str, phase: str, evidence: Path, run_label: str) -> dict[str, Any]:
    if current["config"] != desired:
        patch_path = evidence / f"simulator-control-patch-{safe_name(current['pool'])}-{phase}.json"
        write_private_json(patch_path, {"data": {current["dataKey"]: desired}})
        kubectl(
            context, "patch", "configmap", current["name"], "--namespace", current["namespace"],
            "--type=merge", f"--patch-file={patch_path}",
            description=f"update task-owned simulator control {current['name']}",
        )
    return read_control(context, current, run_label)


def simulator_pods(context: str, namespace: str, reference: dict[str, str], run_label: str) -> list[dict[str, str]]:
    result = kubectl_json(
        context, "get", "pods", "--namespace", namespace, "--output=json",
        description=f"inventory simulator Pods for {reference['pool']}",
    )
    found: list[dict[str, str]] = []
    for pod in result.get("items", []):
        spec = pod.get("spec", {})
        volumes = {
            volume.get("name")
            for volume in spec.get("volumes", [])
            if volume.get("configMap", {}).get("name") == reference["name"]
        }
        if not volumes:
            continue
        metadata = pod.get("metadata", {})
        labels = metadata.get("labels", {})
        if (
            labels.get("praxis.fast/qualification-run") != run_label
            or labels.get("praxis.fast/simulator-pool") != reference["pool"]
        ):
            raise HookError(f"Pod mounting {reference['name']} is not owned by this run and pool")
        container = next((item for item in spec.get("containers", []) if item.get("name") == "main"), None)
        status = next(
            (item for item in pod.get("status", {}).get("containerStatuses", []) if item.get("name") == "main"),
            None,
        )
        mounted = container and any(item.get("name") in volumes for item in container.get("volumeMounts", []))
        ready = (
            pod.get("status", {}).get("phase") == "Running"
            and any(item.get("type") == "Ready" and item.get("status") == "True" for item in pod.get("status", {}).get("conditions", []))
            and status
            and status.get("ready") is True
        )
        image_id = status.get("imageID", "") if status else ""
        if not mounted or not ready or "@sha256:" not in image_id or not metadata.get("uid"):
            raise HookError(f"simulator Pod {metadata.get('name', '<unnamed>')} is not Ready, digest-pinned, and mounted")
        found.append({
            "name": metadata["name"],
            "uid": metadata["uid"],
            "container": "main",
            "imageId": image_id,
        })
    if len(found) != 1:
        raise HookError(f"expected exactly one Ready simulator Pod for {reference['name']}, found {len(found)}")
    return found


def local_port() -> int:
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return int(listener.getsockname()[1])


def admin_request(port: int, method: str, body: dict[str, Any] | None = None) -> tuple[int, dict[str, Any]]:
    payload = None if body is None else json.dumps(body, separators=(",", ":")).encode("utf-8")
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/admin/config",
        data=payload,
        headers={"Content-Type": "application/json"} if payload is not None else {},
        method=method,
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            parsed = json.loads(response.read(1_048_576))
            if not isinstance(parsed, dict):
                raise HookError("simulator /admin/config returned a non-object")
            return response.status, parsed
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as error:
        raise HookError(f"simulator /admin/config {method} request failed") from error


def update_simulator(context: str, namespace: str, pod: dict[str, str], expected: dict[str, int | float], update: bool, phase: str, evidence: Path) -> dict[str, Any]:
    port = local_port()
    log_path = evidence / f"simulator-port-forward-{safe_name(pod['name'])}-{phase}.log"
    log_file = log_path.open("ab")
    os.chmod(log_path, 0o600)
    process = subprocess.Popen(
        kubectl_base(context) + [
            "--namespace", namespace, "port-forward", "--address=127.0.0.1",
            f"pod/{pod['name']}", f"{port}:8000",
        ],
        stdout=log_file,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        deadline = time.monotonic() + 25
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise HookError(f"port-forward for simulator Pod {pod['name']} exited")
            try:
                _, initial = admin_request(port, "GET")
                break
            except HookError:
                time.sleep(0.5)
        else:
            raise HookError(f"simulator Pod {pod['name']} admin endpoint did not become reachable")
        if not isinstance(initial.get("fake-metrics"), dict):
            raise HookError(f"simulator Pod {pod['name']} has no fake-metrics configuration")
        if update:
            status, _ = admin_request(port, "POST", {"fake-metrics": expected})
            if status != 200:
                raise HookError(f"simulator Pod {pod['name']} rejected the fake-metrics update")
        status, current = admin_request(port, "GET")
        actual = current.get("fake-metrics")
        if not isinstance(actual, dict) or not metrics_match(actual, expected):
            raise HookError(f"simulator Pod {pod['name']} runtime fake metrics do not match the control")
        return {"httpStatus": status, "metrics": {key: actual[key] for key in METRICS}}
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
        log_file.close()


def execute(phase: str, args: argparse.Namespace) -> dict[str, Any]:
    evidence = Path(args.evidence_dir)
    evidence.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(evidence, 0o700)
    server, api_ca_verified, run_label = verify_api(args.context, args.namespace, evidence)
    references = parse_control_list(os.environ.get("GRID_LLMD_SIMULATOR_CONTROL_RESOURCES", ""), args.namespace)
    target_pool = os.environ.get("GRID_LLMD_PRESSURE_POOL", "")
    target = next((item for item in references if item["pool"] == target_pool), None)
    if target is None:
        raise HookError("GRID_LLMD_PRESSURE_POOL must identify one configured task pool")
    baseline_path = evidence / "simulator-control-baseline.json"
    if phase == "recovery" and not baseline_path.exists():
        raise HookError("recovery refused because the pressure hook did not save a baseline")

    controls = [read_control(args.context, reference, run_label) for reference in references]
    saved = baseline_document(baseline_path, server, run_label, controls)
    baseline = saved["controls"]
    signal_fake_metric = selected_fake_metric()
    pressure_value = parse_pressure_value(signal_fake_metric)

    desired_configs: dict[str, str] = {}
    pods_by_identity: dict[str, list[dict[str, str]]] = {}
    for reference, current in zip(references, controls, strict=True):
        original = baseline[reference["identity"]]["config"]
        desired = replace_fake_metric(original, signal_fake_metric, pressure_value) if (
            phase == "pressure" and reference["pool"] == target_pool
        ) else original
        expected_pressure = replace_fake_metric(original, signal_fake_metric, pressure_value) if reference["pool"] == target_pool else original
        if current["config"] not in (original, expected_pressure):
            raise HookError(f"control {reference['name']} differs from its saved baseline and this run's pressure")
        desired_configs[reference["identity"]] = desired
        pods = simulator_pods(args.context, args.namespace, reference, run_label)
        pods_by_identity[reference["identity"]] = pods
        if phase == "pressure":
            for pod in pods:
                update_simulator(
                    args.context,
                    args.namespace,
                    pod,
                    current["metrics"],
                    False,
                    "pressure-preflight",
                    evidence,
                )
    if phase == "pressure":
        original_signal = baseline[target["identity"]]["metrics"][signal_fake_metric]
        if parse_fake_metrics(desired_configs[target["identity"]])[signal_fake_metric] <= original_signal:
            raise HookError(f"pressure must raise the selected pool's {signal_fake_metric} fake metric")

    desired_controls: dict[str, dict[str, Any]] = {}
    for reference, current in zip(references, controls, strict=True):
        desired_controls[reference["identity"]] = write_control(
            args.context, current, desired_configs[reference["identity"]], phase, evidence, run_label
        )

    observations: list[dict[str, Any]] = []
    runtime: list[dict[str, Any]] = []
    for reference in references:
        control = desired_controls[reference["identity"]]
        expected_metrics = parse_fake_metrics(control["config"])
        pods = pods_by_identity[reference["identity"]]
        update = phase == "recovery" or reference["pool"] == target_pool
        for pod in pods:
            result = update_simulator(args.context, args.namespace, pod, expected_metrics, update, phase, evidence)
            runtime.append({
                "pool": reference["pool"],
                "podName": pod["name"],
                "podUid": pod["uid"],
                "metrics": result["metrics"],
                "adminHttpStatus": result["httpStatus"],
                "controlSha256": control["sha256"],
            })
            observations.append({
                "controlResource": reference["identity"],
                "podName": pod["name"],
                "podUid": pod["uid"],
                "container": pod["container"],
                "ready": True,
                "imageId": pod["imageId"],
                "controlSha256": control["sha256"],
            })
    if len(observations) != len(references):
        raise HookError("hook did not observe exactly one Ready simulator Pod per configured pool")
    report = {
        "schema": SCHEMA,
        "phase": phase,
        "selectedEppSignal": os.environ.get(
            "GRID_SIGNAL_METRIC",
            os.environ.get("EPP_QUEUE_METRIC", "llm_d_epp_average_queue_size"),
        ),
        "selectedSimulatorFakeMetric": signal_fake_metric,
        "pressurePool": target_pool,
        "pressureValue": pressure_value if phase == "pressure" else None,
        "apiCaVerified": api_ca_verified,
        "observations": observations,
        "runtimeMetrics": runtime,
    }
    write_private_json(evidence / f"simulator-admin-config-{phase}.json", {
        "capturedAt": utc_now(),
        "apiServer": server,
        "runLabel": run_label,
        "phase": phase,
        "selectedEppSignal": os.environ.get(
            "GRID_SIGNAL_METRIC",
            os.environ.get("EPP_QUEUE_METRIC", "llm_d_epp_average_queue_size"),
        ),
        "selectedSimulatorFakeMetric": signal_fake_metric,
        "pressurePool": target_pool,
        "pressureValue": pressure_value if phase == "pressure" else None,
        "runtimeMetrics": runtime,
    })
    return report


def main(argv: list[str] | None = None) -> int:
    values = sys.argv[1:] if argv is None else argv
    if not values or values[0] not in ("pressure", "recovery"):
        print("usage: simulator-signal-hook.py pressure|recovery --context CONTEXT --namespace NS --evidence-dir DIR", file=sys.stderr)
        return 2
    phase, *arguments = values
    parser = argparse.ArgumentParser()
    parser.add_argument("--context", required=True)
    parser.add_argument("--namespace", required=True)
    parser.add_argument("--evidence-dir", required=True)
    args = parser.parse_args(arguments)
    try:
        result = execute(phase, args)
    except HookError as error:
        print(f"simulator signal hook {phase} failed: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, separators=(",", ":"), sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
