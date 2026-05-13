#!/usr/bin/env bash
set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

CLUSTER_NAME="${KIND_CLUSTER_NAME:-praxis-extproc}"
EXTPROC_IMAGE="${EXTPROC_IMAGE:-praxis-extproc:dev}"
SAIL_REPO="https://istio-ecosystem.github.io/sail-operator"
GWAPI_VERSION="v1.5.1"
METALLB_VERSION="v0.14.9"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
KUBECTL="kubectl --context kind-${CLUSTER_NAME}"

# ---------------------------------------------------------------------------
# KIND Cluster
# ---------------------------------------------------------------------------

create_cluster() {
    if kind get clusters 2>/dev/null | grep -qx "${CLUSTER_NAME}"; then
        echo "==> Cluster '${CLUSTER_NAME}' already exists, reusing."
    else
        echo "==> Creating KIND cluster '${CLUSTER_NAME}'..."
        kind create cluster \
            --name "${CLUSTER_NAME}" \
            --config "${SCRIPT_DIR}/kind-config.yaml" \
            --wait 60s
    fi
}

# ---------------------------------------------------------------------------
# Gateway API CRDs
# ---------------------------------------------------------------------------

install_gateway_api() {
    echo "==> Installing Gateway API CRDs ${GWAPI_VERSION}..."
    ${KUBECTL} apply -f \
        "https://github.com/kubernetes-sigs/gateway-api/releases/download/${GWAPI_VERSION}/standard-install.yaml"
}

# ---------------------------------------------------------------------------
# MetalLB
# ---------------------------------------------------------------------------

install_metallb() {
    echo "==> Installing MetalLB ${METALLB_VERSION}..."
    ${KUBECTL} apply -f \
        "https://raw.githubusercontent.com/metallb/metallb/${METALLB_VERSION}/config/manifests/metallb-native.yaml"
    ${KUBECTL} wait --namespace metallb-system \
        --for=condition=ready pod \
        --selector=app=metallb \
        --timeout=300s
}

