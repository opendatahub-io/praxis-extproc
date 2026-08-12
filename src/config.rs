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
    profile::{NamedProfile, ProfilePipeline},
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
/// profiles:
///   - name: default
///     filter_chains:
///       - name: main
///         filters:
///           - filter: request_id
/// "#,
/// )
/// .unwrap();
/// assert_eq!(cfg.profiles.unwrap()[0].name, "default");
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtProcConfig {
    /// Filter chains executed before profile selection.
    #[serde(default)]
    pub pre_processing: Option<Vec<praxis_core::config::FilterChainConfig>>,

    /// Named processing profiles. Exactly one is selected per request.
    #[serde(default)]
    pub profiles: Option<Vec<ProfileConfig>>,

    /// Filter chains executed after the selected profile completes.
    #[serde(default)]
    pub post_processing: Option<Vec<praxis_core::config::FilterChainConfig>>,

    /// Security overrides for development.
    #[serde(default)]
    pub insecure_options: praxis_core::config::InsecureOptions,

    /// gRPC server settings.
    #[serde(default)]
    pub server: ServerConfig,
}

/// A named processing profile with its own filter chains.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    /// Profile identifier.
    pub name: String,
    /// Filter chains for this profile.
    pub filter_chains: Vec<praxis_core::config::FilterChainConfig>,
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
/// Returns [`ExtProcError::Pipeline`] if filter instantiation or validation fails.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
pub fn build_profile_pipeline(config: &ExtProcConfig, registry: &FilterRegistry) -> Result<Arc<ProfilePipeline>> {
    let pre = config
        .pre_processing
        .as_deref()
        .map(|chains| build_filter_pipeline(chains, registry, &config.insecure_options))
        .transpose()?;

    let mut profiles = Vec::new();
    if let Some(profile_configs) = &config.profiles {
        validate_profile_names(profile_configs)?;
        for pc in profile_configs {
            let pipeline = build_filter_pipeline(&pc.filter_chains, registry, &config.insecure_options)?;
            profiles.push(NamedProfile {
                name: Arc::from(pc.name.as_str()),
                pipeline,
            });
        }
    }

    if profiles.is_empty() {
        return Err(ExtProcError::Config("at least one profile is required".to_owned()));
    }

    let post = config
        .post_processing
        .as_deref()
        .map(|chains| build_filter_pipeline(chains, registry, &config.insecure_options))
        .transpose()?;

    Ok(Arc::new(ProfilePipeline::new(pre, profiles, post)))
}

/// Build a single [`FilterPipeline`] from a slice of filter chain configs.
fn build_filter_pipeline(
    chains: &[praxis_core::config::FilterChainConfig],
    registry: &FilterRegistry,
    insecure_options: &praxis_core::config::InsecureOptions,
) -> Result<FilterPipeline> {
    validate_chain_names(chains)?;

    let chain_map: std::collections::HashMap<&str, &[_]> =
        chains.iter().map(|c| (c.name.as_str(), c.filters.as_slice())).collect();

    let mut entries = flatten_chains(chains);

    let mut pipeline = FilterPipeline::build_with_chains(&mut entries, registry, &chain_map)
        .map_err(|e| ExtProcError::Pipeline(e.to_string()))?;

    pipeline
        .apply_body_limits(None, None, insecure_options.allow_unbounded_body)
        .map_err(|e| ExtProcError::Pipeline(e.to_string()))?;

    pipeline.apply_insecure_options(insecure_options);
    pipeline.add_pipeline_extension(Box::new(praxis_ai_apis::store::ResponseStoreRegistry::new()));

    Ok(pipeline)
}

