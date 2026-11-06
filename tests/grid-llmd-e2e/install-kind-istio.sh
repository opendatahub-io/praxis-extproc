#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install/upgrade the run-owned Kind Istio profile required by real KServe LLMInferenceServices.
set -Eeuo pipefail
umask 077

KUBE_CONTEXT="${KUBE_CONTEXT:-}"
KUBECONFIG_PATH="${KUBECONFIG:-}"
ISTIOCTL="${ISTIOCTL:-}"
EVIDENCE_DIR="${KIND_ISTIO_EVIDENCE_DIR:-}"

for name in KUBE_CONTEXT KUBECONFIG_PATH ISTIOCTL EVIDENCE_DIR; do
  [[ -n "${!name}" ]] || { printf '%s is required\n' "$name" >&2; exit 2; }
done
[[ -f "$KUBECONFIG_PATH" && "$KUBECONFIG_PATH" != "$EVIDENCE_DIR"/* ]] || {
  printf 'private KUBECONFIG must exist outside evidence\n' >&2
  exit 2
}
[[ -x "$ISTIOCTL" ]] || { printf 'ISTIOCTL must be executable\n' >&2; exit 2; }
command -v kubectl >/dev/null 2>&1 || { printf 'kubectl is required\n' >&2; exit 2; }
[[ ! -e "$EVIDENCE_DIR" ]] || { printf 'evidence directory already exists\n' >&2; exit 2; }
mkdir -p "$EVIDENCE_DIR"
chmod 0700 "$EVIDENCE_DIR"

kubectl --context "$KUBE_CONTEXT" get --raw=/readyz >"$EVIDENCE_DIR/readyz-before.txt"
grep -qx 'ok' "$EVIDENCE_DIR/readyz-before.txt" || { printf 'Kubernetes API /readyz failed\n' >&2; exit 1; }
if kubectl --context "$KUBE_CONTEXT" get namespace istio-system -o json \
  >"$EVIDENCE_DIR/istio-namespace-before.json" 2>"$EVIDENCE_DIR/istio-namespace-before.stderr"; then
  if kubectl --context "$KUBE_CONTEXT" get deployment istiod -n istio-system -o json \
    >"$EVIDENCE_DIR/istiod-before.json" 2>"$EVIDENCE_DIR/istiod-before.stderr"; then
    printf 'Istio namespace and existing control-plane Deployment found; upgrading existing installation.\n' \
      >"$EVIDENCE_DIR/istio-install-mode.txt"
  else
    if ! grep -q 'NotFound' "$EVIDENCE_DIR/istiod-before.stderr"; then
      cat "$EVIDENCE_DIR/istiod-before.stderr" >&2
      exit 1
    fi
    printf '{}\n' >"$EVIDENCE_DIR/istiod-before.json"
    printf 'Istio namespace exists without istiod; installing control plane.\n' \
      >"$EVIDENCE_DIR/istio-install-mode.txt"
  fi
else
  if ! grep -q 'NotFound' "$EVIDENCE_DIR/istio-namespace-before.stderr"; then
    cat "$EVIDENCE_DIR/istio-namespace-before.stderr" >&2
    exit 1
  fi
  printf '{}\n' >"$EVIDENCE_DIR/istio-namespace-before.json"
  printf '{}\n' >"$EVIDENCE_DIR/istiod-before.json"
  printf 'Istio namespace is absent; installing namespace and control plane.\n' \
    >"$EVIDENCE_DIR/istio-install-mode.txt"
fi
kubectl --context "$KUBE_CONTEXT" get gatewayclass istio -o json \
  >"$EVIDENCE_DIR/gatewayclass-before.json" 2>"$EVIDENCE_DIR/gatewayclass-before.stderr" || true
"$ISTIOCTL" version --remote=false >"$EVIDENCE_DIR/istioctl-version.txt" 2>&1
sha256sum "$ISTIOCTL" >"$EVIDENCE_DIR/istioctl-sha256.txt"

# MaaS main's local deployment path enables both flags. KServe-created
# InferencePool HTTPRoutes otherwise fail Gateway API ResolvedRefs in Istio.
cat >"$EVIDENCE_DIR/istio-install-command.txt" <<EOF
istioctl install --context $KUBE_CONTEXT --set profile=minimal --set components.ingressGateways[0].name=istio-ingressgateway --set components.ingressGateways[0].enabled=true --set values.gateways.istio-ingressgateway.autoscaleEnabled=false --set values.gateways.istio-ingressgateway.replicaCount=1 --set values.pilot.env.SUPPORT_GATEWAY_API_INFERENCE_EXTENSION=true --set values.pilot.env.ENABLE_GATEWAY_API_INFERENCE_EXTENSION=true -y
EOF
"$ISTIOCTL" install --context "$KUBE_CONTEXT" \
  --set profile=minimal \
  --set 'components.ingressGateways[0].name=istio-ingressgateway' \
  --set 'components.ingressGateways[0].enabled=true' \
  --set values.gateways.istio-ingressgateway.autoscaleEnabled=false \
  --set values.gateways.istio-ingressgateway.replicaCount=1 \
  --set values.pilot.env.SUPPORT_GATEWAY_API_INFERENCE_EXTENSION=true \
  --set values.pilot.env.ENABLE_GATEWAY_API_INFERENCE_EXTENSION=true \
  -y >"$EVIDENCE_DIR/istio-install.txt" 2>&1

kubectl --context "$KUBE_CONTEXT" rollout status deployment/istiod -n istio-system --timeout=240s \
  >"$EVIDENCE_DIR/istiod-rollout.txt"
kubectl --context "$KUBE_CONTEXT" wait gatewayclass/istio --for=condition=Accepted --timeout=120s \
  >"$EVIDENCE_DIR/gatewayclass-ready.txt"
kubectl --context "$KUBE_CONTEXT" get deployment istiod -n istio-system -o json \
  >"$EVIDENCE_DIR/istiod-after.json"
kubectl --context "$KUBE_CONTEXT" get pods -n istio-system -o json \
  >"$EVIDENCE_DIR/istio-pods-after.json"
kubectl --context "$KUBE_CONTEXT" get --raw=/readyz >"$EVIDENCE_DIR/readyz-after.txt"
grep -qx 'ok' "$EVIDENCE_DIR/readyz-after.txt" || { printf 'Kubernetes API /readyz failed after Istio rollout\n' >&2; exit 1; }
python3 - "$EVIDENCE_DIR/istiod-after.json" "$EVIDENCE_DIR/istio-pods-after.json" <<'PY'
import json
import sys
from pathlib import Path

deployment = json.loads(Path(sys.argv[1]).read_text())
containers = deployment["spec"]["template"]["spec"]["containers"]
env = {item["name"]: item.get("value") for item in containers[0].get("env", [])}
required = (
    "SUPPORT_GATEWAY_API_INFERENCE_EXTENSION",
    "ENABLE_GATEWAY_API_INFERENCE_EXTENSION",
)
missing = [name for name in required if env.get(name) != "true"]
if missing:
    raise SystemExit(f"istiod feature flags are not true: {', '.join(missing)}")
status = deployment.get("status", {})
if status.get("observedGeneration", 0) < deployment["metadata"]["generation"]:
    raise SystemExit("istiod has not observed its current generation")
if status.get("availableReplicas", 0) < deployment["spec"]["replicas"]:
    raise SystemExit("istiod does not have all desired replicas available")
pods = json.loads(Path(sys.argv[2]).read_text())["items"]
istiod_pods = [pod for pod in pods if pod["metadata"]["labels"].get("app") == "istiod"]
ready = [pod for pod in istiod_pods if all(c.get("ready") for c in pod.get("status", {}).get("containerStatuses", []))]
if len(ready) != deployment["spec"]["replicas"]:
    raise SystemExit(f"expected {deployment['spec']['replicas']} Ready istiod Pods, found {len(ready)}")
print("Istio Gateway API Inference Extension flags and current rollout: PASS")
PY
printf 'Istio Gateway API Inference Extension enabled and verified for context %s\n' "$KUBE_CONTEXT"
