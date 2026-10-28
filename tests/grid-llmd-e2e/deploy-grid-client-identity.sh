#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Create the run-owned Kubernetes identity used by the native Grid Gateway's
# Authorino TokenReview/SAR policy. A short-lived token is minted separately
# with `oc create token`; token bytes are never written to evidence.
set -Eeuo pipefail
umask 077

MODE="${GRID_CLIENT_IDENTITY_MODE:-plan}"
EXPECTED_API_SERVER="${EXPECTED_API_SERVER:-}"
KUBE_CONTEXT="${KUBE_CONTEXT:-}"
CLUSTER_ENV="${CLUSTER_ENV:-}"
ALLOW_INSECURE_DEV_API="${ALLOW_INSECURE_DEV_API:-false}"
NAMESPACE="${GRID_RESOURCE_NAMESPACE:-grid-llmd-e2e}"
RUN_LABEL="${GRID_RUN_LABEL:-}"
SA_NAME="${GRID_CLIENT_SERVICE_ACCOUNT:-grid-llmd-client}"
ROLE_NAME="${GRID_CLIENT_ROLE_NAME:-grid-llmd-client-inference-read}"
ROLE_BINDING_NAME="${GRID_CLIENT_ROLE_BINDING_NAME:-grid-llmd-client-inference-read}"
POOL_A="${GRID_LLMD_POOL_A_NAME:-pool-a}"
POOL_B="${GRID_LLMD_POOL_B_NAME:-pool-b}"
MODEL_NAME="${GRID_CLIENT_MODEL_NAME:-grid-test-model}"
EVIDENCE_DIR="${GRID_CLIENT_IDENTITY_EVIDENCE_DIR:-}"

oc() { command oc --context="$KUBE_CONTEXT" "$@"; }

if [[ "$MODE" == plan ]]; then
  cat <<'PLAN'
Plan: verify the task-owned namespace, render a run-labeled ServiceAccount and
least-privilege Role/RoleBinding, run server-side dry-run, then verify that the
identity can only get the two task LLMInferenceServices and authorize the exact
publisher model required by the current pool Gateway AuthPolicies. No token is
minted or recorded by this script.
PLAN
  exit 0
fi

for binary in oc jq; do
  command -v "$binary" >/dev/null || { printf '%s is required\n' "$binary" >&2; exit 2; }
done
[[ "$MODE" =~ ^(render|install|verify)$ ]] || { printf 'unsupported mode: %s\n' "$MODE" >&2; exit 2; }
[[ -n "$EXPECTED_API_SERVER" && -n "$KUBE_CONTEXT" && -n "$RUN_LABEL" ]] || {
  printf 'EXPECTED_API_SERVER, KUBE_CONTEXT, and GRID_RUN_LABEL are required\n' >&2
  exit 2
}
for name in "$NAMESPACE" "$SA_NAME" "$ROLE_NAME" "$ROLE_BINDING_NAME" "$POOL_A" "$POOL_B"; do
  [[ "$name" =~ ^[a-z0-9]([-a-z0-9]*[a-z0-9])?$ ]] || { printf 'unsafe Kubernetes name: %s\n' "$name" >&2; exit 2; }
