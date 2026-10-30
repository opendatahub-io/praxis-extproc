#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TEMP_DIR"' EXIT

bash "$SCRIPT_DIR/render-monitoringstack-profile.sh" --profile strict --output "$TEMP_DIR/strict.yaml"
bash "$SCRIPT_DIR/render-monitoringstack-profile.sh" --profile reduced-dev --output "$TEMP_DIR/reduced.yaml"

grep -Fq 'cpu: "100m"' "$TEMP_DIR/strict.yaml"
grep -Fq 'cpu: "50m"' "$TEMP_DIR/strict.yaml"
grep -Fq 'cpu: "50m"' "$TEMP_DIR/reduced.yaml"
grep -Fq 'cpu: "10m"' "$TEMP_DIR/reduced.yaml"
if grep -Eq 'replicas:|memory:|limits:|retention:|storage:' "$TEMP_DIR/reduced.yaml"; then
  printf 'reduced-dev overlay must preserve replicas, memory, limits, retention, and storage\n' >&2
  exit 1
fi
diff -u "$TEMP_DIR/strict.yaml" "$TEMP_DIR/reduced.yaml" >"$TEMP_DIR/profile.diff" || [[ "$?" == 1 ]]
[[ "$(grep -c '^[+-].*cpu:' "$TEMP_DIR/profile.diff")" == 4 ]]

python3 - "$SCRIPT_DIR/cold-deploy.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
collect = source.split("  collect)\n", 1)[1].split("    ;;", 1)[0]
profile_check = source.split("verify_monitoring_resource_profile() {\n", 1)[1].split("\n}\n", 1)[0]
if "verify_monitoring_resource_profile" not in collect or "verify_platform || true" not in collect:
    raise SystemExit("read-only collection must verify the reduced-dev MonitoringStack before platform gates")
if "apply_monitoringstack_resource_profile" in collect or "verify_monitoring_profile_survives_reconciliation" in collect:
    raise SystemExit("read-only collection must not apply or reconcile the MonitoringStack override")
if "apply_monitoringstack_resource_profile" in profile_check:
    raise SystemExit("direct MonitoringStack profile verification must remain read-only")
PY

printf 'strict and reduced-dev overlays change only the Prometheus/Thanos CPU requests: PASS\n'
