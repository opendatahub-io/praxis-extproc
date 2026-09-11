// SPDX-License-Identifier: MIT

//! `MaaS` model-to-provider routing state.
//!
//! Maintains the mapping from model names to authorized providers.
//! This state is the source of truth for routing decisions.
//!
//! # Example
//!
//! ```
//! use praxis_extproc::maas::routing::{ProviderConfig, RoutingState};
//!
//! let mut state = RoutingState::new();
//! state.add_model(
//!     "gpt-4",
//!     ProviderConfig {
//!         authority: "api.openai.com".to_owned(),
//!         path_prefix: "/v1".to_owned(),
//!         ..Default::default()
//!     },
//! );
//!
//! let provider = state.resolve_model("gpt-4");
//! assert!(provider.is_some());
//! assert_eq!(provider.unwrap().authority, "api.openai.com");
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// Provider Configuration
// -----------------------------------------------------------------------------

/// Configuration for a single provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProviderConfig {
    /// Target authority (host:port) for this provider.
    ///
    /// Example: `"api.openai.com"`, `"claude.anthropic.com"`
    pub authority: String,

    /// Path prefix for provider API.
    ///
    /// Example: `"/v1"` for `OpenAI`, `"/v1/messages"` for Anthropic
    #[serde(default)]
    pub path_prefix: String,

    /// Optional effective model name to use with this provider.
    ///
    /// Some providers use different model identifiers than `MaaS`.
    /// When set, the request model field is rewritten to this value.
    #[serde(default)]
    pub effective_model: Option<String>,

    /// Additional headers to set for this provider.
    #[serde(default)]
    pub extra_headers: HashMap<String, String>,
}

// -----------------------------------------------------------------------------
// Model Entry
// -----------------------------------------------------------------------------

/// A model entry with its authorized provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelEntry {
    /// The model name (e.g., "gpt-4", "claude-3-opus").
    pub model: String,
    /// The authorized provider configuration.
    pub provider: ProviderConfig,
}

// -----------------------------------------------------------------------------
// Routing State
// -----------------------------------------------------------------------------

/// In-memory routing state for model-to-provider resolution.
///
/// Provides a lookup table from model names to provider configurations.
/// Initially populated from configuration; designed for later extension
/// to read from Kubernetes state (Issue #7).
#[derive(Debug, Clone, Default)]
pub struct RoutingState {
    /// Model name → provider configuration.
    models: HashMap<String, ProviderConfig>,
}

impl RoutingState {
    /// Create a new empty routing state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create routing state from a list of model entries.
    ///
    /// Model names are normalized to lowercase for case-insensitive lookup.
    pub fn from_entries(entries: &[ModelEntry]) -> Self {
        let models = entries
            .iter()
            .map(|e| (e.model.to_lowercase(), e.provider.clone()))
            .collect();
        Self { models }
    }

    /// Add or update a model mapping.
    ///
    /// Model names are normalized to lowercase for case-insensitive lookup.
    pub fn add_model(&mut self, model: &str, provider: ProviderConfig) {
        self.models.insert(model.to_lowercase(), provider);
    }

    /// Remove a model mapping.
    ///
    /// Model names are normalized to lowercase for case-insensitive lookup.
    pub fn remove_model(&mut self, model: &str) -> Option<ProviderConfig> {
        self.models.remove(&model.to_lowercase())
    }

    /// Resolve a model name to its provider configuration.
    ///
    /// Model names are normalized to lowercase for case-insensitive lookup.
    /// Returns `None` if the model is not found.
    pub fn resolve_model(&self, model: &str) -> Option<&ProviderConfig> {
        self.models.get(&model.to_lowercase())
    }

    /// Check if a model exists in the routing state.
    ///
    /// Model names are normalized to lowercase for case-insensitive lookup.
    pub fn has_model(&self, model: &str) -> bool {
        self.models.contains_key(&model.to_lowercase())
    }

    /// Get the number of registered models.
    pub fn model_count(&self) -> usize {
        self.models.len()
    }

    /// Iterate over all model entries.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &ProviderConfig)> {
        self.models.iter()
    }
}