done
[[ "$MODEL_NAME" =~ ^[a-z0-9]([-a-z0-9.]*[a-z0-9])?$ && ${#MODEL_NAME} -le 253 ]] || {
  printf 'GRID_CLIENT_MODEL_NAME must be a DNS-safe model name\n' >&2
  exit 2
}
MODEL_RESOURCE_NAME="publishers/$NAMESPACE/models/$MODEL_NAME"
[[ "$RUN_LABEL" =~ ^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$ ]] || { printf 'unsafe GRID_RUN_LABEL\n' >&2; exit 2; }

observed_server="$(oc whoami --show-server)"
[[ "$observed_server" == "$EXPECTED_API_SERVER" ]] || { printf 'API server mismatch\n' >&2; exit 2; }
[[ "$(oc get --raw=/readyz)" == *ok* ]] || { printf 'API /readyz failed\n' >&2; exit 2; }
insecure="$(oc config view --raw --minify -o jsonpath='{.clusters[0].cluster.insecure-skip-tls-verify}' 2>/dev/null || true)"
if [[ "$insecure" == true ]]; then
  [[ "$CLUSTER_ENV" == ephemeral-dev && "$ALLOW_INSECURE_DEV_API" == true ]] || {
    printf 'insecure API TLS requires explicit ephemeral-dev opt-in\n' >&2
    exit 2
  }
fi

namespace_json="$(oc get namespace "$NAMESPACE" -o json)"
jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' <<<"$namespace_json" >/dev/null || {
  printf 'namespace is not owned by this qualification run\n' >&2
  exit 2
}
[[ -n "$EVIDENCE_DIR" ]] || EVIDENCE_DIR="evidence/grid-client-identity-$(date -u +%Y%m%dT%H%M%SZ)"
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence path already exists: %s\n' "$EVIDENCE_DIR" >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

manifest="$EVIDENCE_DIR/grid-client-identity.yaml"
cat >"$manifest" <<YAML
apiVersion: v1
kind: ServiceAccount
metadata:
  name: $SA_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
automountServiceAccountToken: false
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: $ROLE_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
rules:
- apiGroups: [serving.kserve.io]
  resources: [llminferenceservices]
  resourceNames: [$POOL_A, $POOL_B]
  verbs: [get]
- apiGroups: [serving.opendatahub.io]
  resources: [models]
  resourceNames: ["$MODEL_RESOURCE_NAME"]
  verbs: [post]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata:
  name: $ROLE_BINDING_NAME
  namespace: $NAMESPACE
  labels:
    app.kubernetes.io/managed-by: grid-llmd-e2e-run
    app.kubernetes.io/part-of: grid-llmd-e2e
    praxis.fast/qualification-run: "$RUN_LABEL"
subjects:
- kind: ServiceAccount
  name: $SA_NAME
  namespace: $NAMESPACE
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: Role
  name: $ROLE_NAME
YAML
chmod 0600 "$manifest"

for kind_name in "serviceaccount/$SA_NAME" "role/$ROLE_NAME" "rolebinding/$ROLE_BINDING_NAME"; do
  kind="${kind_name%%/*}"
  name="${kind_name#*/}"
  if existing="$(oc get "$kind" "$name" -n "$NAMESPACE" -o json 2>/dev/null)"; then
    jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run' <<<"$existing" >/dev/null || {
      printf 'refusing to replace unowned %s/%s\n' "$kind" "$name" >&2
      exit 2
    }
  fi
done

if [[ "$MODE" != verify ]]; then
  oc apply --dry-run=server -f "$manifest" -o yaml >"$EVIDENCE_DIR/server-dry-run.yaml"
  if [[ "$MODE" == render ]]; then
    printf 'render and server-side dry-run passed; no client identity resources changed\n'
    exit 0
  fi
  oc apply -f "$manifest" >"$EVIDENCE_DIR/apply.txt"
fi

deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  live_sa="$(oc get serviceaccount "$SA_NAME" -n "$NAMESPACE" -o json 2>/dev/null || true)"
  live_role="$(oc get role "$ROLE_NAME" -n "$NAMESPACE" -o json 2>/dev/null || true)"
  live_binding="$(oc get rolebinding "$ROLE_BINDING_NAME" -n "$NAMESPACE" -o json 2>/dev/null || true)"
  if [[ -n "$live_sa" && -n "$live_role" && -n "$live_binding" ]] && \
     jq -e --arg run "$RUN_LABEL" '.metadata.labels["praxis.fast/qualification-run"]==$run and .automountServiceAccountToken==false' <<<"$live_sa" >/dev/null && \
     jq -e --arg run "$RUN_LABEL" --arg a "$POOL_A" --arg b "$POOL_B" --arg model "$MODEL_RESOURCE_NAME" '
       .metadata.labels["praxis.fast/qualification-run"]==$run and
       .rules==[
         {apiGroups:["serving.kserve.io"],resources:["llminferenceservices"],resourceNames:[$a,$b],verbs:["get"]},
         {apiGroups:["serving.opendatahub.io"],resources:["models"],resourceNames:[$model],verbs:["post"]}
       ]
     ' <<<"$live_role" >/dev/null && \
     jq -e --arg run "$RUN_LABEL" --arg sa "$SA_NAME" --arg role "$ROLE_NAME" --arg ns "$NAMESPACE" '
       .metadata.labels["praxis.fast/qualification-run"]==$run and
       .roleRef.kind=="Role" and .roleRef.name==$role and
       .subjects==[{kind:"ServiceAccount",name:$sa,namespace:$ns}]
     ' <<<"$live_binding" >/dev/null; then
    break
  fi
  sleep 2
