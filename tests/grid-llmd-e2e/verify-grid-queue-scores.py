#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare Grid's queue-score breakdown with authenticated EPP samples."""

from __future__ import annotations

import argparse
import json
import math
import re
import sys
from pathlib import Path


# The pinned Grid source c21ae729 resolves QueueDepth to queue weight 1.0.
QUEUE_SCORE_WEIGHT = 1.0
SCORE_TOLERANCE = 0.02
PROM_VALUE = re.compile(
    r"^(?P<metric>[a-zA-Z_:][a-zA-Z0-9_:]*)(?:\{(?P<labels>[^}]*)\})?\s+"
    r"(?P<value>[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?)"
)


def fail(message: str) -> "None":
    print(f"Grid queue-score verification: {message}", file=sys.stderr)
    raise SystemExit(1)


def read_metric(path: Path, metric: str, pool_name: str) -> float:
    label_pattern = re.compile(rf'(?:^|,)\s*name="{re.escape(pool_name)}"(?:,|$)')
    values: list[float] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        match = PROM_VALUE.match(line.strip())
        if match is None or match.group("metric") != metric:
            continue
        labels = match.group("labels") or ""
        if label_pattern.search(labels) is None:
            continue
        value = float(match.group("value"))
        if not math.isfinite(value) or value < 0:
            fail(f"{path} contains a non-finite or negative {metric} sample")
        values.append(value)
    if len(values) != 1:
        fail(f"{path} must contain exactly one {metric} sample for InferencePool {pool_name}; found {len(values)}")
    return values[0]


def candidate_for(overlay: dict, model: str, provider: str) -> dict:
    candidates = [
        candidate
        for candidate in overlay.get("overlay", {}).get("candidates", [])
        if candidate.get("kind") == "inference_model"
        and candidate.get("name") == model
        and candidate.get("cluster") == provider
    ]
    if len(candidates) != 1:
        fail(f"overlay must contain exactly one {model} candidate for provider {provider}; found {len(candidates)}")
    candidate = candidates[0]
    score = candidate.get("score")
    rank = candidate.get("rank")
    queue_score = candidate.get("score_breakdown", {}).get("queue_depth")
    if not isinstance(score, (int, float)) or not math.isfinite(score):
        fail(f"provider {provider} candidate has no finite numeric score")
    if not isinstance(rank, int):
        fail(f"provider {provider} candidate has no integer rank")
    if not isinstance(queue_score, (int, float)) or not math.isfinite(queue_score):
        fail(f"provider {provider} candidate has no finite queue-depth score breakdown")
    return candidate


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--overlay", type=Path, required=True)
    parser.add_argument("--queue-metric", required=True)
    parser.add_argument("--queue-capacity", type=float, required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--provider-a", required=True)
    parser.add_argument("--provider-b", required=True)
    parser.add_argument("--pool-a", required=True)
    parser.add_argument("--pool-b", required=True)
    parser.add_argument("--pool-a-samples", type=Path, required=True)
    parser.add_argument("--pool-b-samples", type=Path, required=True)
    args = parser.parse_args()
    if not math.isfinite(args.queue_capacity) or args.queue_capacity <= 0:
        fail("queue capacity must be a positive finite number")
    try:
        overlay = json.loads(args.overlay.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read the Grid routing overlay: {error}")

    provider_data = []
    for provider, pool_name, sample_path in (
        (args.provider_a, args.pool_a, args.pool_a_samples),
        (args.provider_b, args.pool_b, args.pool_b_samples),
    ):
        candidate = candidate_for(overlay, args.model, provider)
        epp_queue = read_metric(sample_path, args.queue_metric, pool_name)
        normalized = min(epp_queue / args.queue_capacity, 1.0)
        expected = QUEUE_SCORE_WEIGHT * (1.0 - normalized)
        observed = float(candidate["score_breakdown"]["queue_depth"])
        if abs(expected - observed) > SCORE_TOLERANCE:
            fail(
                f"provider {provider} score breakdown {observed:.6f} does not match "
                f"EPP queue sample {epp_queue:.6f} / capacity {args.queue_capacity:.6f} "
                f"(expected {expected:.6f})"
            )
        provider_data.append(
            {
                "provider": provider,
                "poolName": pool_name,
                "eppQueueDepth": epp_queue,
                "queueCapacity": args.queue_capacity,
                "queueDepthScoreWeight": QUEUE_SCORE_WEIGHT,
                "expectedQueueDepthScore": expected,
                "observedQueueDepthScore": observed,
                "score": candidate["score"],
                "rank": candidate["rank"],
            }
        )

    if provider_data[0]["rank"] == provider_data[1]["rank"]:
        fail("the two task providers must have distinct overlay ranks")
    if provider_data[0]["score"] != provider_data[1]["score"]:
        higher = 0 if provider_data[0]["score"] > provider_data[1]["score"] else 1
        lower = 1 - higher
        if provider_data[higher]["rank"] >= provider_data[lower]["rank"]:
            fail("overlay ranks do not order the higher-scoring task provider first")

    json.dump(
        {
            "queueMetric": args.queue_metric,
            "scoreTolerance": SCORE_TOLERANCE,
            "sourceQueueScoreWeight": QUEUE_SCORE_WEIGHT,
            "providers": provider_data,
            "verified": True,
        },
        sys.stdout,
        indent=2,
        sort_keys=True,
    )
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
