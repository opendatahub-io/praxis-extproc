#!/usr/bin/env python3
from pathlib import Path
import sys


source_path = Path(__file__).parents[2] / "xtask/src/e2e.rs"
source = source_path.read_text(encoding="utf-8")

validation_start = source.index("fn validate_signal_transition_inputs(")
validation_end = source.index("\nfn validate_epp_scoring_inputs(", validation_start)
validation = source[validation_start:validation_end]
baseline_only_validation = validation[: validation.index("\n    for (is_missing, name) in [")]
if "if !args.require_signal_transition" not in baseline_only_validation:
    print("baseline-only mode must have an explicit input-validation branch", file=sys.stderr)
    raise SystemExit(1)
if "validate_epp_scoring_inputs(args, missing);" not in baseline_only_validation:
    print("baseline-only mode must require authenticated EPP score inputs", file=sys.stderr)
    raise SystemExit(1)

scoring_start = source.index("fn check_grid_scoring_contract(")
scoring_end = source.index("\nfn grid_routing_map_status_entries(", scoring_start)
scoring = source[scoring_start:scoring_end]
if "args.mode != Mode::Run" not in scoring:
    print("live score qualification must run for Mode::Run", file=sys.stderr)
    raise SystemExit(1)
if "if !args.require_signal_transition" in scoring:
    print("baseline-only mode must not bypass live score qualification", file=sys.stderr)
    raise SystemExit(1)
if "sample_epp_metrics(args, report, evidence_dir, phase)?" not in scoring:
    print("baseline qualification must sample authenticated EPP metrics", file=sys.stderr)
    raise SystemExit(1)
if "verify_grid_queue_score_contract(" not in scoring:
    print("baseline qualification must compare the live overlay with EPP samples", file=sys.stderr)
    raise SystemExit(1)
if "check_grid_providers_current(args, report, evidence_dir, phase)?" not in scoring:
    print("baseline qualification must verify current provider and metrics conditions", file=sys.stderr)
    raise SystemExit(1)
if "candidate_scores.len() == 2" not in source or "candidate_queue_scores.len() == 2" not in source:
    print("baseline qualification must require exactly two scored candidates", file=sys.stderr)
    raise SystemExit(1)
if "== [0, 1].into_iter().collect()" not in source:
    print("candidate ranks must be contiguous and score ordered", file=sys.stderr)
    raise SystemExit(1)
if "candidate_count == 2" not in source or "candidate_identity_count == 2" not in source:
    print("score qualification must require exactly two unique overlay candidates", file=sys.stderr)
    raise SystemExit(1)

provider_start = source.index("fn check_grid_providers_current(")
provider_end = source.index("\nfn check_grid_scoring_contract(", provider_start)
provider_check = source[provider_start:provider_end]
if 'condition_current("Ready")' not in provider_check or 'condition_current("MetricsAvailable")' not in provider_check:
    print("both providers must have current Ready and MetricsAvailable conditions", file=sys.stderr)
    raise SystemExit(1)
for requirement in (
    'provider["metadata"]["generation"]',
    'provider["status"]["observedGeneration"]',
    'condition["observedGeneration"].as_u64() == generation',
    'provider["status"]["phase"].as_str() == Some("Available")',
):
    if requirement not in provider_check:
        print("provider availability checks must be current-generation", file=sys.stderr)
        raise SystemExit(1)

sample_start = source.index("fn sample_epp_metrics(")
sample_end = source.index("\nfn ", sample_start + 4)
metric_sample = source[sample_start:sample_end]
if '"--metrics-service-account"' not in metric_sample or '"--metrics-service-account-namespace"' not in metric_sample:
    print("EPP metric samples must use the dedicated authenticated scraper identity", file=sys.stderr)
    raise SystemExit(1)

