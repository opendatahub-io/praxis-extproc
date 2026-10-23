// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Qualification-tier helpers compiled only with `--features k8s-e2e`.
//!
//! Non-production: FIPS/release builds never enable this feature, so the
//! shipped binary does not register these filters.

pub mod body_oracle;
pub mod header_oracle;

use praxis_filter::{FilterError, FilterRegistry};

/// Env var assigning this Deployment's hop index in request-flow order. Shared
/// by the e2e oracles so a single per-hop env var names all their headers.
pub const HOP_INDEX_ENV: &str = "BODY_ORACLE_HOP_INDEX";

/// Parse the hop index from [`HOP_INDEX_ENV`], when set.
///
/// # Errors
///
/// Returns [`FilterError`] if the variable is set but is not a `u32`.
pub(crate) fn hop_index_from_env() -> Result<Option<u32>, FilterError> {
    std::env::var(HOP_INDEX_ENV)
        .ok()
        .map(|raw| {
            raw.parse::<u32>()
                .map_err(|error| FilterError::from(format!("{HOP_INDEX_ENV}={raw:?} is not a u32: {error}")))
        })
        .transpose()
}

/// Register e2e-only filters on an existing AI registry.
///
/// # Errors
///
/// Returns a registry error if a filter name is already taken.
pub fn register_e2e_filters(registry: &mut FilterRegistry) -> Result<(), FilterError> {
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