done
(( SECONDS < deadline )) || { printf 'task-owned client identity did not converge\n' >&2; exit 1; }

subject="system:serviceaccount:$NAMESPACE:$SA_NAME"
authorization_review() {
  local verb="$1" resource="$2" resource_name="$3"
  jq -n --arg user "$subject" --arg ns "$NAMESPACE" --arg verb "$verb" \
    --arg group serving.opendatahub.io --arg resource "$resource" --arg name "$resource_name" \
    '{apiVersion:"authorization.k8s.io/v1",kind:"SubjectAccessReview",spec:{user:$user,groups:["system:serviceaccounts","system:serviceaccounts:"+$ns,"system:authenticated"],resourceAttributes:{namespace:$ns,verb:$verb,group:$group,resource:$resource,name:$name}}}' \
    | oc create -f - -o json \
    | jq -r 'if .status.allowed==true then "yes" else "no" end'
}
can_get_a="$(oc auth can-i get "llminferenceservices.serving.kserve.io/$POOL_A" -n "$NAMESPACE" --as="$subject" || true)"
can_get_b="$(oc auth can-i get "llminferenceservices.serving.kserve.io/$POOL_B" -n "$NAMESPACE" --as="$subject" || true)"
can_get_other="$(oc auth can-i get llminferenceservices.serving.kserve.io/not-a-run-pool -n "$NAMESPACE" --as="$subject" || true)"
can_list="$(oc auth can-i list llminferenceservices.serving.kserve.io -n "$NAMESPACE" --as="$subject" || true)"
can_post_model="$(authorization_review post models "$MODEL_RESOURCE_NAME")"
can_post_other_model="$(authorization_review post models "publishers/$NAMESPACE/models/not-this-run-model")"
jq -n --arg a "$can_get_a" --arg b "$can_get_b" --arg other "$can_get_other" --arg list "$can_list" \
  --arg modelName "$MODEL_RESOURCE_NAME" --arg modelPost "$can_post_model" --arg otherModelPost "$can_post_other_model" \
  '{poolAGet:$a,poolBGet:$b,otherModelGet:$other,listModels:$list,modelResource:$modelName,modelPost:$modelPost,otherModelPost:$otherModelPost}' \
  >"$EVIDENCE_DIR/authorization-matrix.json"
[[ "$can_get_a" == yes && "$can_get_b" == yes && "$can_get_other" == no && "$can_list" == no && \
   "$(jq -r '.modelPost' "$EVIDENCE_DIR/authorization-matrix.json")" == yes && \
   "$(jq -r '.otherModelPost' "$EVIDENCE_DIR/authorization-matrix.json")" == no ]] || {
  printf 'task client RBAC is broader or narrower than the two-pool and exact publisher-model authorization contract\n' >&2
  exit 1
}

jq -n --arg status PASS --arg namespace "$NAMESPACE" --arg serviceAccount "$SA_NAME" \
  --arg role "$ROLE_NAME" --arg binding "$ROLE_BINDING_NAME" --arg run "$RUN_LABEL" \
  --arg modelResource "$MODEL_RESOURCE_NAME" --arg evidence "$EVIDENCE_DIR" \
  '{status:$status,namespace:$namespace,serviceAccount:$serviceAccount,role:$role,roleBinding:$binding,runLabel:$run,authorizedPublisherModel:$modelResource,evidenceDirectory:$evidence,tokenMinted:false,tokenRecorded:false}' \
  >"$EVIDENCE_DIR/RESULT.json"
printf 'task-owned Grid client identity verified for the two pools and exact publisher model; no token was minted or recorded\n'