route_hook = Path(__file__).with_name("validate-pool-route-contract.py").read_text(encoding="utf-8")
for requirement in (
    "validate_intelligent_provider_hops(",
    '"intelligentRoute"',
    '"mutual_tls"',
    "providerHopEndpoints",
    '"provider_route"',
    '"x-ai-demo-provider-gateway"',
    '"x-ai-inference-provider"',
):
    if requirement not in route_hook:
        print(f"the score-based provider-hop attribution contract is missing {requirement}", file=sys.stderr)
        raise SystemExit(1)
if "x-grid-site" in route_hook or "x-grid-backend" in route_hook:
    print("the selected score-based router must not be checked against gridSiteRoute-only attribution", file=sys.stderr)
    raise SystemExit(1)

run_start = source.index("fn run_probes(")
run_end = source.index("\nfn ", run_start + 4)
run_probes = source[run_start:run_end]
preflight = run_probes.index("preflight(args, report, evidence_dir)?;")
traffic_gate = run_probes.index('check.status.as_str(), "blocked" | "fail"')
baseline_only_branch = run_probes.index("if !args.require_signal_transition")
first_probe = run_probes.index('run_probe(args, report, evidence_dir, "baseline")?')
if not preflight < traffic_gate < baseline_only_branch < first_probe:
    print("baseline-only traffic must remain behind failed/blocked preflight checks", file=sys.stderr)
    raise SystemExit(1)
baseline_gates = run_probes[baseline_only_branch:first_probe]
if "grid_revision_is_distributed(&baseline_grid)" not in baseline_gates:
    print("baseline traffic must require a current Distributed overlay", file=sys.stderr)
    raise SystemExit(1)
if 'wait_for_gateway_revision(args, report, evidence_dir, &baseline_grid, "baseline")?' not in baseline_gates:
    print("baseline traffic must wait for Gateway consumption of the exact overlay revision", file=sys.stderr)
    raise SystemExit(1)
if baseline_gates.index('wait_for_gateway_revision(args, report, evidence_dir, &baseline_grid, "baseline")?') > baseline_gates.index(
    "check_baseline_score_overlay_contract(args, report, evidence_dir, &baseline_grid, \"baseline\")?"
):
    print("baseline EPP samples must be checked against the exact Gateway-consumed overlay", file=sys.stderr)
    raise SystemExit(1)
baseline_score_start = source.index("fn check_baseline_score_overlay_contract(")
baseline_score_end = source.index("\nfn wait_for_grid_revision_change(", baseline_score_start)
baseline_score = source[baseline_score_start:baseline_score_end]
for requirement in (
    "check_grid_providers_current(args, report, evidence_dir, phase)?",
    "sample_epp_metrics(args, report, evidence_dir, phase)?",
    'observe_grid_revision(args, report, evidence_dir, &format!("{phase}-score-overlay"))?',
    "same_distributed_grid_revision(grid, &observed_grid)",
    "verify_grid_queue_score_contract(",
):
    if requirement not in baseline_score:
        print(f"final baseline score/overlay gate is missing {requirement}", file=sys.stderr)
        raise SystemExit(1)
if not (
    baseline_score.index("sample_epp_metrics(args, report, evidence_dir, phase)?")
    < baseline_score.index('observe_grid_revision(args, report, evidence_dir, &format!("{phase}-score-overlay"))?')
    < baseline_score.index("verify_grid_queue_score_contract(")
):
    print("final EPP samples must be compared with the overlay read after the scrape", file=sys.stderr)
    raise SystemExit(1)

revision_start = source.index("fn observe_grid_revision(")
revision_end = source.index("\nfn overlay_candidate_state(", revision_start)
revision_observation = source[revision_start:revision_end]
if "observed_generation == network_generation" not in revision_observation:
    print("baseline overlay observation must require current GridNetwork generation", file=sys.stderr)
    raise SystemExit(1)

print("baseline-only EPP metric, queue-score, policy, and no-traffic-on-failed-preflight contract: PASS")
