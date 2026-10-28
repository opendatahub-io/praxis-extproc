#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - "$SCRIPT_DIR/deploy-extproc-filter.sh" <<'PY'
from pathlib import Path
import sys

source = Path(sys.argv[1]).read_text(encoding="utf-8")
runner = Path(sys.argv[1]).parents[2] / "xtask/src/e2e.rs"
runner_source = runner.read_text(encoding="utf-8")
required = (
    'Modes: plan, render, install, verify, collect.',
    'SOURCE_LOCKS_FILE and GRID_EXTPROC_IMAGE_PROVENANCE_FILE are required',
    '.components.extproc.resolved_sha==$sha and .components.extproc.resolved_worktree_clean==true',
    '.verified==true and .source_sha==$sha and .image==$image and .oci_revision_label==$sha',
    'praxis.fast/qualification-run',
    'response_set:',
    'name: X-Praxis-ExtProc-Response',
    '- filter: e2e_body_oracle',
    'failure_mode_allow: false',
    'request_body_mode: BUFFERED',
    'name: envoy.filters.http.ext_proc',
    'workloadSelector:',
    'gateway.networking.k8s.io/gateway-name: "$GATEWAY_NAME"',
    'oc apply --dry-run=server -f "$EVIDENCE_DIR/extproc-resources.yaml"',
    'pod_deadline=$((SECONDS + 60))',
    '.status.observedGeneration==.metadata.generation',
    '(.status.updatedReplicas//0)==1 and (.status.replicas//0)==1',
    '(.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1',
    'all($pods[];.metadata.deletionTimestamp==null and .phase=="Running"',
    'all($pods[].containers[];.ready==true)',
    '.image==$image and (.imageID|endswith($digest)) and .ready==true',
    'extprocRuntime:$runtime',
    'pilot-agent request GET config_dump',
    '.workloadSelector.labels["gateway.networking.k8s.io/gateway-name"]==$gateway and any(.patches[];.applyTo=="HTTP_FILTER" and .filterName=="envoy.filters.http.ext_proc")',
    '.extprocFilterCount>0 and .extprocClusterPresent==true',
    'inferenceTrafficSent:false',
)
missing = [item for item in required if item not in source]
if missing:
    print("ExtProc deployment contract is incomplete: " + repr(missing), file=sys.stderr)
    raise SystemExit(1)

render = source.index('if [[ "$MODE" == render ]]; then')
dry_run = source.index('oc apply --dry-run=server -f "$EVIDENCE_DIR/extproc-resources.yaml"')
install = source.index('oc apply -f "$EVIDENCE_DIR/extproc-resources.yaml"')
if not dry_run < render < install:
    print("render mode must server-dry-run all resources and exit before mutation", file=sys.stderr)
    raise SystemExit(1)

readiness_start = source.index("CURRENT_PHASE=runtime-readiness")
wait_start = source.index("pod_deadline=$((SECONDS + 60))", readiness_start)
wait_end = source.index('oc get service "$SERVICE_NAME"', wait_start)
readiness_flow = source[readiness_start:wait_end]
poll_start = readiness_flow.index("while (( SECONDS < pod_deadline ))")
poll_end = readiness_flow.index("  sleep 2", poll_start)
poll_body = readiness_flow[poll_start:poll_end]
if "oc get deployment \"$DEPLOYMENT_NAME\" -n \"$NAMESPACE\" -o json >\"$EVIDENCE_DIR/deployment.json\"" not in poll_body:
    print("ExtProc rollout polling must refresh the Deployment status during the bounded wait", file=sys.stderr)
    raise SystemExit(1)
poll_requirements = (
    ".status.observedGeneration==.metadata.generation",
    "(.status.updatedReplicas//0)==1 and (.status.replicas//0)==1",
    "(.status.readyReplicas//0)==1 and (.status.availableReplicas//0)==1",
    "($pods|length)==1",
    ".metadata.deletionTimestamp==null and .phase==\"Running\"",
    "all($pods[].containers[];.ready==true)",
    ".image==$image and (.imageID|endswith($digest)) and .ready==true",
)
if any(requirement not in poll_body for requirement in poll_requirements):
    print("ExtProc bounded rollout wait must require a fully converged single Ready Pod with the pinned runtime digest", file=sys.stderr)
    raise SystemExit(1)
before_wait = readiness_flow[: wait_start - readiness_start]
if "status.readyReplicas" in before_wait or "status.availableReplicas" in before_wait:
    print("transient Deployment availability must be handled by the bounded rollout wait", file=sys.stderr)
    raise SystemExit(1)
runtime_record = readiness_flow.index("extproc_runtime_record=", readiness_flow.index("jq -e --arg image \"$IMAGE\" --arg digest \"$IMAGE_DIGEST\"", poll_end))
if runtime_record < poll_end or "imageId:$container.imageID" not in readiness_flow[runtime_record:]:
    print("ExtProc runtime identity must be recorded only after the rollout convergence gate passes", file=sys.stderr)
    raise SystemExit(1)

filter_start = source.index('kind: EnvoyFilter', source.index('cat >>"$EVIDENCE_DIR/extproc-resources.yaml"'))
filter = source[filter_start:]
if 'namespace: $NAMESPACE' not in filter or 'grid-llmd-ingress' in filter:
    print("the EnvoyFilter must remain namespace-scoped and use the configured task Gateway", file=sys.stderr)
    raise SystemExit(1)
if 'failure_mode_allow: true' in source:
    print("the ExtProc filter must fail closed", file=sys.stderr)
    raise SystemExit(1)
result_start = source.index('cat >"$EVIDENCE_DIR/FINAL-RESULT.md" <<EOF')
result_end = source.index("\nEOF", result_start)
if '`' in source[result_start:result_end]:
    print("the unquoted final-result heredoc must not contain shell command substitutions", file=sys.stderr)
    raise SystemExit(1)
if 'curl ' in source or 'inferenceTrafficSent:true' in source:
    print("deployment must not send inference traffic", file=sys.stderr)
    raise SystemExit(1)
if "- filter: e2e_body_oracle" not in source or "request_body_mode: BUFFERED" not in source:
    print("deployment must enable the ExtProc body oracle and Envoy request buffering", file=sys.stderr)
    raise SystemExit(1)
config_start = source.index("  extproc.yaml: |")
config_end = source.index("\nYAML\ncat >>", config_start)
extproc_config = source[config_start:config_end]
if "X-Praxis-ExtProc-Request" in extproc_config or "request_add:" in extproc_config:
    print("the Grid Gateway rejects the reserved X-Praxis-ExtProc-Request header; request mutation must not inject it", file=sys.stderr)
    raise SystemExit(1)
if "name: X-Praxis-ExtProc-Response" not in extproc_config or "x-qualification-request-sha256" not in source:
    print("the response marker and request-body digest oracle must remain configured", file=sys.stderr)
    raise SystemExit(1)
if "body_oracle_header_matches(&headers, header, &request_body_sha256)" not in runner_source:
    print("xtask must compare the response digest header with the exact sent request body digest", file=sys.stderr)
    raise SystemExit(1)
if "args.extproc_body_oracle.as_ref().is_some_and(|oracle| response_body.contains(oracle))" in runner_source:
    print("body oracle must not be treated as a response-body substring", file=sys.stderr)
    raise SystemExit(1)

print("ExtProc source/image provenance, run ownership, render gate, fail-closed configuration, and runtime proxy verification contract: PASS")
PY
