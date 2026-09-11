// SPDX-License-Identifier: MIT

//! Body-Based Routing (BBR) processor for `MaaS` requests.
//!
//! Resolves routing decisions based on the model name (extracted by the
//! `model_to_header` filter) and produces the mutations needed for Envoy
//! to forward requests to the correct backend provider.
//!
//! # Prerequisites
//!
//! The `model_to_header` filter from `praxis-ai-filters` must be configured
//! in the pipeline to extract the model name from the request body and place
//! it in the `X-AI-Model` header.
//!
//! # Example
//!
//! ```ignore
//! use praxis_extproc::maas::{BbrProcessor, RoutingState, ProviderConfig};
//!
//! let mut state = RoutingState::new();
//! state.add_model("gpt-4", ProviderConfig {
//!     authority: "api.openai.com".to_owned(),
//!     path_prefix: "/v1".to_owned(),
//!     ..Default::default()
//! });
//!
//! let processor = BbrProcessor::new(state);
//! let result = processor.process_request("gpt-4", "/chat/completions", &headers);
//! ```

use std::sync::Arc;

use http::HeaderMap;
use praxis_filter::{PipelineExtension, RequestExtensions};

use super::{
    routing::{RoutingDecision, RoutingError, RoutingState},
    trust_boundary::TrustBoundary,
};

// -----------------------------------------------------------------------------
// BBR Result
// -----------------------------------------------------------------------------

/// Result of BBR processing containing routing decisions and mutations.
#[derive(Debug, Clone)]
pub struct BbrResult {
    /// The routing decision for this request.
    pub decision: RoutingDecision,
    /// Headers to remove (from trust boundary).
    pub headers_to_remove: Vec<String>,
    /// Headers to add/set.
    pub headers_to_set: Vec<(String, String)>,
    /// Whether Envoy should clear its route cache.
    pub clear_route_cache: bool,
}

// -----------------------------------------------------------------------------
// BBR Processor
// -----------------------------------------------------------------------------

/// Body-Based Routing processor for `MaaS` requests.
///
/// Combines model extraction, routing resolution, and trust boundary
/// enforcement into a single processing step.
#[derive(Debug, Clone)]
pub struct BbrProcessor {
    /// Routing state for model-to-provider resolution.
    routing_state: Arc<RoutingState>,
    /// Trust boundary for header filtering.
    trust_boundary: TrustBoundary,
}

impl BbrProcessor {
    /// Create a new BBR processor with the given routing state.
    pub fn new(routing_state: RoutingState) -> Self {
        Self {
            routing_state: Arc::new(routing_state),
            trust_boundary: TrustBoundary::default(),
        }
    }

    /// Create a BBR processor with custom trust boundary configuration.
    #[must_use]
    pub fn with_trust_boundary(mut self, trust_boundary: TrustBoundary) -> Self {
        self.trust_boundary = trust_boundary;
        self
    }

    /// Process a request and determine routing based on the model name.
    ///
    /// The model name should be extracted by the `model_to_header` filter
    /// from the request body before calling this method.
    ///
    /// # Arguments
    ///
    /// * `model` - The model name (from `X-AI-Model` header)
    /// * `request_path` - The original request path
    /// * `request_headers` - Current request headers
    ///
    /// # Returns
    ///
    /// A `BbrResult` containing the routing decision and mutations, or
    /// a `RoutingError` if the request cannot be routed.
    ///
    /// # Errors
    ///
    /// Returns [`RoutingError::ModelMissing`] if the model is empty.
    /// Returns [`RoutingError::ModelNotFound`] if the model is not in routing state.
    /// Returns [`RoutingError::ProviderNotFound`] if the provider has an empty authority.
    #[expect(clippy::too_many_lines, reason = "linear validation and mutation building")]
    pub fn process_request(
        &self,
        model: &str,
        request_path: &str,
        request_headers: &HeaderMap,
    ) -> Result<BbrResult, RoutingError> {
        // Validate model is not empty
        if model.is_empty() {
            return Err(RoutingError::ModelMissing);
        }

        let model = model.to_owned();

        // Resolve model to provider
        let provider = self
            .routing_state
            .resolve_model(&model)
            .ok_or_else(|| RoutingError::ModelNotFound(model.clone()))?;

        if provider.authority.trim().is_empty() {
            return Err(RoutingError::ProviderNotFound(model));
        }

        // Build routing decision
        let decision = RoutingDecision::from_provider(provider, &model, request_path);

        // Determine headers to remove (trust boundary)
        let headers_to_remove = self.trust_boundary.headers_to_remove(request_headers);

        // Build headers to set
        let mut headers_to_set = Vec::new();

        // Add authority
        headers_to_set.push((":authority".to_owned(), decision.authority.clone()));

        // Add path if different
        if decision.path != request_path {
            headers_to_set.push((":path".to_owned(), decision.path.clone()));
        }

        // Add model header for downstream routing
        headers_to_set.push(("x-model".to_owned(), decision.effective_model.clone()));

        // Add X-Effective-Model header when the effective model differs from requested
        if decision.effective_model != model {
            headers_to_set.push(("x-effective-model".to_owned(), decision.effective_model.clone()));
        }

        // Add any extra headers from provider config
        for (key, value) in &decision.extra_headers {
            headers_to_set.push((key.clone(), value.clone()));
        }

        Ok(BbrResult {
            decision,
            headers_to_remove,
            headers_to_set,
            clear_route_cache: true,
        })
    }

