#!/usr/bin/env bash
# Install the pinned Envoy executable for loopback transport qualification.
set -euo pipefail

destination=${1:?usage: install-test-envoy.sh DESTINATION}
digest=0a5729ee4e980d346ebcee80f18e7efe5232b91141bb4c776ec3adcca786e60c
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
  echo 'The pinned test Envoy requires Linux x86_64.' >&2
  exit 1
fi
if [[ -f "$destination" ]] && echo "$digest  $destination" | sha256sum --check --status; then
  exit 0
fi
mkdir -p "$(dirname "$destination")"
temporary=$(mktemp "${destination}.XXXXXX")
trap 'rm -f "$temporary"' EXIT
curl --fail --location --retry 3 --output "$temporary" \
  https://github.com/envoyproxy/envoy/releases/download/v1.37.0/envoy-1.37.0-linux-x86_64
echo "$digest  $temporary" | sha256sum --check
chmod +x "$temporary"
mv "$temporary" "$destination"