/// Reject configs with duplicate profile names.
fn validate_profile_names(profiles: &[ProfileConfig]) -> Result<()> {
    let mut seen = HashSet::new();
    for profile in profiles {
        if !seen.insert(&profile.name) {
            return Err(ExtProcError::Config(format!(
                "duplicate profile name: {}",
                profile.name
            )));
        }
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
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: request_id
"#,
        )
        .unwrap();

        let profiles = cfg.profiles.unwrap();
        assert_eq!(profiles.len(), 1, "should have one profile");
        assert_eq!(profiles[0].name, "default", "profile name should match");
        assert_eq!(profiles[0].filter_chains.len(), 1, "should have one chain");
    }

    #[test]
    fn parse_empty_config_defaults() {
        let cfg: ExtProcConfig = serde_yaml::from_str("{}").unwrap();

        assert!(cfg.profiles.is_none(), "profiles should default to None");
        assert!(cfg.pre_processing.is_none(), "pre should default to None");
        assert!(cfg.post_processing.is_none(), "post should default to None");
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
profiles:
  - name: default
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
        let profile_pipeline = build_profile_pipeline(&cfg, &registry).unwrap();
        let pipeline = profile_pipeline.default_pipeline();

        assert_eq!(pipeline.len(), 2, "pipeline should have two filters");
    }

    #[test]
    fn build_pipeline_with_ai_filter() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: model_to_header
            header: X-AI-Model
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let profile_pipeline = build_profile_pipeline(&cfg, &registry).unwrap();
        let pipeline = profile_pipeline.default_pipeline();

        assert_eq!(pipeline.len(), 1, "pipeline should have one AI filter");
    }

    #[test]
    fn build_pipeline_unknown_filter_fails() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: nonexistent_filter
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let result = build_profile_pipeline(&cfg, &registry);

        assert!(result.is_err(), "unknown filter should fail");
    }

    #[test]
    fn flatten_multiple_chains() {
        let chains: Vec<praxis_core::config::FilterChainConfig> = serde_yaml::from_str(
            r#"
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

        let entries = flatten_chains(&chains);

        assert_eq!(entries.len(), 2, "should flatten both chains");
    }

    #[test]
    fn duplicate_chain_names_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
profiles:
  - name: default
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
        let err = build_profile_pipeline(&cfg, &registry)
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
bogus_key: true
"#,
        );

        assert!(result.is_err(), "unknown fields should be rejected");
    }

    #[test]
    fn parse_three_tier_config() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
pre_processing:
  - name: pre
    filters:
      - filter: request_id
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: headers
            request_add:
              - name: X-Test
                value: extproc
post_processing:
  - name: post
    filters:
      - filter: request_id
"#,
        )
        .unwrap();

        assert!(cfg.pre_processing.is_some(), "pre should be set");
        assert!(cfg.profiles.is_some(), "profiles should be set");
        assert!(cfg.post_processing.is_some(), "post should be set");
    }

    #[test]
    fn unknown_filter_in_pre_processing_fails() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
pre_processing:
  - name: pre
    filters:
      - filter: nonexistent_filter
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: request_id
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let result = build_profile_pipeline(&cfg, &registry);

        assert!(result.is_err(), "unknown filter in pre_processing should fail");
    }

    #[test]
    fn unknown_filter_in_post_processing_fails() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: request_id
post_processing:
  - name: post
    filters:
      - filter: nonexistent_filter
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let result = build_profile_pipeline(&cfg, &registry);

        assert!(result.is_err(), "unknown filter in post_processing should fail");
    }

    #[test]
    fn duplicate_profile_names_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
profiles:
  - name: same_name
    filter_chains:
      - name: a
        filters:
          - filter: request_id
  - name: same_name
    filter_chains:
      - name: b
        filters:
          - filter: request_id
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let err = build_profile_pipeline(&cfg, &registry)
            .err()
            .expect("duplicate profile names should fail");

        assert!(
            err.to_string().contains("duplicate profile name"),
            "error should mention duplicate profile: {err}"
        );
    }

    #[test]
    fn empty_profiles_list_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
profiles: []
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let err = build_profile_pipeline(&cfg, &registry)
            .err()
            .expect("empty profiles should fail");

        assert!(
            err.to_string().contains("at least one profile"),
            "error should mention missing profile: {err}"
        );
    }

    #[test]
    fn missing_profiles_rejected() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
pre_processing:
  - name: pre
    filters:
      - filter: request_id
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let err = build_profile_pipeline(&cfg, &registry)
            .err()
            .expect("config without profiles should fail");

        assert!(
            err.to_string().contains("at least one profile"),
            "error should mention missing profile: {err}"
        );
    }

    #[test]
    fn build_three_tier_pipeline() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
pre_processing:
  - name: pre
    filters:
      - filter: request_id
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: headers
            request_add:
              - name: X-Test
                value: extproc
post_processing:
  - name: post
    filters:
      - filter: request_id
"#,
        )
        .unwrap();

        let registry = praxis_ai_filters::build_ai_registry();
        let profile_pipeline = build_profile_pipeline(&cfg, &registry).unwrap();
        let pipeline = profile_pipeline.default_pipeline();

        assert_eq!(pipeline.len(), 1, "default profile should have one filter");
    }
}
