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

    /// Maximum seconds to drain in-flight streams on shutdown before
    /// forcefully cancelling them.
    ///
    /// Defaults to [`DrainTimeoutSecs::default`]; must be greater than zero.
    #[serde(default)]
    pub shutdown_drain_timeout_secs: DrainTimeoutSecs,

    /// Maximum accumulated request/response body size, in bytes, before a
    /// stream is rejected with `RESOURCE_EXHAUSTED`.
    ///
    /// Defaults to [`DEFAULT_MAX_BODY_BYTES`]. Ignored when
    /// `insecure_options.allow_unbounded_body` is set, which lifts the cap
    /// entirely.
    #[serde(default)]
    pub max_body_bytes: MaxBodyBytes,

    /// Kubernetes `TokenReview` + `SubjectAccessReview` authentication
    /// for the metrics endpoint.
    #[serde(default)]
    pub metrics_auth: MetricsAuthConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            grpc_address: "0.0.0.0:50051".to_owned(),
            health_address: "0.0.0.0:50052".to_owned(),
            metrics_address: "0.0.0.0:9090".to_owned(),
            tls: crate::tls::TlsConfig::default(),
            shutdown_drain_timeout_secs: DrainTimeoutSecs::default(),
            max_body_bytes: MaxBodyBytes::default(),
            metrics_auth: MetricsAuthConfig::default(),
        }
    }
}

/// Graceful-drain deadline in seconds, guaranteed non-zero at parse time.
///
/// Constrained numeric parsed via `#[serde(try_from = "u64")]`, so an invalid
/// (zero) value is rejected during deserialization rather than at a later
/// validation step.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "u64")]
pub struct DrainTimeoutSecs(std::num::NonZeroU64);

impl DrainTimeoutSecs {
    /// The configured drain deadline, in seconds.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl Default for DrainTimeoutSecs {
    /// 20s, chosen to fit inside the common 30s Kubernetes
    /// `terminationGracePeriodSeconds` with headroom for a preStop lameduck
    /// and final cleanup before SIGKILL.
    fn default() -> Self {
        // 20 is non-zero, so the fallback arm is never taken.
        Self(match std::num::NonZeroU64::new(20) {
            Some(v) => v,
            None => std::num::NonZeroU64::MIN,
        })
    }
}

impl TryFrom<u64> for DrainTimeoutSecs {
    type Error = &'static str;

    fn try_from(value: u64) -> std::result::Result<Self, Self::Error> {
        std::num::NonZeroU64::new(value)
            .map(Self)
            .ok_or("shutdown_drain_timeout_secs must be greater than zero")
    }
}

/// Default maximum accumulated body size (10 MiB) before rejecting a stream.
pub const DEFAULT_MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Maximum accumulated request/response body size in bytes, guaranteed non-zero
/// at parse time.
///
/// Constrained numeric parsed via `#[serde(try_from = "usize")]`, so a zero
/// value is rejected during deserialization rather than at a later validation
/// step. Superseded by `insecure_options.allow_unbounded_body`, which removes
/// the cap entirely; see [`ExtProcConfig::max_body_accumulation`].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(try_from = "usize")]
pub struct MaxBodyBytes(std::num::NonZeroUsize);

impl MaxBodyBytes {
    /// The configured ceiling, in bytes.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for MaxBodyBytes {
    /// 10 MiB, matching the historical fixed accumulation cap.
    fn default() -> Self {
        // DEFAULT_MAX_BODY_BYTES is non-zero, so the fallback arm is never taken.
        Self(match std::num::NonZeroUsize::new(DEFAULT_MAX_BODY_BYTES) {
            Some(v) => v,
            None => std::num::NonZeroUsize::MIN,
        })
    }
}

impl TryFrom<usize> for MaxBodyBytes {
    type Error = &'static str;

    fn try_from(value: usize) -> std::result::Result<Self, Self::Error> {
        std::num::NonZeroUsize::new(value)
            .map(Self)
            .ok_or("max_body_bytes must be greater than zero")
    }
}

impl ExtProcConfig {
    /// Effective body-accumulation ceiling in bytes; `None` means unbounded.
    ///
    /// Returns `None` when `insecure_options.allow_unbounded_body` is set,
    /// otherwise the configured [`ServerConfig::max_body_bytes`].
    #[must_use]
    pub fn max_body_accumulation(&self) -> Option<usize> {
        if self.insecure_options.allow_unbounded_body {
            None
        } else {
            Some(self.server.max_body_bytes.get())
        }
    }
}

// -----------------------------------------------------------------------------
// MetricsAuthConfig
// -----------------------------------------------------------------------------

