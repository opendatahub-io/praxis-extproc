#!/usr/bin/env bash
# Rebuild/reload praxis-extproc (or maas-controller/maas-api) into the Kind cluster.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
parse_context "$@"
require_cmd kind
require_cmd docker

COMPONENT="${2:-praxis-extproc}"

prefer_local_kind_tag() {
  local image="$1"
  local name="$2"
  case "$image" in
    quay.io/opendatahub/"${name}":*)
      local tag="${image##*:}"
      echo "localhost/${name}:${tag}"
      ;;
    *)
      echo "$image"
      ;;
  esac
}

pin_deployment_image() {
  local deploy="$1"
  local container="$2"
  local image="$3"
  local ns="${4:-$MAAS_NAMESPACE}"

  kc set image "deployment/${deploy}" -n "$ns" "${container}=${image}" || true
  kc patch "deployment/${deploy}" -n "$ns" --type=json -p="[
    {\"op\":\"replace\",\"path\":\"/spec/template/spec/containers/0/imagePullPolicy\",\"value\":\"IfNotPresent\"}
  ]"
  kc rollout restart "deployment/${deploy}" -n "$ns"
  kc rollout status "deployment/${deploy}" -n "$ns" --timeout=180s
}

case "$COMPONENT" in
  praxis-extproc|praxis|extproc)
    echo "  Building praxis-extproc -> ${PRAXIS_EXTPROC_IMAGE}"
    CONTAINER_ENGINE=docker make -C "$REPO_ROOT" container
    kind load docker-image "$PRAXIS_EXTPROC_IMAGE" --name "$KIND_CLUSTER_NAME"
    # Restart all payload-processing deployments that use praxis-extproc
    for deploy in payload-processing payload-pre-processing; do
      if kc get deployment "$deploy" -n "$GATEWAY_NAMESPACE" &>/dev/null; then
        kc rollout restart "deployment/$deploy" -n "$GATEWAY_NAMESPACE"
        kc rollout status "deployment/$deploy" -n "$GATEWAY_NAMESPACE" --timeout=180s
        ok "$deploy reloaded"
      fi
    done
    ok "praxis-extproc reloaded (${PRAXIS_EXTPROC_IMAGE})"
    ;;
  maas-controller|controller)
    require_maas_root
    MAAS_CONTROLLER_IMAGE="$(prefer_local_kind_tag "$MAAS_CONTROLLER_IMAGE" maas-controller)"
    echo "  Building maas-controller -> ${MAAS_CONTROLLER_IMAGE}"
    (cd "$MAAS_ROOT" && docker build -f maas-controller/Dockerfile -t "$MAAS_CONTROLLER_IMAGE" .)
    kind load docker-image "$MAAS_CONTROLLER_IMAGE" --name "$KIND_CLUSTER_NAME"
    platform_manifests="/maas-api/deploy/overlays/xks"
    if [[ "${MAAS_IPP_PROFILE}" == "praxis" ]]; then
      platform_manifests="/maas-api/deploy/overlays/xks-praxis"
    fi
    kc set env deployment/maas-controller -n "$MAAS_NAMESPACE" \
      "MAAS_IPP_PROFILE=${MAAS_IPP_PROFILE}" \
      "MAAS_PLATFORM_MANIFESTS=${platform_manifests}" \
      "RELATED_IMAGE_PRAXIS_EXTPROC_IMAGE=${PRAXIS_EXTPROC_IMAGE}"
    pin_deployment_image maas-controller manager "$MAAS_CONTROLLER_IMAGE"
    ok "maas-controller reloaded (${MAAS_CONTROLLER_IMAGE}, profile=${MAAS_IPP_PROFILE})"
    ;;
  maas-api|api)
    require_maas_root
    MAAS_API_IMAGE="$(prefer_local_kind_tag "$MAAS_API_IMAGE" maas-api)"
    echo "  Building maas-api -> ${MAAS_API_IMAGE}"
    (cd "$MAAS_ROOT/maas-api" && docker build -t "$MAAS_API_IMAGE" .)
    kind load docker-image "$MAAS_API_IMAGE" --name "$KIND_CLUSTER_NAME"
    pin_deployment_image maas-api '*' "$MAAS_API_IMAGE"
    ok "maas-api reloaded (${MAAS_API_IMAGE})"
    ;;
  *)
    die "unknown component '$COMPONENT' (use praxis-extproc|maas-controller|maas-api)"
    ;;
esac