// -----------------------------------------------------------------------------
// Path Utilities
// -----------------------------------------------------------------------------

/// Join a path prefix and a request path, handling slash normalization.
///
/// Ensures exactly one slash between prefix and path:
/// - `"/api"` + `"/v1/chat"` → `"/api/v1/chat"`
/// - `"/api/"` + `"/v1/chat"` → `"/api/v1/chat"`
/// - `"/api"` + `"v1/chat"` → `"/api/v1/chat"`
/// - `""` + `"/v1/chat"` → `"/v1/chat"`
fn join_paths(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        return path.to_owned();
    }

    let prefix_trimmed = prefix.trim_end_matches('/');
    let path_trimmed = path.trim_start_matches('/');

    if path_trimmed.is_empty() {
        format!("{prefix_trimmed}/")
    } else {
        format!("{prefix_trimmed}/{path_trimmed}")
    }
}

// -----------------------------------------------------------------------------
// Routing Resolution Result
// -----------------------------------------------------------------------------

/// Result of resolving a model to routing decisions.
#[derive(Debug, Clone)]
pub struct RoutingDecision {
    /// Target authority (host:port).
    pub authority: String,
    /// Target path (with any rewriting applied).
    pub path: String,
    /// Effective model name (may differ from requested).
    pub effective_model: String,
    /// Additional headers to set.
    pub extra_headers: HashMap<String, String>,
}

impl RoutingDecision {
    /// Create a routing decision from a provider config.
    pub fn from_provider(provider: &ProviderConfig, requested_model: &str, request_path: &str) -> Self {
        let path = join_paths(&provider.path_prefix, request_path);

        let effective_model = provider
            .effective_model
            .as_ref()
            .cloned()
            .unwrap_or_else(|| requested_model.to_owned());

        Self {
            authority: provider.authority.clone(),
            path,
            effective_model,
            extra_headers: provider.extra_headers.clone(),
        }
    }
}

// -----------------------------------------------------------------------------
// Routing Error
// -----------------------------------------------------------------------------

/// Errors that can occur during routing resolution.
#[derive(Debug, Clone, thiserror::Error)]
pub enum RoutingError {
    /// The requested model was not found.
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// The model name is empty or missing.
    #[error("model name is empty or missing")]
    ModelMissing,

    /// The provider config is unusable (e.g. empty authority).
    #[error("provider not found or invalid for model: {0}")]
    ProviderNotFound(String),
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_empty() {
        let state = RoutingState::new();
        assert_eq!(state.model_count(), 0, "new state should be empty");
    }

    #[test]
    fn add_and_resolve_model() {
        let mut state = RoutingState::new();
        state.add_model(
            "gpt-4",
            ProviderConfig {
                authority: "api.openai.com".to_owned(),
                path_prefix: "/v1".to_owned(),
                ..Default::default()
            },
        );

        let provider = state.resolve_model("gpt-4");
        assert!(provider.is_some(), "should resolve gpt-4");
        assert_eq!(provider.unwrap().authority, "api.openai.com", "authority should match");
    }

    #[test]
    fn resolve_nonexistent_model_returns_none() {
        let state = RoutingState::new();
        assert!(state.resolve_model("nonexistent").is_none(), "should return None");
    }

    #[test]
    fn remove_model() {
        let mut state = RoutingState::new();
        state.add_model(
            "gpt-4",
            ProviderConfig {
                authority: "api.openai.com".to_owned(),
                ..Default::default()
            },
        );

        let removed = state.remove_model("gpt-4");
        assert!(removed.is_some(), "should return removed provider");
        assert!(!state.has_model("gpt-4"), "model should be removed");
    }

    #[test]
    fn from_entries() {
        let entries = vec![
            ModelEntry {
                model: "gpt-4".to_owned(),
                provider: ProviderConfig {
                    authority: "api.openai.com".to_owned(),
                    ..Default::default()
                },
            },
            ModelEntry {
                model: "claude-3".to_owned(),
                provider: ProviderConfig {
                    authority: "api.anthropic.com".to_owned(),
                    ..Default::default()
                },
            },
        ];

        let state = RoutingState::from_entries(&entries);
        assert_eq!(state.model_count(), 2, "should have 2 models");
        assert!(state.has_model("gpt-4"), "should have gpt-4");
        assert!(state.has_model("claude-3"), "should have claude-3");
    }

