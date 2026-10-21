// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Request-header observability oracle for k8s e2e qualification.
//!
//! Non-production: compiled only with `--features k8s-e2e` and registered on
//! the AI filter registry from [`super::register_e2e_filters`].
//!
//! For each configured header name, reports the *effective* value that would be
//! forwarded upstream from this hop, accounting for the removals / sets / adds
//! queued by earlier filters in the chain (via
//! [`HttpFilterContext::pending_header_value`]) layered over the original
//! request headers. Place it AFTER the filters whose header behavior you want to
//! assert (e.g. `identity_header_guard`). The result is published as
//! `x-qualification-fwd-<name>[-hop-N]` response headers so tests can observe the
//! trust boundary without an echo backend.
//!
//! Per-header state is stashed in `filter_metadata` during `on_request` (where
//! the request headers and pending mutations are visible) and emitted during
//! `on_response`; metadata survives ExtProc phase boundaries (`CarriedContext`).

use async_trait::async_trait;
use bytes::Bytes;
use praxis_filter::{
    BodyAccess, FilterAction, FilterError, HttpFilter, HttpFilterContext, PendingHeaderResult, parse_filter_config,
};
use serde::Deserialize;
use tracing::debug;

use super::body_oracle::HOP_INDEX_ENV;

/// Response value when the header is stripped before reaching upstream.
const STATE_STRIPPED: &str = "stripped";
/// Response value when the header is neither present nor added.
const STATE_ABSENT: &str = "absent";
/// Response value when the effective value cannot be determined unambiguously.
const STATE_AMBIGUOUS: &str = "ambiguous";
/// Response value when `on_request` state never reached `on_response`.
const STATE_MISSING: &str = "state-missing";
/// Prefix marking a value that would be forwarded upstream: `fwd:<value>`.
const FWD_PREFIX: &str = "fwd:";
/// Metadata key prefix stashing per-header state between phases.
const META_PREFIX: &str = "e2e_header_oracle.";

/// Deserialized YAML config for the header oracle.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderOracleConfig {
    /// Header names to observe (matched case-insensitively).
    watch: Vec<String>,
}

/// Filter that reflects the effective forwarded value of configured request
/// headers into `x-qualification-fwd-<name>[-hop-N]` response headers.
pub struct HeaderOracleFilter {
    /// Lowercased header names to observe.
    watch: Vec<String>,

    /// Request-flow hop index from [`HOP_INDEX_ENV`], when set.
    hop_index: Option<u32>,
}

impl HeaderOracleFilter {
    /// Registry factory.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if the YAML config is invalid or `watch` is empty.
    pub fn from_config(config: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        let cfg: HeaderOracleConfig = parse_filter_config("e2e_header_oracle", config)?;
        if cfg.watch.is_empty() {
            return Err(FilterError::from("e2e_header_oracle: watch must not be empty"));
        }
        let hop_index = std::env::var(HOP_INDEX_ENV)
            .ok()
            .map(|raw| {
                raw.parse::<u32>().map_err(|error| {
                    FilterError::from(format!(
                        "e2e_header_oracle: {HOP_INDEX_ENV}={raw:?} is not a u32: {error}"
                    ))
                })
            })
            .transpose()?;
        Ok(Box::new(Self {
            watch: cfg.watch.iter().map(|h| h.to_lowercase()).collect(),
            hop_index,
        }))
    }

    /// Response header name reflecting `header`'s forwarded state for this hop.
    fn response_name(&self, header: &str) -> String {
        match self.hop_index {
            Some(n) => format!("x-qualification-fwd-{header}-hop-{n}"),
            None => format!("x-qualification-fwd-{header}"),
        }
    }

    /// Compute and stash the effective forwarded state of each watched header.
    ///
    /// States are collected first (immutable borrow of `ctx`) then written
    /// (mutable borrow). Called in `on_request` and again at request-body EOS so
    /// headers that body-parsing filters derive from the body (e.g. `json_body`
    /// -> `X-Gateway-Model-Name`) are captured once they exist.
    fn snapshot(&self, ctx: &mut HttpFilterContext<'_>) {
        let states: Vec<(String, String)> = self
            .watch
            .iter()
            .map(|name| (name.clone(), forwarded_state(ctx, name)))
            .collect();
        for (name, state) in states {
            ctx.set_metadata(format!("{META_PREFIX}{name}"), state);
        }
    }
}

