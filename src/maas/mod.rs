// SPDX-License-Identifier: MIT

//! `MaaS` (Models as a Service) routing and trust boundary.
//!
//! Implements body-based routing for AI model requests and enforces
//! the trust boundary between consumers and providers.
//!
//! # Overview
//!
//! This module provides the core components for `MaaS` body-based routing:
//!
//! - [`TrustBoundary`]: Identifies headers to strip from consumer requests
//! - [`RoutingState`]: Maintains model-to-provider mappings
//! - [`BbrProcessor`]: Combines extraction, resolution, and boundary enforcement
//!
//! # Example
//!
//! ```
//! use praxis_extproc::maas::{BbrProcessor, ProviderConfig, RoutingState};
//!
//! // Set up routing state
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
//! // Create processor
//! let processor = BbrProcessor::new(state);
//!
//! // Process a request (model comes from X-AI-Model header)
//! let mut headers = http::HeaderMap::new();
//! headers.insert(
//!     "content-type",
//!     http::HeaderValue::from_static("application/json"),
//! );
//! let result = processor.process_request("gpt-4", "/chat/completions", &headers);
//!
//! assert!(result.is_ok());
//! ```

pub mod bbr;
pub mod routing;
pub mod trust_boundary;

pub use bbr::{BbrProcessor, BbrResult};
pub use routing::{ModelEntry, ProviderConfig, RoutingDecision, RoutingError, RoutingState};
pub use trust_boundary::{TrustBoundary, TrustBoundaryConfig};
