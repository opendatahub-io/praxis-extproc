#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Render the narrow MonitoringStack CPU-request overlay owned by this E2E.
set -Eeuo pipefail
umask 077

profile=""
output=""
while (($#)); do
  case "$1" in
    --profile) profile="${2:-}"; shift 2 ;;
    --output) output="${2:-}"; shift 2 ;;
    *) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
  esac
done

[[ -n "$output" ]] || { printf '%s\n' '--output is required' >&2; exit 2; }
case "$profile" in
  strict) prometheus_cpu=100m; thanos_cpu=50m ;;
  reduced-dev) prometheus_cpu=50m; thanos_cpu=10m ;;
  *) printf 'profile must be strict or reduced-dev\n' >&2; exit 2 ;;
esac

cat >"$output" <<YAML
apiVersion: monitoring.rhobs/v1alpha1
kind: MonitoringStack
metadata:
  name: data-science-monitoringstack
  namespace: redhat-ods-monitoring
spec:
  resources:
    requests:
      cpu: "$prometheus_cpu"
  prometheusConfig:
    thanosResources:
      requests:
        cpu: "$thanos_cpu"
YAML
chmod 0600 "$output"