/// Effective forwarded state of `name` at this hop, encoded as a header value.
///
/// `fwd:<value>` when the header reaches upstream, [`STATE_STRIPPED`] when an
/// earlier filter removed it, [`STATE_ABSENT`] when it is neither present nor
/// added, and [`STATE_AMBIGUOUS`] when the value is non-text or contested.
fn forwarded_state(ctx: &HttpFilterContext<'_>, name: &str) -> String {
    let Ok(header) = http::HeaderName::try_from(name) else {
        return STATE_AMBIGUOUS.to_owned();
    };
    // Pending mutations first: an identity_header_guard removal surfaces as
    // `Removed` (stripped); an explicit set/add surfaces as `Value`.
    match ctx.pending_header_value(&header) {
        Ok(PendingHeaderResult::Value(value)) => return format!("{FWD_PREFIX}{value}"),
        Ok(PendingHeaderResult::Removed) => return STATE_STRIPPED.to_owned(),
        Ok(PendingHeaderResult::Absent) => {},
        Err(_) => return STATE_AMBIGUOUS.to_owned(),
    }
    // Trusted routing mutations (e.g. json_body -> X-Gateway-Model-Name).
    match ctx.resolve_trusted_header(&header) {
        Ok(Some(value)) => return format!("{FWD_PREFIX}{value}"),
        Ok(None) => {},
        Err(_) => return STATE_AMBIGUOUS.to_owned(),
    }
    // Otherwise the original client header passes through unchanged.
    match ctx.request.headers.get(&header) {
        Some(value) => value
            .to_str()
            .map_or_else(|_| STATE_AMBIGUOUS.to_owned(), |text| format!("{FWD_PREFIX}{text}")),
        None => STATE_ABSENT.to_owned(),
    }
}

#[async_trait]
impl HttpFilter for HeaderOracleFilter {
    fn name(&self) -> &'static str {
        "e2e_header_oracle"
    }

    fn request_body_access(&self) -> BodyAccess {
        // Needs the request-body phase so it can observe headers that
        // body-parsing filters (e.g. json_body) derive from the body at EOS.
        BodyAccess::ReadOnly
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        // Header-phase view (strips + client-supplied headers); a fallback for
        // bodyless requests. The authoritative snapshot is taken at body EOS.
        self.snapshot(ctx);
        Ok(FilterAction::Continue)
    }

    async fn on_request_body(
        &self,
        ctx: &mut HttpFilterContext<'_>,
        _body: &mut Option<Bytes>,
        end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        if end_of_stream {
            self.snapshot(ctx);
        }
        Ok(FilterAction::Continue)
    }

    async fn on_response(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        for name in &self.watch {
            let value = ctx
                .get_metadata(&format!("{META_PREFIX}{name}"))
                .map_or_else(|| STATE_MISSING.to_owned(), str::to_owned);
            let response_name = self.response_name(name);
            if let Some(response) = ctx.response_header.as_mut() {
                let header_name = http::HeaderName::try_from(response_name.as_str()).map_err(|error| {
                    FilterError::from(format!(
                        "e2e_header_oracle: invalid header name {response_name}: {error}"
                    ))
                })?;
                let header_value = http::HeaderValue::from_str(&value)
                    .unwrap_or_else(|_| http::HeaderValue::from_static(STATE_AMBIGUOUS));
                response.headers.insert(header_name, header_value);
            }
            debug!(target: "e2e_header_oracle", header = %name, state = %value, hop_index = ?self.hop_index, "forwarded header state");
        }
        Ok(FilterAction::Continue)
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn filter(yaml: &str) -> Box<dyn HttpFilter> {
        let value: serde_yaml::Value = serde_yaml::from_str(yaml).expect("yaml");
        HeaderOracleFilter::from_config(&value).expect("from_config")
    }

    #[test]
    fn from_config_lowercases_and_keeps_watch() {
        let value: serde_yaml::Value = serde_yaml::from_str("watch: [X-Maas-Tenant]").expect("yaml");
        let built = HeaderOracleFilter::from_config(&value);
        assert!(built.is_ok(), "valid watch list parses");
    }

    #[test]
    fn from_config_rejects_empty_watch() {
        let value: serde_yaml::Value = serde_yaml::from_str("watch: []").expect("yaml");
        assert!(
            HeaderOracleFilter::from_config(&value).is_err(),
            "empty watch must be rejected"
        );
    }

    #[test]
    fn response_name_uses_hop_suffix_when_configured() {
        let f = HeaderOracleFilter {
            watch: vec!["x-maas-tenant".to_owned()],
            hop_index: Some(1),
        };
        assert_eq!(
            f.response_name("x-maas-tenant"),
            "x-qualification-fwd-x-maas-tenant-hop-1"
        );
    }

    #[test]
    fn response_name_single_hop_when_env_absent() {
        let f = HeaderOracleFilter {
            watch: vec!["x-maas-tenant".to_owned()],
            hop_index: None,
        };
        assert_eq!(f.response_name("x-maas-tenant"), "x-qualification-fwd-x-maas-tenant");
    }

    #[test]
    fn filter_reports_its_name() {
        assert_eq!(filter("watch: [x-maas-tenant]").name(), "e2e_header_oracle");
    }
}
