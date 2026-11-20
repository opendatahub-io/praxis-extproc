// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Deploy-manifest contract guards.
//!
//! Static assertions over the checked-in kustomize overlays (ODH `EnvoyFilter`
//! order, fail-closed posture across every pre-proc anchor, metrics RBAC and SA
//! automount). These lock deploy invariants a YAML edit could silently break,
//! and run in CI without a cluster. They assert on manifests, not Rust behavior.

#[path = "deploy_contract/envoy_filter.rs"]
mod envoy_filter;

#[path = "deploy_contract/fail_closed.rs"]
mod fail_closed;

#[path = "deploy_contract/metrics_auth.rs"]
mod metrics_auth;
