// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! YAML configuration for the ExtProc server.
//!
//! Parses a minimal config containing filter chains and server settings.
//! Listeners and clusters are omitted because Envoy owns networking.

use std::{collections::HashSet, sync::Arc};

use praxis_filter::{FilterPipeline, FilterRegistry};
use serde::Deserialize;

use crate::{
    error::{ExtProcError, Result},
    maas::{BbrProcessor, ModelEntry, RoutingState, TrustBoundary, TrustBoundaryConfig},
};

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

    /// `MaaS` (Models as a Service) configuration.
    #[serde(default)]
    pub maas: Option<MaasConfig>,
}

// -----------------------------------------------------------------------------
// MaaS Configuration
// -----------------------------------------------------------------------------

/// `MaaS` (Models as a Service) configuration.
///
/// ```
/// use praxis_extproc::config::{ExtProcConfig, MaasConfig};
///
/// let cfg: ExtProcConfig = serde_yaml::from_str(
///     r#"
/// maas:
///   bbr:
///     enabled: true
///     models:
///       - model: gpt-4
///         provider:
///           authority: api.openai.com
///           path_prefix: /v1
/// "#,
/// )
/// .unwrap();
/// assert!(cfg.maas.is_some());
/// assert!(cfg.maas.unwrap().bbr.enabled);
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MaasConfig {
    /// Body-based routing configuration.
    #[serde(default)]
    pub bbr: BbrConfig,
}

/// Body-Based Routing (BBR) configuration.
///
/// Controls how requests are routed based on the model name extracted
/// from the request body.
///
/// # Prerequisites
///
/// BBR requires the `model_to_header` filter from `praxis-ai-filters` to be
/// configured in the pipeline. This filter extracts the model name from the
/// JSON request body and places it in the `X-AI-Model` header, which BBR
/// then reads to determine routing.
///
/// # Example Configuration
///
/// ```yaml
/// filter_chains:
///   - name: main
///     filters:
///       - filter: model_to_header
///         header: X-AI-Model
///
/// maas:
///   bbr:
///     enabled: true
///     models:
///       - model: gpt-4
///         provider:
///           authority: api.openai.com
///           path_prefix: /v1
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BbrConfig {
    /// Whether BBR is enabled.
    ///
    /// When `false`, all BBR processing is skipped.
    /// When `true`, requires `model_to_header` filter in the pipeline.
    pub enabled: bool,

    /// Model-to-provider mappings.
    ///
    /// Each entry maps a model name to its authorized provider.
    #[serde(default)]
    pub models: Vec<ModelEntry>,

    /// Trust boundary configuration.
    ///
    /// Controls which headers are stripped from requests.
    #[serde(default)]
    pub trust_boundary: TrustBoundaryConfig,
}

// -----------------------------------------------------------------------------
// Server Configuration
// -----------------------------------------------------------------------------

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
/// Concatenates all chains in order, builds via the registry, applies
/// body limits, and registers pipeline extensions (`MaaS` BBR when enabled).
///
/// # Errors
///
/// Returns [`ExtProcError::Pipeline`] if filter instantiation or validation fails.
/// Returns [`ExtProcError::Config`] if BBR is enabled with invalid models.
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
    register_pipeline_extensions(&mut pipeline, config)?;

    Ok(Arc::new(pipeline))
}

/// Register pipeline-scoped resources copied onto each request.
fn register_pipeline_extensions(pipeline: &mut FilterPipeline, config: &ExtProcConfig) -> Result<()> {
    pipeline.add_pipeline_extension(Box::new(praxis_ai_apis::store::ResponseStoreRegistry::new()));
    if let Some(bbr) = build_bbr_processor(config)? {
        pipeline.add_pipeline_extension(Box::new(bbr));
    }
    Ok(())
}

/// Build a [`BbrProcessor`] from the `MaaS` configuration.
///
/// Returns `Ok(None)` if BBR is not enabled or not configured.
/// Returns an error if BBR is enabled but `models` is empty.
///
/// # Errors
///
/// Returns [`ExtProcError::Config`] when `maas.bbr.enabled` is `true` and
/// `maas.bbr.models` is empty, a model name is duplicated, or a provider
/// `authority` is empty.
///
/// # Example
///
/// ```
/// use praxis_extproc::config::{ExtProcConfig, build_bbr_processor};
///
/// let cfg: ExtProcConfig = serde_yaml::from_str(
///     r#"
/// maas:
///   bbr:
///     enabled: true
///     models:
///       - model: gpt-4
///         provider:
///           authority: api.openai.com
///           path_prefix: /v1
/// "#,
/// )
/// .unwrap();
///
/// let bbr = build_bbr_processor(&cfg).unwrap();
/// assert!(bbr.is_some());
/// ```
pub fn build_bbr_processor(config: &ExtProcConfig) -> Result<Option<BbrProcessor>> {
    let Some(maas) = config.maas.as_ref() else {
        return Ok(None);
    };

    if !maas.bbr.enabled {
        return Ok(None);
    }

    if maas.bbr.models.is_empty() {
        return Err(ExtProcError::Config(
            "maas.bbr.enabled is true but maas.bbr.models is empty".to_owned(),
        ));
    }

    let mut routing_state = RoutingState::new();
    add_configured_models(&mut routing_state, &maas.bbr.models)?;

    let trust_boundary = TrustBoundary::from_config(&maas.bbr.trust_boundary);

    Ok(Some(
        BbrProcessor::new(routing_state).with_trust_boundary(trust_boundary),
    ))
}

