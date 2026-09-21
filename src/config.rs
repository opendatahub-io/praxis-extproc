// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! YAML configuration for the ExtProc server.
//!
//! Parses a minimal config containing filter chains and server settings.
//! Listeners and clusters are omitted because Envoy owns networking.

use std::{collections::HashSet, sync::Arc};

use praxis_filter::{FilterPipeline, FilterRegistry};
use serde::Deserialize;

use crate::error::{ExtProcError, Result};

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

    /// HTTP/2 keepalive ping interval in seconds (0 disables keepalive).
    ///
    /// The server pings idle client connections at this interval so a dead
    /// peer (e.g. a gone Envoy) is detected and its connection reclaimed
    /// rather than lingering.
    pub http2_keepalive_interval_secs: u64,

    /// HTTP/2 keepalive ping timeout in seconds.
    ///
    /// A connection is closed when a keepalive ping is unacknowledged for this
    /// long. Only applies when the interval is non-zero.
    pub http2_keepalive_timeout_secs: u64,

    /// Maximum connection age in seconds (0 disables it).
    ///
    /// Once a connection exceeds this age the server signals a graceful HTTP/2
    /// GOAWAY and lets in-flight requests drain. No grace period is set, so the
    /// server never force-closes. This is a soft cap that prompts periodic
    /// reconnection rather than a hard lifetime bound.
    pub max_connection_age_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            grpc_address: "0.0.0.0:50051".to_owned(),
            health_address: "0.0.0.0:50052".to_owned(),
            metrics_address: "0.0.0.0:9090".to_owned(),
            tls: crate::tls::TlsConfig::default(),
            // Keepalive on by default to detect dead peers. Connection-age bound
            // stays off by default so it does not change connection churn unless set.
            http2_keepalive_interval_secs: 60,
            http2_keepalive_timeout_secs: 20,
            max_connection_age_secs: 0,
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
/// Returns [`ExtProcError::Pipeline`] if filter instantiation or validation fails.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
pub fn build_pipeline(config: &ExtProcConfig, registry: &FilterRegistry) -> Result<Arc<FilterPipeline>> {
    validate_chain_names(&config.filter_chains)?;

    let chains: std::collections::HashMap<&str, &[_]> = config
        .filter_chains
        .iter()
        .map(|c| (c.name.as_str(), c.filters.as_slice()))
        .collect();

    let mut entries = flatten_chains(&config.filter_chains);

    let mut pipeline = FilterPipeline::build_with_chains(&mut entries, registry, &chains)
        .map_err(|e| ExtProcError::Pipeline(e.to_string()))?;

    pipeline
        .apply_body_limits(None, None, config.insecure_options.allow_unbounded_body)
        .map_err(|e| ExtProcError::Pipeline(e.to_string()))?;

    pipeline.apply_insecure_options(&config.insecure_options);
    pipeline.add_pipeline_extension(Box::new(praxis_ai_apis::store::ResponseStoreRegistry::new()));

    Ok(Arc::new(pipeline))
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Reject configs with duplicate filter chain names.
fn validate_chain_names(chains: &[praxis_core::config::FilterChainConfig]) -> Result<()> {
    let mut seen = HashSet::new();
    for chain in chains {
        if !seen.insert(&chain.name) {
            return Err(ExtProcError::Config(format!(
                "duplicate filter chain name: {}",
                chain.name
            )));
        }
    }
    Ok(())
}

/// Concatenate all filter chain entries into a single flat list.
fn flatten_chains(chains: &[praxis_core::config::FilterChainConfig]) -> Vec<praxis_core::config::FilterEntry> {
    chains.iter().flat_map(|c| c.filters.clone()).collect()
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::needless_raw_strings,
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
    fn server_keepalive_defaults() {
        let cfg = ServerConfig::default();
        assert_eq!(
            cfg.http2_keepalive_interval_secs, 60,
            "keepalive interval should default to 60s"
        );
        assert_eq!(
            cfg.http2_keepalive_timeout_secs, 20,
            "keepalive timeout should default to 20s"
        );
        assert_eq!(
            cfg.max_connection_age_secs, 0,
            "max connection age should default to disabled"
        );
    }

    #[test]
    fn parse_server_keepalive_overrides() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  http2_keepalive_interval_secs: 30
  http2_keepalive_timeout_secs: 10
  max_connection_age_secs: 3600
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.server.http2_keepalive_interval_secs, 30,
            "interval override should apply"
        );
        assert_eq!(
            cfg.server.http2_keepalive_timeout_secs, 10,
            "timeout override should apply"
        );
        assert_eq!(
            cfg.server.max_connection_age_secs, 3600,
            "max age override should apply"
        );
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

        let registry = praxis_ai_filters::build_ai_registry();
        let pipeline = build_pipeline(&cfg, &registry).unwrap();

        assert_eq!(pipeline.len(), 2, "pipeline should have two filters");
    }

    #[test]
    fn build_pipeline_with_ai_filter() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: main
    filters:
      - filter: model_to_header
        header: X-AI-Model
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let pipeline = build_pipeline(&cfg, &registry).unwrap();

        assert_eq!(pipeline.len(), 1, "pipeline should have one AI filter");
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

        let registry = praxis_ai_filters::build_ai_registry();
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

    #[test]
    fn duplicate_chain_names_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: dupe
    filters:
      - filter: request_id
  - name: dupe
    filters:
      - filter: request_id
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let err = build_pipeline(&cfg, &registry)
            .err()
            .expect("duplicate chain names should fail");

        assert!(
            err.to_string().contains("duplicate"),
            "error should mention duplicate: {err}"
        );
    }

    #[test]
    fn deny_unknown_fields_rejects_extra_keys() {
        let result: std::result::Result<ExtProcConfig, _> = serde_yaml::from_str(
            r#"
filter_chains: []
bogus_key: true
"#,
        );

        assert!(result.is_err(), "unknown fields should be rejected");
    }
}
