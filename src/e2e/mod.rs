// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Qualification-tier helpers compiled only with `--features k8s-e2e`.
//!
//! Non-production: FIPS/release builds never enable this feature, so the
//! shipped binary does not register these filters.

pub mod body_oracle;
pub mod header_oracle;

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
    registry.register(
        "e2e_header_oracle",
        praxis_filter::http_builtin(header_oracle::HeaderOracleFilter::from_config),
    )?;
    Ok(())
}