/// Insert model entries into routing state, rejecting empty authority and duplicates.
fn add_configured_models(routing_state: &mut RoutingState, models: &[ModelEntry]) -> Result<()> {
    let mut seen_models = HashSet::new();

    for entry in models {
        if entry.provider.authority.trim().is_empty() {
            return Err(ExtProcError::Config(format!(
                "model '{}' has an empty provider authority",
                entry.model
            )));
        }
        let key = entry.model.to_lowercase();
        if !seen_models.insert(key) {
            return Err(ExtProcError::Config(format!(
                "duplicate model name (case-insensitive): {}",
                entry.model
            )));
        }
        routing_state.add_model(&entry.model, entry.provider.clone());
    }

    Ok(())
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

    #[test]
    fn deny_unknown_fields_rejects_trust_boundary_typo() {
        let result: std::result::Result<ExtProcConfig, _> = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          authority: api.openai.com
    trust_boundary:
      strip_prefies: ["x-maas-"]
"#,
        );

        assert!(result.is_err(), "typo in trust_boundary field should be rejected");
    }

    #[test]
    fn deny_unknown_fields_rejects_provider_extra_key() {
        let result: std::result::Result<ExtProcConfig, _> = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          authority: api.openai.com
          cluster: openai-pool
"#,
        );

        assert!(result.is_err(), "unknown provider field should be rejected");
    }

    #[test]
    fn parse_bbr_config() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          authority: api.openai.com
          path_prefix: /v1
      - model: claude-3-opus
        provider:
          authority: api.anthropic.com
          path_prefix: /v1/messages
"#,
        )
        .unwrap();

        let maas = cfg.maas.expect("maas should be present");
        assert!(maas.bbr.enabled, "bbr should be enabled");
        assert_eq!(maas.bbr.models.len(), 2, "should have two models");
        assert_eq!(maas.bbr.models[0].model, "gpt-4");
        assert_eq!(maas.bbr.models[0].provider.authority, "api.openai.com");
    }

    #[test]
    fn bbr_disabled_returns_none() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: false
    models:
      - model: gpt-4
        provider:
          authority: api.openai.com
"#,
        )
        .unwrap();

        let bbr = build_bbr_processor(&cfg).unwrap();
        assert!(bbr.is_none(), "disabled bbr should return None");
    }

    #[test]
    fn bbr_enabled_returns_processor() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          authority: api.openai.com
          path_prefix: /v1
"#,
        )
        .unwrap();

        let bbr = build_bbr_processor(&cfg).unwrap();
        assert!(bbr.is_some(), "enabled bbr should return Some");
    }

    #[test]
    fn bbr_enabled_with_empty_models_errors() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models: []
"#,
        )
        .unwrap();

        let err = build_bbr_processor(&cfg).expect_err("empty models should error");
        assert!(
            err.to_string().contains("models is empty"),
            "error should mention empty models: {err}"
        );
    }

    #[test]
    fn bbr_duplicate_model_names_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: GPT-4
        provider:
          authority: api.openai.com
          path_prefix: /v1
      - model: gpt-4
        provider:
          authority: api.anthropic.com
          path_prefix: /v1
"#,
        )
        .unwrap();

        let err = build_bbr_processor(&cfg).expect_err("duplicate model names should fail");
        assert!(
            err.to_string().contains("duplicate model name"),
            "error should mention duplicate model: {err}"
        );
    }

    #[test]
    fn bbr_empty_provider_authority_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          path_prefix: /v1
"#,
        )
        .unwrap();

        let err = build_bbr_processor(&cfg).expect_err("empty authority should fail");
        assert!(
            err.to_string().contains("empty provider authority"),
            "error should mention empty authority: {err}"
        );
    }

    #[test]
    fn bbr_whitespace_provider_authority_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          authority: "   "
          path_prefix: /v1
"#,
        )
        .unwrap();

        let err = build_bbr_processor(&cfg).expect_err("whitespace authority should fail");
        assert!(
            err.to_string().contains("empty provider authority"),
            "error should mention empty authority: {err}"
        );
    }

    #[test]
    fn no_maas_config_returns_none() {
        let cfg: ExtProcConfig = serde_yaml::from_str("{}").unwrap();

        let bbr = build_bbr_processor(&cfg).unwrap();
        assert!(bbr.is_none(), "no maas config should return None");
    }

    #[test]
    fn build_pipeline_registers_bbr_processor() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains: []
maas:
  bbr:
    enabled: true
    models:
      - model: gpt-4
        provider:
          authority: api.openai.com
          path_prefix: /v1
insecure_options:
  allow_unbounded_body: true
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let pipeline = build_pipeline(&cfg, &registry).unwrap();
        let mut extensions = praxis_filter::RequestExtensions::new();
        pipeline.prepare_extensions(&mut extensions);
        assert!(
            extensions.get::<BbrProcessor>().is_some(),
            "enabled BBR should be injected into request extensions"
        );
    }

    #[test]
    fn build_pipeline_omits_bbr_when_disabled() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains: []
insecure_options:
  allow_unbounded_body: true
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let pipeline = build_pipeline(&cfg, &registry).unwrap();
        let mut extensions = praxis_filter::RequestExtensions::new();
        pipeline.prepare_extensions(&mut extensions);
        assert!(
            extensions.get::<BbrProcessor>().is_none(),
            "disabled BBR should not be present in request extensions"
        );
    }
}
