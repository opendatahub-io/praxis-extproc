#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-grid-providers.sh" "$SCRIPT_DIR/deploy-serving-pools.sh" "$SCRIPT_DIR/README.md" <<'PY'
from pathlib import Path
import json
import subprocess
import sys
import tempfile

providers = Path(sys.argv[1]).read_text(encoding="utf-8")
serving = Path(sys.argv[2]).read_text(encoding="utf-8")
readme = Path(sys.argv[3]).read_text(encoding="utf-8")
score_checker = Path(sys.argv[1]).with_name("verify-grid-queue-scores.py")

required = (
    'oc get crds -o json | jq',
    'select(.spec.names.kind=="InferenceProvider" and .spec.names.plural=="inferenceproviders"',
    'expected exactly one downstream ODH InferenceProvider CRD',
    'GRID_OPERATOR_IMAGE, EPP_IMAGE, and SIMULATOR_IMAGE must use immutable sha256 digests',
    'GRID_OPERATOR_DEPLOYMENT_NAME',
    '(.imageID|endswith($digest))',
    'resolve_inferencepool_name',
    'METRICS_SCRAPER_SERVICE_ACCOUNT',
    'oc create token "$METRICS_SCRAPER_SERVICE_ACCOUNT"',
    'curl --config -',
    'auth:{type:"serviceAccountToken",allowPlaintext:$allowPlaintext}',
    'authTokenRecorded:false',
    'pool_name="$expected_pool_name"',
    'poolLabel:{name:$poolName}',
    'strict runs must use authenticated HTTPS metrics',
    'queueCapacity:$queueCapacity',
    'staleMetricsSeconds:$staleMetricsSeconds',
    'metricsConfig:{endpoint:$metrics',
    'path:"/metrics"',
    'scrapeUrl:("http://"+$host+"/metrics")',
    'any(.status.matchingSites[]?;.==$site)',
    'current-generation Available for GridSite/',
    'capture_current_provider_metrics',
    'pool_name="$(resolve_inferencepool_name "$pool")"',
    'verify_gridnetwork_scoring_config',
    'verify_overlay_queue_scores',
    '.score_breakdown.queue_depth',
    '.spec.selectionPolicy.mode=="deterministic"',
    'capture_metrics_contract "$POOL_A" "$metrics_pool_a"',
    'capture_metrics_contract "$POOL_B" "$metrics_pool_b"',
    'oc apply --server-side --dry-run=server',
    'oc apply --server-side --field-manager=grid-llmd-e2e',
    '.status.observedGeneration==$g and .status.phase=="Available"',
    '.provenance.producer=="grid-operator"',
    '.content_digest.value==$digest',
    'configMapResourceVersion!=""',
    'provider-manifests.sha256',
    'auth.properties.type.enum|index("serviceAccountToken")',
    'scraperCanGetMetrics',
    'Praxis ExtProc checkout:',
    'sha256sum "$path"',
    'readlink -- "$path"',
    'LC_ALL=C sort -k2',
    'write_source_lock',
    'cleanup_task_providers',
    'capture_failure_state || true',
    'all(.[];(.address|type=="string") and (.address|contains(".svc.cluster.local:")) and .transport.mode=="plaintext")',
)
missing = [item for item in required if item not in providers]
if missing:
    raise SystemExit("provider registration contract is missing: " + repr(missing))
if 'write_invocation_tmp' in providers:
    raise SystemExit("provider helper contains the previous no-op invocation placeholder")
score_start = providers.index("verify_overlay_queue_scores() {")
score_end = providers.index("\nwait_for_grid_overlay() {", score_start)
score_gate = providers[score_start:score_end]
for item in (
    "jq -r '.metadata.name // empty' \"$EVIDENCE_DIR/$POOL_A-inferencepool.json\"",
    "jq -r '.metadata.name // empty' \"$EVIDENCE_DIR/$POOL_B-inferencepool.json\"",
    '--pool-a "$pool_a_name" --pool-b "$pool_b_name"',
):
    if item not in score_gate:
        raise SystemExit(f"score verifier does not use the freshly resolved InferencePool name: {item!r}")
if '$POOL_A-metrics-service.json")' in score_gate or '$POOL_B-metrics-service.json")' in score_gate:
    raise SystemExit("score verifier reads a nonexistent poolName from the EPP service inventory")

overlay = providers[providers.index("wait_for_grid_overlay() {"):providers.index("verify_grid_operator_runtime() {")]
for item in (
    '(.routingMapStatus|length)==1',
    '.renderedRevision!=""',
    '.distributedRevision==.renderedRevision',
    '.overlay.candidates[]?',
):
    if item not in overlay:
        raise SystemExit(f"distributed overlay gate is missing {item!r}")
if '([$taskCandidates[].cluster]|unique|sort)==([$a,$b]|sort)' not in overlay:
    raise SystemExit("distributed overlay gate does not require both named pool providers")