    #[test]
    fn routing_decision_from_provider() {
        let provider = ProviderConfig {
            authority: "api.openai.com".to_owned(),
            path_prefix: "/v1".to_owned(),
            effective_model: Some("gpt-4-turbo".to_owned()),
            ..Default::default()
        };

        let decision = RoutingDecision::from_provider(&provider, "gpt-4", "/chat/completions");

        assert_eq!(decision.authority, "api.openai.com", "authority should match");
        assert_eq!(decision.path, "/v1/chat/completions", "path should include prefix");
        assert_eq!(decision.effective_model, "gpt-4-turbo", "should use effective model");
    }

    #[test]
    fn routing_decision_no_effective_model() {
        let provider = ProviderConfig {
            authority: "api.openai.com".to_owned(),
            ..Default::default()
        };

        let decision = RoutingDecision::from_provider(&provider, "gpt-4", "/chat/completions");

        assert_eq!(decision.effective_model, "gpt-4", "should use requested model");
    }

    #[test]
    fn routing_decision_no_path_prefix() {
        let provider = ProviderConfig {
            authority: "api.openai.com".to_owned(),
            ..Default::default()
        };

        let decision = RoutingDecision::from_provider(&provider, "gpt-4", "/chat/completions");

        assert_eq!(decision.path, "/chat/completions", "path should be unchanged");
    }

    #[test]
    fn has_model() {
        let mut state = RoutingState::new();
        assert!(!state.has_model("gpt-4"), "should not have gpt-4");

        state.add_model(
            "gpt-4",
            ProviderConfig {
                authority: "api.openai.com".to_owned(),
                ..Default::default()
            },
        );
        assert!(state.has_model("gpt-4"), "should have gpt-4");
    }

    #[test]
    fn iter_models() {
        let mut state = RoutingState::new();
        state.add_model(
            "gpt-4",
            ProviderConfig {
                authority: "openai".to_owned(),
                ..Default::default()
            },
        );
        state.add_model(
            "claude",
            ProviderConfig {
                authority: "anthropic".to_owned(),
                ..Default::default()
            },
        );

        let models: Vec<_> = state.iter().collect();
        assert_eq!(models.len(), 2, "should iterate over 2 models");
    }

    #[test]
    fn case_insensitive_model_lookup() {
        let mut state = RoutingState::new();
        state.add_model(
            "GPT-4",
            ProviderConfig {
                authority: "api.openai.com".to_owned(),
                ..Default::default()
            },
        );

        assert!(state.resolve_model("gpt-4").is_some(), "lowercase should match");
        assert!(state.resolve_model("GPT-4").is_some(), "uppercase should match");
        assert!(state.resolve_model("Gpt-4").is_some(), "mixed case should match");
        assert!(state.has_model("gpt-4"), "has_model should be case-insensitive");
    }

    #[test]
    fn case_insensitive_remove() {
        let mut state = RoutingState::new();
        state.add_model(
            "GPT-4",
            ProviderConfig {
                authority: "api.openai.com".to_owned(),
                ..Default::default()
            },
        );

        let removed = state.remove_model("gpt-4");
        assert!(removed.is_some(), "remove should be case-insensitive");
        assert!(!state.has_model("GPT-4"), "model should be removed");
    }

    #[test]
    fn join_paths_normalizes_slashes() {
        let cases = [
            ("/api/", "/v1/chat", "/api/v1/chat"),
            ("/api", "/v1/chat", "/api/v1/chat"),
            ("/api", "v1/chat", "/api/v1/chat"),
            ("", "/v1/chat", "/v1/chat"),
            ("/api", "", "/api/"),
        ];

        for (prefix, path, expected) in cases {
            assert_eq!(join_paths(prefix, path), expected, "join_paths({prefix:?}, {path:?})");
        }
    }
}
