#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$SCRIPT_DIR/deploy-serving-pools.sh"
bash -n "$SCRIPT"

python3 - "$SCRIPT" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'wait_seconds="$((build_deadline_seconds + BUILD_STATUS_SYNC_TIMEOUT_SECONDS - SECONDS))"',
    'build_deadline_seconds=$((SECONDS + BUILD_TIMEOUT_SECONDS))',
    'start-build "buildconfig/$SIMULATOR_BUILD_CONFIG" -n "$NAMESPACE" --follow',
    'OpenShift build-log stream exited with code=$build_status while build phase=$phase',
    'continue watching $build_name without starting another build',
    'Adopting active Build $build_name for the pinned source revision; elapsed=${active_elapsed_seconds}s remaining-build-budget=${active_remaining_seconds}s; will not start a duplicate build',
    "active_started_at=\"$(oc get build \"$build_name\" -n \"$NAMESPACE\" -o jsonpath='{.status.startTimestamp}')\"",
    'active_remaining_seconds="$((BUILD_TIMEOUT_SECONDS - active_elapsed_seconds))"',
    'Complete)',
    'continue immutable output verification',
    'Failed|Error|Cancelled|Canceled',
)
missing = [fragment for fragment in required if fragment not in source]
if missing:
    raise SystemExit(f"simulator build watcher contract missing: {missing!r}")

watch_start = source.index("wait_for_build_terminal()")
watch_end = source.index("\nbuild_simulator()", watch_start)
watch = source[watch_start:watch_end]
if 'local build_name="$1" build_deadline_seconds="$2" phase wait_seconds deadline' not in watch:
    raise SystemExit("build terminal watcher must keep build state local")
if 'wait_seconds="${BUILD_STATUS_SYNC_TIMEOUT%s}"' in watch:
    raise SystemExit("build terminal watcher must not cap build execution at status-sync grace")
if 'wait_seconds="$((build_deadline_seconds + BUILD_STATUS_SYNC_TIMEOUT_SECONDS - SECONDS))"' not in watch:
    raise SystemExit("build terminal watcher must honor the original absolute build deadline")

build_start = source.index("build_simulator()")
build_end = source.index("\nmake_task_scheduler_config()", build_start)
build = source[build_start:build_end]
if build.index("case \"$phase\" in") > build.index('return "$build_status"'):
    raise SystemExit("build stream exit must be classified by the current Build API phase")
if build.index('build_deadline_seconds=$((SECONDS + BUILD_TIMEOUT_SECONDS))') > build.index('timeout --foreground --signal=TERM'):
    raise SystemExit("build deadline must be established before the bounded build/log-stream command")
adoption = build[build.index('if [[ "$phase" == New || "$phase" == Pending || "$phase" == Running ]]'):build.index('if [[ -z "$build_name" ]]')]
if 'build_deadline_seconds=$((SECONDS + BUILD_TIMEOUT_SECONDS))' in adoption:
    raise SystemExit("adopting an active Build must not reset its original execution budget")
print("llm-d-sim Build API duration and interrupted-log-stream preservation contract: PASS")
PY