if '(.score_breakdown.queue_depth|type)=="number"' not in overlay:
    raise SystemExit("distributed overlay gate does not require queue-depth score values")
if 'verify_overlay_queue_scores "$envelope_file"' not in overlay:
    raise SystemExit("distributed overlay gate does not compare score values with live EPP samples")

ready_start = providers.index("wait_for_provider_ready() {")
ready_start = providers.index("      .metadata.generation as $g |", ready_start)
ready_end = providers.index("\n    ' \"$EVIDENCE_DIR/$provider-live.json\"", ready_start)
ready_filter = providers[ready_start:ready_end]
ready_object = {
    "metadata": {"generation": 2},
    "status": {
        "observedGeneration": 2,
        "phase": "Available",
        "matchingSites": ["grid-llmd-site"],
    },
}
def run_ready_gate(obj):
    return subprocess.run(
        ["jq", "-e", "--arg", "site", "grid-llmd-site", ready_filter],
        input=json.dumps(obj), text=True, capture_output=True, check=False,
    )

if run_ready_gate(ready_object).returncode != 0:
    raise SystemExit("provider configuration gate rejected current-generation Available with a matching site and no condition list")
stale_generation = json.loads(json.dumps(ready_object))
stale_generation["status"]["observedGeneration"] = 1
if run_ready_gate(stale_generation).returncode == 0:
    raise SystemExit("provider configuration gate accepted stale observedGeneration")
missing_site = json.loads(json.dumps(ready_object))
missing_site["status"]["matchingSites"] = []
if run_ready_gate(missing_site).returncode == 0:
    raise SystemExit("provider configuration gate accepted an Available provider without the requested site")
negative_metric_condition = json.loads(json.dumps(ready_object))
negative_metric_condition["status"]["conditions"] = [
    {"type": "MetricsAvailable", "status": "False", "observedGeneration": 2}
]
if run_ready_gate(negative_metric_condition).returncode == 0:
    raise SystemExit("provider configuration gate accepted a current-generation negative metrics condition")
stale_negative_metric_condition = json.loads(json.dumps(ready_object))
stale_negative_metric_condition["status"]["conditions"] = [
    {"type": "MetricsAvailable", "status": "False", "observedGeneration": 1}
]
if run_ready_gate(stale_negative_metric_condition).returncode != 0:
    raise SystemExit("provider configuration gate rejected a stale negative condition despite independent live metric validation")

epp_metrics_fixture = """llm_d_epp_average_queue_size{name=\"pool-a-inference-pool\"} 0
llm_d_epp_average_kv_cache_utilization{name=\"pool-a-inference-pool\"} 0
llm_d_epp_ready_endpoints{name=\"pool-a-inference-pool\"} 1
llm_d_epp_average_queue_size{name=\"pool-b-inference-pool\"} 0
"""
expected_pool = "pool-a-inference-pool"
selected = [
    line for line in epp_metrics_fixture.splitlines()
    if any(metric in line for metric in (
        "llm_d_epp_average_queue_size",
        "llm_d_epp_average_kv_cache_utilization",
        "llm_d_epp_ready_endpoints",
    )) and f'name="{expected_pool}"' in line
]
if len(selected) != 3 or any('name="pool-b-inference-pool"' in line for line in selected):
    raise SystemExit("pool metrics contract does not select the exact LLMInferenceService-owned InferencePool label")

endpoint_filter = 'length==2 and ([.[].cluster]|unique|length)==2 and all(.[];(.address|type=="string") and (.address|contains(".svc.cluster.local:")) and .transport.mode=="plaintext")'
endpoint_fixture = [
    {"cluster": "provider-pool-a", "address": "pool-a-gw.ns.svc.cluster.local:8080", "transport": {"mode": "plaintext"}},
    {"cluster": "provider-pool-b", "address": "pool-b-gw.ns.svc.cluster.local:8080", "transport": {"mode": "plaintext"}},
]
result = subprocess.run(
    ["jq", "-e", endpoint_filter],
    input=json.dumps(endpoint_fixture),
    text=True,
    capture_output=True,
    check=False,
)
if result.returncode != 0:
    raise SystemExit("task Gateway endpoint inventory jq filter rejects two distinct Service endpoints")
invalid_fixture = [dict(endpoint) for endpoint in endpoint_fixture]
invalid_fixture[1]["address"] = "127.0.0.1:8080"
invalid_result = subprocess.run(
    ["jq", "-e", endpoint_filter],
    input=json.dumps(invalid_fixture),
    text=True,
    capture_output=True,
    check=False,
)
if invalid_result.returncode == 0:
    raise SystemExit("task Gateway endpoint inventory jq filter accepted a non-Service address")

