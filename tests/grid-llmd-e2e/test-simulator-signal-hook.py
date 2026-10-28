#!/usr/bin/env python3
"""Local contract tests for the live simulator signal hooks."""

import importlib.util
from pathlib import Path

SCRIPT = Path(__file__).with_name("simulator-signal-hook.py")
SPEC = importlib.util.spec_from_file_location("simulator_signal_hook", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
HOOK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(HOOK)


def main() -> None:
    ref = HOOK.parse_control_reference(
        "grid-llmd-e2e/configmap/pool-a-simulator-control#config.yaml@pool-a",
        "grid-llmd-e2e",
    )
    assert ref["identity"] == "grid-llmd-e2e/configmap/pool-a-simulator-control#config.yaml"
    assert ref["pool"] == "pool-a"
    config = """model: "grid-test-model"
served-model-name:
- "pool-a"
- "grid-test-model"
mode: echo
port: 8000
fake-metrics:
  running-requests: 0
  waiting-requests: 0
  kv-cache-usage: 0
"""
    baseline = HOOK.parse_fake_metrics(config)
    assert baseline == {"running-requests": 0, "waiting-requests": 0, "kv-cache-usage": 0}
    pressured = HOOK.replace_fake_metric(config, "waiting-requests", 20)
    assert "  waiting-requests: 20\n" in pressured
    assert "  running-requests: 0\n" in pressured
    expected = HOOK.parse_fake_metrics(pressured)
    assert HOOK.metrics_match(
        {"running-requests": 0, "waiting-requests": 20, "kv-cache-usage": 0},
        expected,
    )
    assert not HOOK.metrics_match(
        {"running-requests": 0, "waiting-requests": 19, "kv-cache-usage": 0},
        expected,
    )
    assert HOOK.selected_fake_metric({}) == "waiting-requests"
    assert HOOK.selected_fake_metric({
        "EPP_QUEUE_METRIC": "queue_depth",
        "EPP_KV_CACHE_METRIC": "kv_utilization",
        "GRID_SIGNAL_METRIC": "queue_depth",
    }) == "waiting-requests"
    assert HOOK.selected_fake_metric({
        "EPP_QUEUE_METRIC": "queue_depth",
        "EPP_KV_CACHE_METRIC": "kv_utilization",
        "GRID_SIGNAL_METRIC": "kv_utilization",
    }) == "kv-cache-usage"
    assert HOOK.parse_pressure_value("waiting-requests", {}) == 20
    assert HOOK.parse_pressure_value("kv-cache-usage", {}) == 0.95
    assert HOOK.parse_pressure_value("kv-cache-usage", {
        "GRID_LLMD_PRESSURE_SIGNAL_VALUE": "0.85",
    }) == 0.85
    kv_pressured = HOOK.replace_fake_metric(config, "kv-cache-usage", 0.95)
    assert "  kv-cache-usage: 0.95\n" in kv_pressured
    assert HOOK.parse_fake_metrics(kv_pressured)["waiting-requests"] == 0
    for environment in (
        {"GRID_SIGNAL_METRIC": "unconfigured_signal"},
        {"GRID_LLMD_PRESSURE_SIGNAL_VALUE": "NaN"},
        {"GRID_LLMD_PRESSURE_SIGNAL_VALUE": "1.1"},
    ):
        try:
            if "GRID_SIGNAL_METRIC" in environment:
                HOOK.selected_fake_metric(environment)
            else:
                HOOK.parse_pressure_value("kv-cache-usage", environment)
        except HOOK.HookError:
            pass
        else:
            raise AssertionError(f"invalid signal configuration accepted: {environment}")
    controls = HOOK.parse_control_list(
        "grid-llmd-e2e/configmap/pool-a-simulator-control#config.yaml@pool-a,"
        "grid-llmd-e2e/configmap/pool-b-simulator-control#config.yaml@pool-b",
        "grid-llmd-e2e",
    )
    assert [item["pool"] for item in controls] == ["pool-a", "pool-b"]
    for raw in (
        "other/configmap/pool-a#config.yaml@pool-a",
        "grid-llmd-e2e/configmap/pool-a#config.yaml",
        "grid-llmd-e2e/secret/pool-a#config.yaml@pool-a",
    ):
        try:
            HOOK.parse_control_reference(raw, "grid-llmd-e2e")
        except HOOK.HookError:
            pass
        else:
            raise AssertionError(f"unsafe control reference accepted: {raw}")
    try:
        HOOK.parse_control_list(
            "grid-llmd-e2e/configmap/a#config.yaml@pool-a,"
            "grid-llmd-e2e/configmap/b#config.yaml@pool-a",
            "grid-llmd-e2e",
        )
    except HOOK.HookError:
        pass
    else:
        raise AssertionError("duplicate pool mapping accepted")
    print("simulator runtime signal hook parsing and metric contract: PASS")


if __name__ == "__main__":
    main()
