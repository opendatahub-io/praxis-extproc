#!/usr/bin/env bash
# Build praxis-extproc and maas-controller images, load into Kind.
set -euo pipefail
# shellcheck source=lib.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib.sh"
parse_context "$@"
require_maas_root
require_cmd docker
require_cmd kind

echo -e "${BOLD}Building praxis-extproc image${NC}"
CONTAINER_ENGINE=docker make -C "$REPO_ROOT" container
kind load docker-image "$PRAXIS_EXTPROC_IMAGE" --name "$KIND_CLUSTER_NAME"
ok "praxis-extproc image loaded (${PRAXIS_EXTPROC_IMAGE})"

echo -e "${BOLD}Building maas-controller image${NC}"
MAAS_CONTROLLER_IMAGE="localhost/maas-controller:dev"
docker build -f "$MAAS_ROOT/maas-controller/Dockerfile" -t "$MAAS_CONTROLLER_IMAGE" "$MAAS_ROOT"
kind load docker-image "$MAAS_CONTROLLER_IMAGE" --name "$KIND_CLUSTER_NAME"
ok "maas-controller image loaded (${MAAS_CONTROLLER_IMAGE})"
