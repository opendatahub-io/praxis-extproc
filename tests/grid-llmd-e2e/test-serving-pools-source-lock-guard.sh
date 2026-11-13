#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/deploy-serving-pools.sh" <<'PY'
from pathlib import Path
import json
import os
import subprocess
import sys
import tempfile

helper = Path(sys.argv[1])
with tempfile.TemporaryDirectory(prefix="serving-pools-source-lock-") as temp:
    root = Path(temp)
    fake_bin = root / "bin"
    fake_bin.mkdir()
    sentinel = root / "oc-was-called"
    fake_oc = fake_bin / "oc"
    fake_oc.write_text(
        "#!/usr/bin/env bash\n"
        f"touch {sentinel}\n"
        "exit 99\n",
        encoding="utf-8",
    )
    fake_oc.chmod(0o755)

    components = (
        "llm-d-sim",
        "epp",
        "extproc",
        "gitops",
        "grid",
        "grid-operator",
        "maas",
    )
    revisions = {name: f"{index:040x}" for index, name in enumerate(components, 1)}
    lock = root / "source-locks.json"
    lock.write_text(
        json.dumps(
            {"components": {name: {"resolved_sha": sha} for name, sha in revisions.items()}}
        ),
        encoding="utf-8",
    )
    markdown = root / "source-locks.md"
    markdown.write_text("# Test lock fixture\n", encoding="utf-8")
    evidence = root / "evidence"

    env = os.environ.copy()
    env.update(
        {
            "PATH": f"{fake_bin}:{env['PATH']}",
            "SERVING_POOLS_MODE": "build",
            "GRID_RUN_LABEL": "source-lock-test",
            "GRID_RESOURCE_NAMESPACE": "source-lock-test",
            "EXPECTED_API_SERVER": "https://api.invalid:6443",
            "KUBE_CONTEXT": "must-not-be-used",
            "CLUSTER_ENV": "ephemeral-dev",
            "RESOURCE_PROFILE": "reduced-dev",
            "ALLOW_INSECURE_DEV_API": "true",
            "SOURCE_LOCKS_MD": str(markdown),
            "SOURCE_LOCKS_FILE": str(lock),
            "SERVING_POOLS_EVIDENCE_DIR": str(evidence),
            "SIMULATOR_SOURCE_REVISION": revisions["llm-d-sim"],
            "EPP_SOURCE_REVISION": "f" * 40,
            "EXTPROC_SOURCE_REVISION": revisions["extproc"],
            "GITOPS_SOURCE_REVISION": revisions["gitops"],
            "GRID_GATEWAY_SOURCE_REVISION": revisions["grid"],
            "ODH_GRID_OPERATOR_SOURCE_REVISION": revisions["grid-operator"],
            "MAAS_SOURCE_REVISION": revisions["maas"],
            "EPP_IMAGE": f"registry.invalid/epp@sha256:{'a' * 64}",
            "EXTPROC_IMAGE": f"registry.invalid/extproc@sha256:{'b' * 64}",
        }
    )
    result = subprocess.run(
        ["bash", str(helper)],
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode == 0:
        raise SystemExit("helper accepted an EPP source revision that differs from the lock")
    if "declared epp source revision does not match SOURCE_LOCKS_FILE" not in result.stderr:
        raise SystemExit(f"helper failed for an unexpected reason: {result.stderr[-1000:]}")
    if sentinel.exists():
        raise SystemExit("helper invoked oc before rejecting the source-lock mismatch")
    if evidence.exists():
        raise SystemExit("helper created phase evidence before rejecting the source-lock mismatch")
    print("serving-pool helper rejects stale source pins before evidence creation or cluster access: PASS")
PY