configure_metallb_pool() {
    echo "==> Configuring MetalLB IP pool..."
    SUBNET=$(docker network inspect kind \
        -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' \
        | tr ' ' '\n' | grep '\.' | head -1)
    IFS='.' read -r a b c d <<< "${SUBNET%%/*}"
    cat <<EOF | ${KUBECTL} apply -f -
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata:
  name: kind-pool
  namespace: metallb-system
spec:
  addresses:
    - ${a}.${b}.255.200-${a}.${b}.255.210
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata:
  name: l2
  namespace: metallb-system
EOF
}

# ---------------------------------------------------------------------------
# Istio (Sail Operator)
# ---------------------------------------------------------------------------

install_sail_operator() {
    echo "==> Installing Sail Operator..."
    helm repo add sail-operator "${SAIL_REPO}" 2>/dev/null || true
    helm repo update

    if helm list --namespace sail-operator \
        --kube-context "kind-${CLUSTER_NAME}" 2>/dev/null \
        | grep -q sail-operator; then
        echo "    Sail Operator already installed, skipping"
    else
        ${KUBECTL} create namespace sail-operator 2>/dev/null || true
        helm install sail-operator sail-operator/sail-operator \
            --namespace sail-operator \
            --kube-context "kind-${CLUSTER_NAME}" \
            --wait --timeout 5m
    fi

    ${KUBECTL} wait --namespace sail-operator \
        --for=condition=Available \
        deployment/sail-operator \
        --timeout=300s
}

create_istio_control_plane() {
    local istio_version
    istio_version="${ISTIO_VERSION:-}"

    if [ -z "${istio_version}" ]; then
        istio_version=$(helm list -n sail-operator \
            --kube-context "kind-${CLUSTER_NAME}" -o json \
            | grep -o '"app_version":"[^"]*"' \
            | head -1 \
            | sed 's/"app_version":"//;s/"//')
        echo "==> Auto-detected Istio version: v${istio_version}"
    fi

    echo "==> Creating Istio control plane (v${istio_version})..."
    ${KUBECTL} create namespace istio-system 2>/dev/null || true

    cat <<EOF | ${KUBECTL} apply -f -
apiVersion: sailoperator.io/v1
kind: Istio
metadata:
  name: default
  namespace: istio-system
spec:
  namespace: istio-system
  version: v${istio_version}
  values:
    pilot:
      env:
        PILOT_ENABLE_GATEWAY_API: "true"
        PILOT_ENABLE_GATEWAY_API_STATUS: "true"
        PILOT_ENABLE_GATEWAY_API_DEPLOYMENT_CONTROLLER: "true"
EOF

    echo "==> Waiting for Istio control plane..."
    ${KUBECTL} --namespace istio-system wait \
        --for=condition=Ready istio/default \
        --timeout=600s
}

# ---------------------------------------------------------------------------
# Container Image
# ---------------------------------------------------------------------------

build_and_load_image() {
    echo "==> Building container image..."
    docker build -t "${EXTPROC_IMAGE}" -f "${ROOT_DIR}/Containerfile" "${ROOT_DIR}"

    echo "==> Loading image into KIND..."
    kind load docker-image "${EXTPROC_IMAGE}" --name "${CLUSTER_NAME}"
}

# ---------------------------------------------------------------------------
# praxis-extproc Deployment
# ---------------------------------------------------------------------------

deploy_extproc() {
    echo "==> Deploying praxis-extproc..."
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/namespace.yaml"
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/configmap.yaml"
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/deployment.yaml"
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/service.yaml"

    echo "==> Waiting for praxis-extproc rollout..."
    ${KUBECTL} -n praxis-extproc rollout status \
        deployment/praxis-extproc --timeout=120s
}

# ---------------------------------------------------------------------------
# Test Resources
# ---------------------------------------------------------------------------

deploy_test_resources() {
    echo "==> Deploying test resources..."
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/echo.yaml"

    ${KUBECTL} -n praxis-test rollout status \
        deployment/echo --timeout=60s

    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/gateway.yaml"
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/httproute.yaml"

    echo "==> Waiting for Gateway to be programmed..."
    for i in $(seq 1 120); do
        STATUS=$(${KUBECTL} -n praxis-test get gateway praxis-test \
            -o jsonpath='{.status.conditions[?(@.type=="Programmed")].status}' \
            2>/dev/null || true)
        if [ "${STATUS}" = "True" ]; then
            echo "    Gateway programmed after ${i}s"
            break
        fi
        if [ "${i}" -eq 120 ]; then
            echo "FAIL: Gateway not programmed after 120s"
            ${KUBECTL} -n praxis-test describe gateway praxis-test
            exit 1
        fi
        sleep 1
    done

    echo "==> Applying EnvoyFilter..."
    ${KUBECTL} apply -f "${ROOT_DIR}/deploy/envoyfilter.yaml"

    echo "==> Waiting for Envoy config propagation..."
    sleep 5
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

for cmd in kind kubectl helm docker; do
    if ! command -v "${cmd}" &>/dev/null; then
        echo "ERROR: ${cmd} is required but not found"
        exit 1
    fi
done

create_cluster
install_gateway_api
install_metallb
configure_metallb_pool
install_sail_operator
create_istio_control_plane
build_and_load_image
deploy_extproc
deploy_test_resources

echo "==> KIND cluster ready for integration tests."
echo ""
echo "    Cluster:  ${CLUSTER_NAME}"
echo "    Context:  kind-${CLUSTER_NAME}"
echo ""
echo "    Port-forward to Gateway:"
echo "    kubectl --context kind-${CLUSTER_NAME} -n praxis-test port-forward deploy/praxis-test-istio 18080:8080"
echo ""
echo "    Run smoke test:"
echo "    make smoke-test"
