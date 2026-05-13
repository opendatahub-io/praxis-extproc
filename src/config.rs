// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! YAML configuration for the ExtProc server.
//!
//! Parses a minimal config containing filter chains and server settings.
//! Listeners and clusters are omitted because Envoy owns networking.

use std::sync::Arc;

use praxis_filter::{FilterPipeline, FilterRegistry};
use serde::Deserialize;

use crate::error::{Error, Result};

// -----------------------------------------------------------------------------
// ExtProcConfig
// -----------------------------------------------------------------------------

/// Top-level ExtProc server configuration.
///
/// ```
/// use praxis_extproc::config::ExtProcConfig;
///
/// let cfg: ExtProcConfig = serde_yaml::from_str(
///     r#"
/// filter_chains:
///   - name: main
///     filters:
///       - filter: request_id
/// "#,
/// )
/// .unwrap();
/// assert_eq!(cfg.filter_chains[0].name, "main");
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtProcConfig {
    /// Named filter chains. Concatenated in order to form the pipeline.
    #[serde(default)]
    pub filter_chains: Vec<praxis_core::config::FilterChainConfig>,

    /// Security overrides for development.
    #[serde(default)]
    pub insecure_options: praxis_core::config::InsecureOptions,

    /// gRPC server settings.
    #[serde(default)]
    pub server: ServerConfig,
}

/// gRPC server bind address and options.
///
/// ```
/// use praxis_extproc::config::ServerConfig;
///
/// let cfg = ServerConfig::default();
/// assert_eq!(cfg.grpc_address, "0.0.0.0:50051");
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// gRPC listen address.
    pub grpc_address: String,

    /// Health check listen address.
    pub health_address: String,

    /// Metrics endpoint listen address.
    pub metrics_address: String,

    /// TLS configuration.
    #[serde(default)]
    pub tls: crate::tls::TlsConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            grpc_address: "0.0.0.0:50051".to_owned(),
            health_address: "0.0.0.0:50052".to_owned(),
            metrics_address: "0.0.0.0:9090".to_owned(),
            tls: crate::tls::TlsConfig::default(),
        }
    }
}

// -----------------------------------------------------------------------------
// Pipeline Construction
// -----------------------------------------------------------------------------

/// Build a [`FilterPipeline`] from the config's filter chains.
///
/// Concatenates all chains in order, builds via the registry, and
/// applies body limits and insecure options.
///
/// # Errors
///
/// Returns [`Error::Pipeline`] if filter instantiation or validation fails.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
pub fn build_pipeline(config: &ExtProcConfig, registry: &FilterRegistry) -> Result<Arc<FilterPipeline>> {
    let chains: std::collections::HashMap<&str, &[_]> = config
        .filter_chains
        .iter()
        .map(|c| (c.name.as_str(), c.filters.as_slice()))
        .collect();

    let mut entries = flatten_chains(&config.filter_chains);

    let mut pipeline = FilterPipeline::build_with_chains(&mut entries, registry, &chains)
        .map_err(|e| Error::Pipeline(e.to_string()))?;

    pipeline
        .apply_body_limits(None, None, config.insecure_options.allow_unbounded_body)
        .map_err(|e| Error::Pipeline(e.to_string()))?;

    pipeline.apply_insecure_options(&config.insecure_options);

    Ok(Arc::new(pipeline))
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Concatenate all filter chain entries into a single flat list.
fn flatten_chains(chains: &[praxis_core::config::FilterChainConfig]) -> Vec<praxis_core::config::FilterEntry> {
    chains.iter().flat_map(|c| c.filters.clone()).collect()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    reason = "tests"
)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: main
    filters:
      - filter: request_id
"#,
        )
        .unwrap();

        assert_eq!(cfg.filter_chains.len(), 1, "should have one chain");
        assert_eq!(cfg.filter_chains[0].name, "main", "chain name should match");
        assert_eq!(cfg.filter_chains[0].filters.len(), 1, "should have one filter");
    }

    #[test]
    fn parse_empty_chains_defaults() {
        let cfg: ExtProcConfig = serde_yaml::from_str("{}").unwrap();

        assert!(cfg.filter_chains.is_empty(), "chains should default to empty");
        assert_eq!(cfg.server.grpc_address, "0.0.0.0:50051", "grpc address should default");
    }

    #[test]
    fn parse_custom_server_address() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  grpc_address: "127.0.0.1:9004"
"#,
        )
        .unwrap();

        assert_eq!(cfg.server.grpc_address, "127.0.0.1:9004", "address should match");
    }

    #[test]
    fn build_pipeline_with_builtins() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: main
    filters:
      - filter: request_id
      - filter: headers
        request_add:
          - name: X-Test
            value: extproc
"#,
        )
        .unwrap();

        let registry = FilterRegistry::with_builtins();
        let pipeline = build_pipeline(&cfg, &registry).unwrap();

        assert_eq!(pipeline.len(), 2, "pipeline should have two filters");
    }

    #[test]
    fn build_pipeline_unknown_filter_fails() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: main
    filters:
      - filter: nonexistent_filter
"#,
        )
        .unwrap();

        let registry = FilterRegistry::with_builtins();
        let result = build_pipeline(&cfg, &registry);

        assert!(result.is_err(), "unknown filter should fail");
    }

    #[test]
    fn flatten_multiple_chains() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: security
    filters:
      - filter: request_id
  - name: routing
    filters:
      - filter: headers
        request_add:
          - name: X-A
            value: "1"
"#,
        )
        .unwrap();

        let entries = flatten_chains(&cfg.filter_chains);

        assert_eq!(entries.len(), 2, "should flatten both chains");
    }
}
