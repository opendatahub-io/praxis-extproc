#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
python3 - "$SCRIPT_DIR/install-kind-kserve.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
required = {
    "helper directory initialized before referenced verifier": 'SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"',
    "explicit kubeconfig and context": 'kubectl --kubeconfig="$KUBECONFIG_FILE" --context="$CONTEXT"',
    "explicit Helm kubeconfig and context": 'helm --kubeconfig "$KUBECONFIG_FILE" --kube-context "$CONTEXT"',
    "expected API identity": 'actual_server" == "$EXPECTED_API_SERVER"',
    "clean detached source": 'git -C "$SOURCE_DIR" status --porcelain --untracked-files=all',
    "source archive check": 'actual_context_sha" == "$SOURCE_CONTEXT_SHA"',
    "image provenance check": 'controller image provenance does not match',
    "build metadata and architecture check": '.build_platform=="linux/amd64"',
    "build tag is derived from image metadata": 'resolve_kind_image_tag.py',
    "image tag is not a mandatory duplicate input": '[--image-tag TAG]',
    "KServe chart versions come from locked source metadata": 'resolve_kind_kserve_chart_metadata.py',
    "KServe CRD package uses source chart version": 'kserve-llmisvc-crd-$crd_chart_version.tgz',
    "KServe controller package uses source chart version": 'kserve-llmisvc-resources-$resources_chart_version.tgz',
    "KServe runtime package uses source chart version": 'kserve-runtime-configs-$runtime_chart_version.tgz',
    "source chart metadata recorded": 'chart-source-metadata.json',
    "runtime configuration toggles use source chart defaults": '--set kserve.llmisvcConfigs.enabled=true',
    "serving certificate before controller rollout": 'wait --for=condition=Ready certificate/llmisvc-serving-cert',
    "controller rollout and pinned runtime digest": 'exactly one Ready Pod with the pinned runtime image digest',
    "bounded webhook convergence": 'webhook-convergence-polls.tsv',
    "webhook Pod and EndpointSlice contract": 'verify_kind_kserve_webhook_readiness.py',
    "live API served and stored gate": '.name=="v1alpha2" and .served==true and .storage==true',
    "webhook CA bundle gate": 'webhook CA bundle or service reference is missing',
    "automatic failure snapshots": 'failure-kserve-workloads.yaml',
    "no manually fabricated InferencePool": 'does not install a',
}
missing = [label for label, fragment in required.items() if fragment not in source]
if missing:
    raise SystemExit("missing KServe install guarantees: " + ", ".join(missing))
script_dir_assignment = source.index('SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"')
verifier_invocation = source.index('python3 "$SCRIPT_DIR/verify_kind_kserve_webhook_readiness.py"')
if script_dir_assignment > verifier_invocation:
    raise SystemExit("KServe helper directory must be initialized before the webhook verifier invocation")
tag_resolution = source.index('resolve_kind_image_tag.py')
first_cluster_read = source.index('actual_server="$(k config view')
if tag_resolution > first_cluster_read:
    raise SystemExit("KServe image tag must be verified before contacting or mutating the cluster")
if "kubectl patch" in source or "--set image.tag=latest" in source:
    raise SystemExit("installer contains a manual resource patch or mutable latest image")
if "--set kserve.version=" in source:
    raise SystemExit("installer overrides the source-locked runtime chart version")
if 'k rollout restart deployment/llmisvc-controller-manager' in source:
    raise SystemExit("installer forces a controller rollout that can leave an old webhook Pod terminating")
if not (Path(sys.argv[1]).parent / "verify_kind_kserve_webhook_readiness.py").is_file():
    raise SystemExit("KServe webhook readiness verifier is missing")

ordered = [
    'h upgrade --install kserve-llmisvc-crd',
    'h upgrade --install kserve-llmisvc-resources',
    'k wait --for=condition=Ready certificate/llmisvc-serving-cert',
    'k rollout status deployment/llmisvc-controller-manager',
    'verify_kind_kserve_webhook_readiness.py',
    'h upgrade --install kserve-runtime-configs',
]
positions = [source.index(fragment) for fragment in ordered]
if positions != sorted(positions):
    raise SystemExit("KServe CRD/controller/certificate/runtime installation order changed")
print("KServe install contract passed")
PY
python3 "$SCRIPT_DIR/test_resolve_kind_image_tag.py"