/// Kubernetes-native authentication for the Prometheus metrics endpoint.
///
/// When enabled, the metrics server validates incoming
/// `Authorization: Bearer <token>` headers via the Kubernetes
/// `TokenReview` and `SubjectAccessReview` APIs. The bearer token
/// is expected to be a Kubernetes `ServiceAccount` token; the API
/// server verifies its validity (`TokenReview`) and checks that the
/// caller has `GET /metrics` permission (`SubjectAccessReview`).
///
/// Health probes (`/healthz`) remain unauthenticated so Kubernetes
/// liveness checks continue to work.
///
/// Enabled by default. Disable with `enabled: false` for local
/// development outside a Kubernetes cluster.
///
/// ```
/// use praxis_extproc::config::MetricsAuthConfig;
///
/// let cfg = MetricsAuthConfig::default();
/// assert!(cfg.enabled);
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MetricsAuthConfig {
    /// Enable Kubernetes `TokenReview` + `SubjectAccessReview`
    /// authentication on `/metrics`.
    pub enabled: bool,
}

impl Default for MetricsAuthConfig {
    fn default() -> Self {
        Self { enabled: true }
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
    fn shutdown_drain_timeout_defaults() {
        let cfg: ExtProcConfig = serde_yaml::from_str("{}").unwrap();

        assert_eq!(
            cfg.server.shutdown_drain_timeout_secs.get(),
            20,
            "drain timeout should default to 20s"
        );
    }

    #[test]
    fn parse_custom_shutdown_drain_timeout() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  shutdown_drain_timeout_secs: 5
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.server.shutdown_drain_timeout_secs.get(),
            5,
            "drain timeout should match"
        );
    }

    #[test]
    fn zero_shutdown_drain_timeout_rejected() {
        let result: std::result::Result<ExtProcConfig, _> = serde_yaml::from_str(
            r#"
server:
  shutdown_drain_timeout_secs: 0
"#,
        );

        let err = result.expect_err("zero drain timeout should be rejected at parse time");
        assert!(
            err.to_string().contains("shutdown_drain_timeout_secs"),
            "error should name the field: {err}"
        );
    }

    #[test]
    fn max_body_bytes_defaults() {
        let cfg: ExtProcConfig = serde_yaml::from_str("{}").unwrap();

        assert_eq!(
            cfg.server.max_body_bytes.get(),
            DEFAULT_MAX_BODY_BYTES,
            "max_body_bytes should default to 10 MiB"
        );
        assert_eq!(
            cfg.max_body_accumulation(),
            Some(DEFAULT_MAX_BODY_BYTES),
            "effective limit should be the default when bounded"
        );
    }

    #[test]
    fn parse_custom_max_body_bytes() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  max_body_bytes: 52428800
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.server.max_body_bytes.get(),
            52_428_800,
            "max_body_bytes should match"
        );
        assert_eq!(
            cfg.max_body_accumulation(),
            Some(52_428_800),
            "effective limit should reflect the configured ceiling"
        );
    }

    #[test]
    fn zero_max_body_bytes_rejected() {
        let result: std::result::Result<ExtProcConfig, _> = serde_yaml::from_str(
            r#"
server:
  max_body_bytes: 0
"#,
        );

        let err = result.expect_err("zero max_body_bytes should be rejected at parse time");
        assert!(
            err.to_string().contains("max_body_bytes"),
            "error should name the field: {err}"
        );
    }

    #[test]
    fn allow_unbounded_body_disables_accumulation_limit() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
insecure_options:
  allow_unbounded_body: true
server:
  max_body_bytes: 1024
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.max_body_accumulation(),
            None,
            "allow_unbounded_body should lift the accumulation cap"
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

    // -------------------------------------------------------------------------
    // MetricsAuthConfig
    // -------------------------------------------------------------------------

    #[test]
    fn metrics_auth_defaults_to_enabled() {
        let cfg = MetricsAuthConfig::default();

        assert!(cfg.enabled, "auth should be enabled by default");
    }

    #[test]
    fn parse_metrics_auth_enabled_from_yaml() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  metrics_auth:
    enabled: true
"#,
        )
        .unwrap();

        assert!(cfg.server.metrics_auth.enabled, "enabled should be true");
    }

    #[test]
    fn parse_metrics_auth_disabled_from_yaml() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  metrics_auth:
    enabled: false
"#,
        )
        .unwrap();

        assert!(!cfg.server.metrics_auth.enabled, "enabled should be false");
    }

    #[test]
    fn parse_config_without_metrics_auth_defaults_to_enabled() {
        let cfg: ExtProcConfig = serde_yaml::from_str(
            r#"
server:
  grpc_address: "0.0.0.0:9004"
"#,
        )
        .unwrap();

        assert!(cfg.server.metrics_auth.enabled, "auth should default to enabled");
    }

    #[test]
    fn metrics_auth_rejects_unknown_fields() {
        let result: std::result::Result<ExtProcConfig, _> = serde_yaml::from_str(
            r#"
server:
  metrics_auth:
    enabled: true
    bogus_field: true
"#,
        );

        assert!(result.is_err(), "unknown fields in metrics_auth should be rejected");
    }
}
