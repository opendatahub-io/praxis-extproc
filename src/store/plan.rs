// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Adapt ExtProc's concatenated chains to Praxis AI's shared store planner.

use std::{collections::HashMap, sync::Arc};

use praxis_ai_apis::store::plan;
use praxis_ai_store::StoreBackendFactory;
use praxis_ai_store_lifecycle::StoreRef;

use crate::{
    config::ExtProcConfig,
    error::{ExtProcError, Result},
};

/// Plan one registry spanning every ExtProc chain, without opening a database.
pub(super) fn store_refs(config: &ExtProcConfig, factories: &[Arc<dyn StoreBackendFactory>]) -> Result<Vec<StoreRef>> {
    let roots: Vec<String> = config.filter_chains.iter().map(|chain| chain.name.clone()).collect();
    let chains = config
        .filter_chains
        .iter()
        .map(|chain| (chain.name.as_str(), chain.filters.as_slice()))
        .collect();
    let mut refs =
        plan::discover_store_refs(&roots, &chains).map_err(|error| ExtProcError::Config(error.to_string()))?;
    let factories: HashMap<&str, &dyn StoreBackendFactory> = factories
        .iter()
        .map(|factory| (factory.backend_id(), factory.as_ref()))
        .collect();
    #[cfg(any(feature = "conversations", feature = "responses-full"))]
    plan::coalesce_compatible_filter_refs(std::slice::from_mut(&mut refs), &factories)
        .map_err(|error| ExtProcError::Config(error.to_string()))?;
    plan::validate_sql_namespace_collisions(std::slice::from_ref(&refs), &factories)
        .map_err(|error| ExtProcError::Config(error.to_string()))?;
    plan::deduplicate_refs("ExtProc", &mut refs, &factories)
        .map_err(|error| ExtProcError::Config(error.to_string()))?;
    Ok(refs)
}
