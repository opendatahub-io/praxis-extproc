#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Install/upgrade the run-owned Kind Istio profile required by real KServe LLMInferenceServices.
set -Eeuo pipefail
umask 077

KUBE_CONTEXT="${KUBE_CONTEXT:-}"
KUBECONFIG_PATH="${KUBECONFIG:-}"
ISTIOCTL="${ISTIOCTL:-}"
EVIDENCE_DIR="${KIND_ISTIO_EVIDENCE_DIR:-}"
CAPACITY_PROFILE="${KIND_CAPACITY_PROFILE:-full}"
ISTIOD_CPU_REQUEST=""

case "$CAPACITY_PROFILE" in
  full) ;;
  gha-standard-4cpu) ISTIOD_CPU_REQUEST=400m ;;
  *) printf 'KIND_CAPACITY_PROFILE must be full or gha-standard-4cpu\n' >&2; exit 2 ;;
esac

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

install_args=(
  install
  --context "$KUBE_CONTEXT"
  --set
  profile=minimal
  --set 'components.ingressGateways[0].name=istio-ingressgateway' \
  --set 'components.ingressGateways[0].enabled=true' \
  --set values.gateways.istio-ingressgateway.autoscaleEnabled=false \
  --set values.gateways.istio-ingressgateway.replicaCount=1 \
  --set values.pilot.env.SUPPORT_GATEWAY_API_INFERENCE_EXTENSION=true \
  --set values.pilot.env.ENABLE_GATEWAY_API_INFERENCE_EXTENSION=true
)
if [[ -n "$ISTIOD_CPU_REQUEST" ]]; then
  # The four-CPU runner's one-node Kind cluster reached 100% CPU requests
  # before the second 10m provider proxy and the 50m ExtProc could schedule.
  # Lower only Istiod's scheduling request; its memory and unbounded CPU limit
  # remain at the pinned Istio profile defaults. Keep the full profile intact.
  install_args+=(--set "values.pilot.resources.requests.cpu=$ISTIOD_CPU_REQUEST")
fi
{
  printf 'istioctl'
  printf ' %q' "${install_args[@]}"
  printf ' -y\n'
} >"$EVIDENCE_DIR/istio-install-command.txt"
printf '%s\n' "$CAPACITY_PROFILE" >"$EVIDENCE_DIR/capacity-profile.txt"
printf '%s\n' "${ISTIOD_CPU_REQUEST:-istio-default}" >"$EVIDENCE_DIR/istiod-cpu-request-profile.txt"
"$ISTIOCTL" "${install_args[@]}" -y >"$EVIDENCE_DIR/istio-install.txt" 2>&1

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
python3 - "$EVIDENCE_DIR/istiod-after.json" "$EVIDENCE_DIR/istio-pods-after.json" "$ISTIOD_CPU_REQUEST" \
  >"$EVIDENCE_DIR/istiod-install-verification.json" <<'PY'
import json
import sys
from pathlib import Path

deployment = json.loads(Path(sys.argv[1]).read_text())
containers = deployment["spec"]["template"]["spec"]["containers"]
discovery = next((container for container in containers if container["name"] == "discovery"), None)
if discovery is None:
    raise SystemExit("istiod Deployment has no discovery container")
env = {item["name"]: item.get("value") for item in discovery.get("env", [])}
required = (
    "SUPPORT_GATEWAY_API_INFERENCE_EXTENSION",
    "ENABLE_GATEWAY_API_INFERENCE_EXTENSION",
)
missing = [name for name in required if env.get(name) != "true"]
if missing:
    raise SystemExit(f"istiod feature flags are not true: {', '.join(missing)}")
actual_cpu_request = discovery.get("resources", {}).get("requests", {}).get("cpu")
expected_cpu_request = sys.argv[3]
if expected_cpu_request and actual_cpu_request != expected_cpu_request:
    raise SystemExit(
        f"istiod CPU request profile mismatch: expected {expected_cpu_request}, found {actual_cpu_request}"
    )
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
print(json.dumps({
    "capacityProfile": "gha-standard-4cpu" if expected_cpu_request else "full",
    "cpuRequest": actual_cpu_request,
    "cpuLimit": discovery.get("resources", {}).get("limits", {}).get("cpu"),
    "memoryRequest": discovery.get("resources", {}).get("requests", {}).get("memory"),
    "memoryLimit": discovery.get("resources", {}).get("limits", {}).get("memory"),
    "readyReplicas": len(ready),
    "observedGeneration": status.get("observedGeneration"),
    "generation": deployment["metadata"]["generation"],
}, sort_keys=True))
PY
printf 'Istio Gateway API Inference Extension flags and current rollout: PASS\n'
printf 'Istio Gateway API Inference Extension enabled and verified for context %s\n' "$KUBE_CONTEXT"
