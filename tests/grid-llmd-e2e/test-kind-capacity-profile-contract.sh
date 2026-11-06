#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CREATE="$SCRIPT_DIR/create-kind-environment.sh"
BUILD="$SCRIPT_DIR/build-kind-images.sh"
create_source="$(<"$CREATE")"
build_source="$(<"$BUILD")"

rg -Fq 'full)' <<<"$create_source" || { printf 'existing full-capacity profile was removed\n' >&2; exit 1; }
rg -Fq 'KIND_MIN_CPU_COUNT:-8' <<<"$create_source" || { printf 'full profile CPU guard changed\n' >&2; exit 1; }
rg -Fq 'KIND_MIN_AVAILABLE_MEMORY_GIB:-8' <<<"$create_source" || { printf 'full profile memory guard changed\n' >&2; exit 1; }
rg -Fq 'gha-standard-4cpu)' <<<"$create_source" || { printf 'explicit four-CPU profile is missing\n' >&2; exit 1; }
rg -Fq 'REQUIRED_CPU_COUNT=4' <<<"$create_source" || { printf 'four-CPU profile does not require four CPUs\n' >&2; exit 1; }
rg -Fq 'EXACT_CPU_COUNT=4' <<<"$create_source" || { printf 'four-CPU profile does not reject other CPU counts\n' >&2; exit 1; }
rg -Fq 'docker_cpus != exact_cpus' <<<"$create_source" || { printf 'four-CPU profile does not validate Docker CPU visibility\n' >&2; exit 1; }

rg -Fq 'CONTAINER_BUILD_JOBS=1' <<<"$build_source" || { printf 'four-CPU profile does not serialize image builds\n' >&2; exit 1; }
rg -Fq 'CONTAINER_BUILD_CPU_QUOTA=400000' <<<"$build_source" || { printf 'four-CPU profile does not cap builder CPU quota\n' >&2; exit 1; }
rg -Fq 'CONTAINER_BUILD_MEMORY=6g' <<<"$build_source" || { printf 'four-CPU profile does not cap per-build memory\n' >&2; exit 1; }
rg -Fq 'container_build_limits' "$SCRIPT_DIR/../shared/build_and_push.sh" || { printf 'builder resource limits are not included in provenance\n' >&2; exit 1; }

if rg -n 'replicas: 1|resource requests|--replicas|finalizer.*remove|docker system prune' "$BUILD" "$CREATE"; then
  printf 'capacity profile must not weaken runtime resources, remove finalizers, or prune shared Docker state\n' >&2
  exit 1
fi
bash -n "$CREATE" "$BUILD" "$SCRIPT_DIR/../shared/build_and_push.sh"
printf 'Kind capacity profile contract passed\n'
