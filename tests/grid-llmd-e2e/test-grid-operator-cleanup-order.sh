#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/cleanup-grid-operator.sh" <<'PY'
from pathlib import Path
import sys
import subprocess

source = Path(sys.argv[1]).read_text(encoding="utf-8")
ready = source.find("await_operator_ready immediately-before-network-delete")
network_delete = source.find('delete_resource_with_uid "/apis/grid.praxis.fast/v1alpha1/gridnetworks/$NETWORK_NAME" "$NETWORK_UID"')
network_wait = source.find("deadline=$((SECONDS + ${WAIT_TIMEOUT%s}))", network_delete)
watch_loop = source.find("while ((SECONDS < deadline)); do", network_delete)
withdrawal = source.find('published GridNetwork SWIM withdrawal; removed finalizer', network_delete)
site_delete = source.find('delete_resource_with_uid "/apis/grid.praxis.fast/v1alpha1/gridsites/$SITE_NAME" "$SITE_UID"', withdrawal)
release_delete = source.find('helm uninstall "$RELEASE_NAME"', site_delete)
namespace_delete = source.find('delete_resource_with_uid "/api/v1/namespaces/$RELEASE_NAMESPACE" "$NAMESPACE_UID"', release_delete)
positions = [ready, network_delete, network_wait, watch_loop, withdrawal, site_delete, release_delete, namespace_delete]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("Grid cleanup must await operator-confirmed network withdrawal before deleting the site, Helm release, or namespace")

required = (
    "--ignore-not-found -o name",
    "could not verify absence",
    "could not list providers; absence is not established",
    "'{apiVersion:\"v1\",kind:\"DeleteOptions\",preconditions:{uid:$uid}}'",
    "$DEPLOYMENT_UID",
    "$OPERATOR_IMAGE",
    "providers-blocking-cleanup.json",
    "operator was not still Ready after GridNetwork deletion",
    "published GridNetwork SWIM withdrawal; removed finalizer",
    "deadline=$((SECONDS + ${WAIT_TIMEOUT%s}))",
    "operator-readiness-attempts.jsonl",
    "MODE\" == resume",
    "GridNetwork still exists; resume mode will not submit a second delete",
    "prior UID-preconditioned request",
    "gsub(/\\033\\[[0-9;]*m/",
    '"$EVIDENCE_DIR/gridnetwork-delete-options.json"',
    '"$EVIDENCE_DIR/gridsite-delete-options.json"',
    '"$EVIDENCE_DIR/operator-namespace-delete-options.json"',
    'prior_delete_options="$EVIDENCE_DIR/prior-attempt/gridnetwork-delete-options.json"',
    'prior_delete_options="$EVIDENCE_DIR/prior-attempt/delete-options.json"',
)
missing = [item for item in required if item not in source]
if missing:
    raise SystemExit(f"cleanup ownership/runtime guards are missing: {missing}")

if any(fragment in source for fragment in ("oc patch", "kubectl patch", "remove-finalizer", "finalizers/remove")):
    raise SystemExit("cleanup must let the operator remove the GridNetwork finalizer")

if '>"$EVIDENCE_DIR/delete-options.json"' in source or '--data-binary "@$EVIDENCE_DIR/delete-options.json"' in source:
    raise SystemExit("each destructive request must retain its own UID-preconditioned DeleteOptions")

helper = source[source.index("require_absent() {"):source.index("\napi_ready() {")]
for result, output, expected in ((0, "", 0), (0, "gridnetwork/task", 1), (1, "", 1)):
    script = ("fail() { exit 1; }\n"
              f"oc() {{ printf '%s' '{output}'; return {result}; }}\n"
              + helper + '\nrequire_absent gridnetwork task ""\n')
    completed = subprocess.run(["bash", "-c", script], capture_output=True, text=True)
    if completed.returncode != expected:
        raise SystemExit(f"absence guard accepted an API failure or existing resource: {result=}, {output=}")

print("GridNetwork withdrawal is verified before operator uninstall: PASS")
PY
