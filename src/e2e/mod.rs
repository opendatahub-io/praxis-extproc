// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Qualification-tier helpers compiled only with `--features k8s-e2e`.
//!
//! See `docs/proposals/27_extended-e2e-tls-idle-fd-streamed.md` (How?).

pub mod body_oracle;

use praxis_filter::FilterRegistry;

/// Register e2e-only filters on an existing AI registry.
///
/// # Errors
///
/// Returns a registry error if a filter name is already taken.
pub fn register_e2e_filters(registry: &mut FilterRegistry) -> Result<(), praxis_filter::FilterError> {
    registry.register(
        "e2e_body_oracle",
        praxis_filter::http_builtin(body_oracle::BodyOracleFilter::from_config),
    )?;
    Ok(())
}
