// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

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

    /// Ceilings on the body bytes the server assembles per direction.
    ///
    /// Bodies are assembled for `BUFFERED` and, when body filters are
    /// configured, `FULL_DUPLEX_STREAMED` processing. A body over the
    /// ceiling is answered with a local `413` reply. Both default to 10 MiB;
    /// `null` removes a ceiling and requires
    /// `insecure_options.allow_unbounded_body`.
    #[serde(default)]
    pub limits: praxis_core::config::BodyLimitsConfig,

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
/// Returns [`ExtProcError::Config`] if a body limit is removed without
/// `insecure_options.allow_unbounded_body`, and [`ExtProcError::Pipeline`]
/// if filter instantiation or validation fails.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
pub fn build_pipeline(config: &ExtProcConfig, registry: &FilterRegistry) -> Result<Arc<FilterPipeline>> {
    validate_chain_names(&config.filter_chains)?;
    validate_body_limits(&config.limits, config.insecure_options.allow_unbounded_body)?;

    let chains: std::collections::HashMap<&str, &[_]> = config
        .filter_chains
        .iter()
        .map(|c| (c.name.as_str(), c.filters.as_slice()))
        .collect();

    let mut entries = flatten_chains(&config.filter_chains);

    let mut pipeline = FilterPipeline::build_with_chains(&mut entries, registry, &chains)
        .map_err(|e| ExtProcError::Pipeline(e.to_string()))?;

    // The server assembles bodies itself and enforces `limits` while doing
    // so. The pipeline only needs the ceiling where a filter already buffers,
    // because `apply_body_limits` also marks a direction as needing the body,
    // which would switch every stream out of passthrough.
    let caps = pipeline.body_capabilities();
    let request_ceiling = caps
        .needs_request_body
        .then_some(config.limits.max_request_bytes)
        .flatten();
    let response_ceiling = caps
        .needs_response_body
        .then_some(config.limits.max_response_bytes)
        .flatten();
    pipeline
        .apply_body_limits(
            request_ceiling,
            response_ceiling,
            config.insecure_options.allow_unbounded_body,
        )
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

/// Reject removed body limits unless unbounded accumulation was opted into.
fn validate_body_limits(limits: &praxis_core::config::BodyLimitsConfig, allow_unbounded: bool) -> Result<()> {
    let removed = match (limits.max_request_bytes, limits.max_response_bytes) {
        (None, _) => Some("max_request_bytes"),
        (_, None) => Some("max_response_bytes"),
        (Some(_), Some(_)) => None,
    };
    match removed {
        Some(field) if !allow_unbounded => Err(ExtProcError::Config(format!(
            "limits.{field} is null; unbounded body accumulation requires \
             insecure_options.allow_unbounded_body: true"
        ))),
        _ => Ok(()),
    }
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
    clippy::panic,
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
    fn body_limits_default_to_ten_mib() {
        let cfg: ExtProcConfig = serde_yaml::from_str("{}").unwrap();

        assert_eq!(cfg.limits.max_request_bytes, Some(10_485_760), "request limit");
        assert_eq!(cfg.limits.max_response_bytes, Some(10_485_760), "response limit");
    }

    #[test]
    fn body_filter_pipeline_builds_without_insecure_flag() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: main
    filters:
      - filter: guardrails
        rules:
          - target: body
            contains: "DROP TABLE"
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let err = build_pipeline(&cfg, &registry).err().map(|e| e.to_string());

        assert!(
            err.is_none(),
            "the default limits bound body buffering, so no insecure opt-in is needed: {err:?}"
        );
    }

    #[test]
    fn null_limit_requires_allow_unbounded_body() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
limits:
  max_request_bytes: null
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let err = build_pipeline(&cfg, &registry)
            .err()
            .expect("removing a limit without opting in must fail");

        assert!(
            err.to_string().contains("allow_unbounded_body"),
            "error should name the opt-in: {err}"
        );
    }

    #[test]
    fn null_limit_allowed_with_insecure_opt_in() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
limits:
  max_request_bytes: null
  max_response_bytes: null
insecure_options:
  allow_unbounded_body: true
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();

        assert!(build_pipeline(&cfg, &registry).is_ok(), "opt-in lifts the limit");
    }

    #[test]
    fn shipped_examples_build_pipelines() {
        let examples = [
            ("praxis-extproc.yaml", include_str!("../examples/praxis-extproc.yaml")),
            (
                "ai-model-to-header.yaml",
                include_str!("../examples/ai-model-to-header.yaml"),
            ),
            ("branch-chains.yaml", include_str!("../examples/branch-chains.yaml")),
        ];
        let registry = praxis_ai_filters::build_ai_registry();

        for (name, yaml) in examples {
            let cfg: ExtProcConfig = serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{name}: {e}"));
            let err = build_pipeline(&cfg, &registry).err().map(|e| e.to_string());
            assert!(err.is_none(), "{name} must build a pipeline: {err:?}");
        }
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
