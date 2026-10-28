#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = (
    'SIMULATOR_SOURCE_URL="$(env_default SIMULATOR_SOURCE_URL https://github.com/llm-d/llm-d-inference-sim.git)"',
    'SIMULATOR_OCI_SOURCE_URL="$(env_default SIMULATOR_OCI_SOURCE_URL https://github.com/llm-d/llm-d-inference-sim)"',
    'SOURCE_LOCKS_MD="$(env_default SOURCE_LOCKS_MD "$REPO_ROOT/evidence/$RUN_LABEL/source-locks.md")"',
    'praxis.fast/oci-source-url: "$SIMULATOR_OCI_SOURCE_URL"',
    'uri: "$SIMULATOR_SOURCE_URL"',
    'value: "$SIMULATOR_OCI_SOURCE_URL"',
    '[[ "$source" == "$SIMULATOR_OCI_SOURCE_URL" ]]',
    'simulator-build|simulator-image-provenance) classification=',
    'CURRENT_PHASE=simulator-image-provenance',
    'cp "$SOURCE_LOCKS_MD" "$EVIDENCE_DIR/source-lock.md"',
)
missing = [fragment for fragment in required if fragment not in source]
forbidden = ('cp "$SCRIPT_DIR/source-lock.md" "$EVIDENCE_DIR/source-lock.md"',)
present = [fragment for fragment in forbidden if fragment in source]
if missing or present:
    raise SystemExit(
        "simulator provenance or run-specific source-lock contract is incomplete: "
        f"missing={missing!r}, forbidden={present!r}"
    )

apply_start = source.index('  apply)\n', source.index('case "$MODE" in', source.index('api_access\n')))
apply_end = source.index('  verify)\n', apply_start)
apply = source[apply_start:apply_end]
ordered = (
    "verify_post_transport_platform_gate",
    "CURRENT_PHASE=simulator-image-provenance",
    "verify_simulator_image_pin",
    "make_task_scheduler_config",
    'apply_one_pool "$POOL_A"',
)
positions = [apply.index(fragment) for fragment in ordered]
if positions != sorted(positions):
    raise SystemExit("simulator image provenance and post-DSCI gates must pass before pool resources are applied")

print("simulator Git remote and canonical OCI source identity are separately verified before serving mutation: PASS")
PY
