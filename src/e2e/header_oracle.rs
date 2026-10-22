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

use std::fmt;

use async_trait::async_trait;
use bytes::Bytes;
use praxis_filter::{
    BodyAccess, FilterAction, FilterError, HttpFilter, HttpFilterContext, PendingHeaderResult, parse_filter_config,
};
use serde::Deserialize;
use tracing::debug;

/// Response value when the effective value cannot be determined unambiguously.
const STATE_AMBIGUOUS: &str = "ambiguous";
/// Response value when the request-phase snapshot never reached `on_response`.
const STATE_MISSING: &str = "state-missing";
/// Metadata key prefix stashing per-header state between phases.
const META_PREFIX: &str = "e2e_header_oracle.";

/// Effective state of a watched header at this hop, as published to tests.
///
/// One owner of the wire vocabulary: the `Display` impl is the single
/// serialization the tests parse.
enum ForwardedState {
    /// Reaches upstream with this value (serialized as `fwd:<value>`).
    Forwarded(String),
    /// Removed by an earlier filter before reaching upstream.
    Stripped,
    /// Neither present on the request nor added by a filter.
    Absent,
    /// Non-text or contested value.
    Ambiguous,
}

impl fmt::Display for ForwardedState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Forwarded(value) => write!(f, "fwd:{value}"),
            Self::Stripped => f.write_str("stripped"),
            Self::Absent => f.write_str("absent"),
            Self::Ambiguous => f.write_str(STATE_AMBIGUOUS),
        }
    }
}

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
        let hop_index = super::hop_index_from_env()?;
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
            .map(|name| (name.clone(), forwarded_state(ctx, name).to_string()))
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
fn forwarded_state(ctx: &HttpFilterContext<'_>, name: &str) -> ForwardedState {
    let Ok(header) = http::HeaderName::try_from(name) else {
        return ForwardedState::Ambiguous;
    };
    // Pending mutations first: an identity_header_guard removal surfaces as
    // `Removed` (stripped); an explicit set/add surfaces as `Value`.
    match ctx.pending_header_value(&header) {
        Ok(PendingHeaderResult::Value(value)) => return ForwardedState::Forwarded(value),
        Ok(PendingHeaderResult::Removed) => return ForwardedState::Stripped,
        Ok(PendingHeaderResult::Absent) => {},
        Err(_) => return ForwardedState::Ambiguous,
    }
    // Trusted routing mutations (e.g. json_body -> X-Gateway-Model-Name).
    match ctx.resolve_trusted_header(&header) {
        Ok(Some(value)) => return ForwardedState::Forwarded(value),
        Ok(None) => {},
        Err(_) => return ForwardedState::Ambiguous,
    }
    // Otherwise the original client header passes through unchanged.
    match ctx.request.headers.get(&header) {
        Some(value) => value.to_str().map_or(ForwardedState::Ambiguous, |text| {
            ForwardedState::Forwarded(text.to_owned())
        }),
        None => ForwardedState::Absent,
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

    async fn on_request(&self, _ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        // The single authoritative snapshot is taken at request-body EOS, once
        // every request-side mutation is visible: header-phase strips persist to
        // EOS, and body-phase additions (e.g. json_body's routing header) are set
        // by then. This phase only observes; see `on_request_body`.
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
#[expect(clippy::expect_used, clippy::too_many_lines, reason = "tests")]
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

    // -------------------------------------------------------------------------
    // Debugger walkthrough
    //
    // A self-contained reproduction that builds a real `HttpFilterContext` and
    // drives the filter end-to-end, with no cluster. Set breakpoints in
    // `forwarded_state`, `snapshot`, and `on_response` and step through it:
    //
    //   cargo test --features k8s-e2e --lib \
    //     e2e::header_oracle::tests::debugger_walkthrough --no-run
    //   rust-lldb target/debug/deps/praxis_extproc-<hash> \
    //     -o 'b praxis_extproc::e2e::header_oracle::forwarded_state' -o run
    //
    // (or set a gutter breakpoint and Debug the test in your IDE.)
    // -------------------------------------------------------------------------

    use std::{collections::HashMap, sync::LazyLock, time::Instant};

    use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
    use praxis_core::{id::IdGenerator, time::SystemTimeSource};
    use praxis_filter::{BodyMode, Request, RequestExtensions, Response, SubRequestResponseMode};

    /// Deterministic id generator for the test context.
    static TEST_ID_GENERATOR: LazyLock<IdGenerator> = LazyLock::new(|| IdGenerator::with_seed(0));

    /// Build a minimal `HttpFilterContext` borrowing `req` (mirrors the fields of
    /// the filter SDK's own test helper; everything empty/default).
    fn test_ctx(req: &Request) -> HttpFilterContext<'_> {
        HttpFilterContext {
            buffered_request_body: None,
            body_done_indices: Vec::new(),
            branch_iterations: HashMap::new(),
            grpc_completion: None,
            client_addr: None,
            cluster: None,
            current_filter_id: None,
            downstream_tls: false,
            extensions: RequestExtensions::default(),
            executed_branch_filters: Vec::new(),
            executed_filter_indices: Vec::new(),
            extra_request_headers: Vec::new(),
            request_headers_to_remove: Vec::new(),
            request_headers_to_set: Vec::new(),
            filter_metadata: HashMap::new(),
            prior_pre_read_mutations: Vec::new(),
            pre_read_mutations: Vec::new(),
            structured_metadata: HashMap::new(),
            filter_results: HashMap::new(),
            filter_state: HashMap::new(),
            health_registry: None,
            id_generator: &TEST_ID_GENERATOR,
            kv_stores: None,
            session_stores: None,
            metrics_route: None,
            peer_identity: None,
            subrequest_client: None,
            subrequest_response_mode: SubRequestResponseMode::Buffered,
            request: req,
            request_body_bytes: 0,
            request_body_mode: BodyMode::Stream,
            request_start: Instant::now(),
            response_body_bytes: 0,
            response_body_mode: BodyMode::Stream,
            response_header: None,
            response_headers_modified: false,
            upstream_reached: false,
            rewritten_path: None,
            selected_endpoint_index: None,
            attempted_endpoints: Vec::new(),
            retry_policy: None,
            route_retry_policy: None,
            hedge_policy: None,
            cluster_retry_state: None,
            cluster_retry_state_released: false,
            endpoint_reselector: None,
            pinned_endpoint_address: None,
            time_source: &SystemTimeSource,
            upstream: None,
        }
    }

    #[tokio::test]
    async fn debugger_walkthrough() {
        // A request carrying a spoofed identity header and a benign one.
        let mut req = Request {
            method: Method::POST,
            uri: "/v1/chat/completions".parse::<Uri>().expect("uri"),
            headers: HeaderMap::new(),
        };
        req.headers
            .insert("x-maas-tenant", HeaderValue::from_static("spoofed-by-client"));
        req.headers.insert("x-keep", HeaderValue::from_static("keepme"));

        let mut response = Response {
            headers: HeaderMap::new(),
            status: StatusCode::OK,
        };

        let mut ctx = test_ctx(&req);
        // Simulate identity_header_guard stripping the spoofed header (→ stripped).
        ctx.request_headers_to_remove
            .push(HeaderName::from_static("x-maas-tenant"));
        // Simulate json_body injecting the routing header from the body (→ fwd:).
        ctx.request_headers_to_set.push((
            HeaderName::from_static("x-gateway-model-name"),
            HeaderValue::from_static("granite-8b"),
        ));

        // Watch one header per forwarded_state branch:
        //   x-maas-tenant        → pending Removed      → "stripped"
        //   x-gateway-model-name → pending Value        → "fwd:granite-8b"
        //   x-keep               → original passthrough → "fwd:keepme"
        //   x-absent             → nothing              → "absent"
        let filter = HeaderOracleFilter {
            watch: vec![
                "x-maas-tenant".to_owned(),
                "x-gateway-model-name".to_owned(),
                "x-keep".to_owned(),
                "x-absent".to_owned(),
            ],
            hop_index: Some(1),
        };

        drop(filter.on_request(&mut ctx).await.expect("on_request"));
        drop(
            filter
                .on_request_body(&mut ctx, &mut None, true)
                .await
                .expect("on_request_body"),
        );
        ctx.response_header = Some(&mut response);
        drop(filter.on_response(&mut ctx).await.expect("on_response"));

        let headers = &ctx.response_header.as_ref().expect("response set").headers;
        let fwd = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        assert_eq!(
            fwd("x-qualification-fwd-x-maas-tenant-hop-1").as_deref(),
            Some("stripped")
        );
        assert_eq!(
            fwd("x-qualification-fwd-x-gateway-model-name-hop-1").as_deref(),
            Some("fwd:granite-8b")
        );
        assert_eq!(fwd("x-qualification-fwd-x-keep-hop-1").as_deref(), Some("fwd:keepme"));
        assert_eq!(fwd("x-qualification-fwd-x-absent-hop-1").as_deref(), Some("absent"));
    }
}