def run_score_contract(queue_a, queue_b, score_a, score_b, rank_a, rank_b, queue_score_a=None, queue_score_b=None):
    with tempfile.TemporaryDirectory(prefix="grid-queue-score-test-") as directory:
        root = Path(directory)
        overlay = {
            "overlay": {
                "candidates": [
                    {"kind": "inference_model", "name": "grid-test-model", "cluster": "provider-pool-a", "score": score_a, "rank": rank_a, "score_breakdown": {"queue_depth": queue_score_a if queue_score_a is not None else 1 * (1 - min(queue_a / 4, 1))}},
                    {"kind": "inference_model", "name": "grid-test-model", "cluster": "provider-pool-b", "score": score_b, "rank": rank_b, "score_breakdown": {"queue_depth": queue_score_b if queue_score_b is not None else 1 * (1 - min(queue_b / 4, 1))}},
                ]
            }
        }
        overlay_path = root / "overlay.json"
        overlay_path.write_text(json.dumps(overlay), encoding="utf-8")
        (root / "pool-a.prom").write_text(f'llm_d_epp_average_queue_size{{name="pool-a-inference-pool"}} {queue_a}\n', encoding="utf-8")
        (root / "pool-b.prom").write_text(f'llm_d_epp_average_queue_size{{name="pool-b-inference-pool"}} {queue_b}\n', encoding="utf-8")
        return subprocess.run(
            [
                sys.executable, str(score_checker), "--overlay", str(overlay_path),
                "--queue-metric", "llm_d_epp_average_queue_size", "--queue-capacity", "4",
                "--model", "grid-test-model", "--provider-a", "provider-pool-a",
                "--provider-b", "provider-pool-b", "--pool-a", "pool-a-inference-pool",
                "--pool-b", "pool-b-inference-pool", "--pool-a-samples", str(root / "pool-a.prom"),
                "--pool-b-samples", str(root / "pool-b.prom"),
            ],
            text=True, capture_output=True, check=False,
        )

baseline_score_contract = run_score_contract(0, 0, 1, 1, 0, 1)
if baseline_score_contract.returncode != 0:
    raise SystemExit("queue-score checker rejected matching baseline EPP/overlay samples: " + baseline_score_contract.stderr)
pressure_score_contract = run_score_contract(4, 0, 0, 1, 1, 0)
if pressure_score_contract.returncode != 0:
    raise SystemExit("queue-score checker rejected a measured pressure score/rank transition: " + pressure_score_contract.stderr)
wrong_score_contract = run_score_contract(4, 0, 0, 1, 1, 0, queue_score_a=0.75)
if wrong_score_contract.returncode == 0:
    raise SystemExit("queue-score checker accepted a score that disagrees with the EPP pressure sample")
wrong_rank_contract = run_score_contract(4, 0, 1, 0, 1, 0)
if wrong_rank_contract.returncode == 0:
    raise SystemExit("queue-score checker accepted a rank order that disagrees with the higher provider score")

metrics_service_fixture = {
    "endpoint": "http://pool-a-epp-service.ns.svc.cluster.local:9090",
    "path": "/metrics",
    "scrapeUrl": "http://pool-a-epp-service.ns.svc.cluster.local:9090/metrics",
}
from urllib.parse import urlsplit
if urlsplit(metrics_service_fixture["endpoint"]).path or metrics_service_fixture["path"] != "/metrics":
    raise SystemExit("provider metrics endpoint must be the service base URL with the scrape path configured separately")
if urlsplit(metrics_service_fixture["scrapeUrl"]).path != metrics_service_fixture["path"]:
    raise SystemExit("provider metrics scrape URL must resolve to exactly /metrics")

apply_start = providers.index('apply|verify)\n', providers.index('case "$MODE" in\n  apply|verify)'))
apply_end = providers.index('  cleanup)', apply_start)
apply_case = providers[apply_start:apply_end]
ordered = ("prepare_provider_inputs", "write_source_lock", "apply_or_verify_providers", "wait_for_provider_ready", "wait_for_grid_overlay")
positions = [apply_case.index(item) if item in apply_case else -1 for item in ordered]
if any(position < 0 for position in positions) or positions != sorted(positions):
    raise SystemExit("provider registration/readiness/overlay phases are not ordered safely")

serving_case = serving.index('case "$MODE" in\n  collect)')
serving_apply_start = serving.index('  apply)\n', serving_case)
serving_apply_end = serving.index('  verify)\n', serving_apply_start)
apply = serving[serving_apply_start:serving_apply_end]
gate = apply.index("verify_post_transport_platform_gate")
pool_a = apply.index('apply_one_pool "$POOL_A"')
if gate > pool_a:
    raise SystemExit("serving-pool apply bypasses the post-DSCI readiness gate")

serving_position = readme.find("SERVING_POOLS_MODE=apply")
provider_position = readme.find("GRID_PROVIDERS_MODE=apply")
qualification_position = readme.find("cargo run --locked -p xtask -- e2e grid-llmd")
if provider_position < 0 or not (serving_position < provider_position < qualification_position):
    raise SystemExit("runbook must apply both serving pools, register providers, then run inference qualification")

print("downstream CRD discovery, digest pins, owned provider resources, EPP signal metrics, and current overlay qualification: PASS")
PY
