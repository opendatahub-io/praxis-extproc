// SPDX-License-Identifier: MIT

//! Trusted header boundary enforcement.
//!
//! Strips consumer-supplied headers that could influence routing decisions
//! or bypass authorization. This prevents consumers from:
//!
//! - Forging internal routing headers (X-MaaS-*, X-Provider-*)
//! - Passing their credentials to backend providers
//! - Overriding authorization results
//!
//! # Example
//!
//! ```
//! use http::{HeaderMap, HeaderValue};
//! use praxis_extproc::maas::TrustBoundary;
//!
//! let boundary = TrustBoundary::default();
//! let mut headers = HeaderMap::new();
//! headers.insert("x-maas-provider", HeaderValue::from_static("forged"));
//! headers.insert("x-request-id", HeaderValue::from_static("abc123"));
//! headers.insert("authorization", HeaderValue::from_static("Bearer token"));
//!
//! let headers_to_strip = boundary.headers_to_remove(&headers);
//!
//! assert!(headers_to_strip.contains(&"x-maas-provider".to_owned()));
//! assert!(headers_to_strip.contains(&"authorization".to_owned()));
//! assert!(!headers_to_strip.contains(&"x-request-id".to_owned()));
//! ```

use http::HeaderMap;
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------------

/// Configuration for the trust boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TrustBoundaryConfig {
    /// Header prefixes to strip (case-insensitive).
    ///
    /// Headers starting with any of these prefixes will be removed.
    /// Default: `["x-maas-", "x-provider-"]`
    pub strip_prefixes: Vec<String>,

    /// Exact header names to strip (case-insensitive).
    ///
    /// Default: `["authorization"]`
    pub strip_headers: Vec<String>,

    /// Whether to strip the Authorization header.
    ///
    /// Default: `true`
    pub strip_authorization: bool,
}

impl Default for TrustBoundaryConfig {
    fn default() -> Self {
        Self {
            strip_prefixes: vec!["x-maas-".to_owned(), "x-provider-".to_owned()],
            strip_headers: vec![],
            strip_authorization: true,
        }
    }
}

// -----------------------------------------------------------------------------
// Trust Boundary
// -----------------------------------------------------------------------------

/// Enforces the trust boundary between consumers and providers.
///
/// Identifies headers that must be stripped from consumer requests
/// before routing decisions are made or requests are forwarded to
/// backend providers.
#[derive(Debug, Clone)]
pub struct TrustBoundary {
    /// Lowercase header prefixes to strip.
    strip_prefixes: Vec<String>,
    /// Lowercase exact header names to strip.
    strip_headers: Vec<String>,
    /// Whether to strip Authorization header.
    strip_authorization: bool,
}

impl Default for TrustBoundary {
    fn default() -> Self {
        Self::from_config(&TrustBoundaryConfig::default())
    }
}

impl TrustBoundary {
    /// Create a new trust boundary from configuration.
    pub fn from_config(config: &TrustBoundaryConfig) -> Self {
        Self {
            strip_prefixes: config.strip_prefixes.iter().map(|p| p.to_lowercase()).collect(),
            strip_headers: config.strip_headers.iter().map(|h| h.to_lowercase()).collect(),
            strip_authorization: config.strip_authorization,
        }
    }

    /// Check if a header should be stripped.
    pub fn should_strip(&self, header_name: &str) -> bool {
        let lower = header_name.to_lowercase();

        // Check exact matches first
        if self.strip_headers.iter().any(|h| h == &lower) {
            return true;
        }

        // Check authorization
        if self.strip_authorization && lower == "authorization" {
            return true;
        }

        // Check prefixes
        self.strip_prefixes.iter().any(|prefix| lower.starts_with(prefix))
    }

    /// Return header names that should be removed from the request.
    ///
    /// # Arguments
    ///
    /// * `headers` - Request headers to inspect
    ///
    /// # Returns
    ///
    /// Vector of header names (as owned strings) that should be stripped.
    pub fn headers_to_remove(&self, headers: &HeaderMap) -> Vec<String> {
        self.filter_header_names(headers.keys().map(http::HeaderName::as_str))
    }

    /// Return header names that should be removed, from an iterator.
    ///
    /// # Arguments
    ///
    /// * `header_names` - Iterator of header names
    ///
    /// # Returns
    ///
    /// Vector of header names that should be stripped.
    pub fn filter_header_names<'a, I>(&self, header_names: I) -> Vec<String>
    where
        I: Iterator<Item = &'a str>,
    {
        header_names
            .filter(|name| self.should_strip(name))
            .map(ToOwned::to_owned)
            .collect()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_should_strip_rules() {
        let boundary = TrustBoundary::default();

        let strip = [
            "x-maas-provider",
            "x-maas-route",
            "X-MaaS-Model",
            "x-provider-key",
            "X-Provider-Auth",
            "authorization",
            "Authorization",
        ];
        for name in strip {
            assert!(boundary.should_strip(name), "should strip {name}");
        }

        let keep = ["content-type", "x-request-id", "accept"];
        for name in keep {
            assert!(!boundary.should_strip(name), "should keep {name}");
        }
    }

    #[test]
    fn custom_config_strips_custom_prefixes() {
        let config = TrustBoundaryConfig {
            strip_prefixes: vec!["x-internal-".to_owned()],
            strip_headers: vec![],
            strip_authorization: false,
        };
        let boundary = TrustBoundary::from_config(&config);

        assert!(
            boundary.should_strip("x-internal-secret"),
            "should strip x-internal-secret"
        );
        assert!(
            !boundary.should_strip("x-maas-provider"),
            "should not strip x-maas-provider"
        );
        assert!(
            !boundary.should_strip("authorization"),
            "should not strip authorization"
        );
    }

    #[test]
    fn custom_config_strips_exact_headers() {
        let config = TrustBoundaryConfig {
            strip_prefixes: vec![],
            strip_headers: vec!["x-api-key".to_owned()],
            strip_authorization: false,
        };
        let boundary = TrustBoundary::from_config(&config);

        assert!(boundary.should_strip("x-api-key"), "should strip x-api-key");
        assert!(boundary.should_strip("X-Api-Key"), "should strip X-Api-Key (case)");
        assert!(!boundary.should_strip("x-api-key-id"), "should not strip x-api-key-id");
    }

    #[test]
    fn headers_to_remove_filters_correctly() {
        let boundary = TrustBoundary::default();

        let mut headers = HeaderMap::new();
        headers.insert("x-maas-provider", http::HeaderValue::from_static("forged"));
        headers.insert("x-request-id", http::HeaderValue::from_static("abc123"));
        headers.insert("authorization", http::HeaderValue::from_static("Bearer token"));
        headers.insert("content-type", http::HeaderValue::from_static("application/json"));

        let to_remove = boundary.headers_to_remove(&headers);

        assert_eq!(to_remove.len(), 2, "should remove 2 headers");
        assert!(
            to_remove.contains(&"x-maas-provider".to_owned()),
            "should include x-maas-provider"
        );
        assert!(
            to_remove.contains(&"authorization".to_owned()),
            "should include authorization"
        );
    }

    #[test]
    fn disabled_authorization_stripping() {
        let config = TrustBoundaryConfig {
            strip_authorization: false,
            ..Default::default()
        };
        let boundary = TrustBoundary::from_config(&config);

        assert!(
            !boundary.should_strip("authorization"),
            "should not strip authorization"
        );
        assert!(
            boundary.should_strip("x-maas-provider"),
            "should still strip x-maas-provider"
        );
    }
}
