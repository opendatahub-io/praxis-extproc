#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOOK="$SCRIPT_DIR/extproc-failure-closed-hook.sh"

bash -n "$HOOK"
python3 - "$HOOK" <<'PY'
import pathlib
import sys

source = pathlib.Path(sys.argv[1]).read_text()
required = {
    "explicit context": 'command oc --context="$CONTEXT"',
    "exact API guard": '[[ "$CURRENT_SERVER" == "$EXPECTED_API_SERVER" ]]',
    "ephemeral-only API TLS bypass": '[[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]]',
    "run-owned Deployment gate": '.metadata.labels["praxis.fast/qualification-run"]==$run',
    "run-owned ExtProc Service gate": '.metadata.labels["praxis.fast/qualification-run"]==$run',
    "task EnvoyFilter gate": 'oc get envoyfilter.networking.istio.io "$ENVOYFILTER_NAME"',
    "fail-closed filter contract": 'failure_mode_allow==false',
    "filter targets the run Gateway": '.spec.workloadSelector.labels["gateway.networking.k8s.io/gateway-name"]==$gateway',
    "accepted task HTTPRoute": 'type=="Accepted" and .status=="True"',
    "resolved task HTTPRoute": 'type=="ResolvedRefs" and .status=="True"',
    "run-owned Gateway readiness": 'oc get gateway "$GATEWAY_NAME" -n "$NAMESPACE" -o json',
    "run-owned backend Service": 'oc get service "$BACKEND_NAME" -n "$BACKEND_NAMESPACE" -o json',
    "ExtProc endpoint ready before injection": 'READY_ENDPOINTS_BEFORE" == 1',
    "only task ExtProc scale-down": 'oc scale "deployment/$DEPLOYMENT_NAME" -n "$NAMESPACE" --replicas=0',
    "restoration trap": 'trap cleanup EXIT',
    "unique correlated request": '--header "x-request-id: $REQUEST_ID"',
    "application TLS verification": '--cacert "$GATEWAY_CA_FILE"',
    "optional TLS-preserving connect mapping": 'GATEWAY_CONNECT_TO="${GRID_LLMD_GATEWAY_CONNECT_TO:-}"',
    "connect mapping preserves route hostname": '[[ "$CONNECT_URL_HOST" == "$URL_HOST" && "$CONNECT_URL_PORT" == "$URL_PORT" ]]',
    "validated connect mapping is passed only to curl": 'CONNECT_ARGS=(--connect-to "${CONNECT_URL_HOST}:${CONNECT_URL_PORT}:${CONNECT_HOST}:${CONNECT_PORT}")',
    "curl uses the validated connect mapping": '"${CONNECT_ARGS[@]}" --cacert "$GATEWAY_CA_FILE"',
    "connect target is recorded without credentials": 'gatewayConnectTo:(if $gatewayConnectTo=="" then null else $gatewayConnectTo end)',
    "ExtProc failure counter evidence": 'extprocFilterFailures',
    "backend upstream counter evidence": 'backendRequests',
    "Envoy cluster endpoint counters": '/clusters?format=json',
    "per-host backend request total": '"rq_total"',
    "5xx and zero backend assertion": '[[ "$HTTP_STATUS" -ge 500 && "$EXTPROC_FAILURE_OBSERVED" == true && "$BACKEND_REQUESTS" == 0 ]]',
    "required JSON schema": 'praxis-extproc.grid-llmd-failure-closed/v1',
    "runtime restoration check": 'RESTORED_DEPLOYMENT',
}
missing = [label for label, fragment in required.items() if fragment not in source]
forbidden = [fragment for fragment in ("oc delete", "oc patch", "oc apply", "oc replace") if fragment in source]
if missing:
    raise SystemExit("failure-closed hook contract is missing: " + ", ".join(missing))
if forbidden:
    raise SystemExit("failure-closed hook must not mutate filters or shared resources: " + ", ".join(forbidden))
print("ExtProc failure-closed task-ownership, TLS, correlation, counters, and restoration contract: PASS")
PY