    /// Get a reference to the routing state.
    pub fn routing_state(&self) -> &RoutingState {
        &self.routing_state
    }

    /// Get a reference to the trust boundary.
    pub fn trust_boundary(&self) -> &TrustBoundary {
        &self.trust_boundary
    }
}

impl PipelineExtension for BbrProcessor {
    /// Copy this processor onto the per-request extension map.
    fn prepare(&self, extensions: &mut RequestExtensions) {
        extensions.insert(self.clone());
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::{super::ProviderConfig, *};

    fn test_processor() -> BbrProcessor {
        let mut state = RoutingState::new();
        state.add_model(
            "gpt-4",
            ProviderConfig {
                authority: "api.openai.com".to_owned(),
                path_prefix: "/v1".to_owned(),
                ..Default::default()
            },
        );
        state.add_model(
            "claude-3",
            ProviderConfig {
                authority: "api.anthropic.com".to_owned(),
                path_prefix: "/v1".to_owned(),
                effective_model: Some("claude-3-opus-20240229".to_owned()),
                ..Default::default()
            },
        );
        BbrProcessor::new(state)
    }

    fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for &(name, value) in pairs {
            headers.insert(
                http::HeaderName::try_from(name).expect("valid header name"),
                http::HeaderValue::try_from(value).expect("valid header value"),
            );
        }
        headers
    }

    #[test]
    fn process_request_success() {
        let processor = test_processor();
        let headers = header_map(&[("content-type", "application/json")]);

        let result = processor
            .process_request("gpt-4", "/chat/completions", &headers)
            .expect("should process");

        assert_eq!(result.decision.authority, "api.openai.com", "authority should match");
        assert_eq!(
            result.decision.path, "/v1/chat/completions",
            "path should include prefix"
        );
        assert!(result.clear_route_cache, "should clear route cache");

        let authority_set = result
            .headers_to_set
            .iter()
            .any(|(k, v)| k == ":authority" && v == "api.openai.com");
        assert!(authority_set, "should set :authority");

        let model_set = result
            .headers_to_set
            .iter()
            .any(|(k, v)| k == "x-model" && v == "gpt-4");
        assert!(model_set, "should set x-model");

        let effective_model_set = result.headers_to_set.iter().any(|(k, _)| k == "x-effective-model");
        assert!(
            !effective_model_set,
            "should not set x-effective-model when model is not remapped"
        );
    }

    #[test]
    fn process_request_empty_model() {
        let processor = test_processor();
        let headers = HeaderMap::new();

        let result = processor.process_request("", "/chat/completions", &headers);

        assert!(result.is_err(), "should error on empty model");
        assert!(
            matches!(result.unwrap_err(), RoutingError::ModelMissing),
            "should be ModelMissing error"
        );
    }

    #[test]
    fn process_request_unknown_model() {
        let processor = test_processor();
        let headers = HeaderMap::new();

        let result = processor.process_request("unknown", "/chat/completions", &headers);

        assert!(result.is_err(), "should error on unknown model");
        assert!(
            matches!(result.unwrap_err(), RoutingError::ModelNotFound(_)),
            "should be ModelNotFound error"
        );
    }

    #[test]
    fn process_request_strips_internal_headers() {
        let processor = test_processor();
        let headers = header_map(&[
            ("content-type", "application/json"),
            ("x-maas-provider", "forged"),
            ("authorization", "Bearer token"),
        ]);

        let result = processor
            .process_request("gpt-4", "/chat/completions", &headers)
            .expect("should process");

        assert!(
            result.headers_to_remove.contains(&"x-maas-provider".to_owned()),
            "should remove x-maas-provider"
        );
        assert!(
            result.headers_to_remove.contains(&"authorization".to_owned()),
            "should remove authorization"
        );
    }

    #[test]
    fn process_request_effective_model() {
        let processor = test_processor();
        let headers = HeaderMap::new();

        let result = processor
            .process_request("claude-3", "/messages", &headers)
            .expect("should process");

        assert_eq!(
            result.decision.effective_model, "claude-3-opus-20240229",
            "should use effective model"
        );

        let model_set = result
            .headers_to_set
            .iter()
            .any(|(k, v)| k == "x-model" && v == "claude-3-opus-20240229");
        assert!(model_set, "should set effective model in x-model header");

        let effective_model_set = result
            .headers_to_set
            .iter()
            .any(|(k, v)| k == "x-effective-model" && v == "claude-3-opus-20240229");
        assert!(
            effective_model_set,
            "should set x-effective-model header when mapping differs"
        );
    }

    #[test]
    fn process_request_empty_authority() {
        let mut state = RoutingState::new();
        state.add_model(
            "bad-model",
            ProviderConfig {
                authority: "   ".to_owned(),
                ..Default::default()
            },
        );
        let processor = BbrProcessor::new(state);
        let headers = HeaderMap::new();

        let result = processor.process_request("bad-model", "/v1", &headers);

        assert!(result.is_err(), "should error on empty authority");
        assert!(
            matches!(result.unwrap_err(), RoutingError::ProviderNotFound(_)),
            "should be ProviderNotFound error"
        );
    }

    #[test]
    fn prepare_inserts_processor_into_extensions() {
        let processor = test_processor();
        let mut extensions = RequestExtensions::new();
        processor.prepare(&mut extensions);
        assert!(
            extensions.get::<BbrProcessor>().is_some(),
            "prepare should insert BbrProcessor into request extensions"
        );
    }
}
