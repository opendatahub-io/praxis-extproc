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
//! Overlapping mutations from distinct contexts report `ambiguous`; their exact
//! wire outcome depends on the merged Envoy operations, not snapshot order.
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
use praxis_proto::envoy::service::ext_proc::v3::HeaderMutation;
use serde::Deserialize;
use tracing::debug;

/// Response value when the effective value cannot be determined unambiguously.
const STATE_AMBIGUOUS: &str = "ambiguous";
/// Response value when the request-phase snapshot never reached `on_response`.
const STATE_MISSING: &str = "state-missing";
/// Response value when the stashed state cannot be encoded as a header value.
const STATE_ENCODE_ERROR: &str = "encode-error";
/// Metadata key prefix stashing per-header state between phases.
const META_PREFIX: &str = "e2e_header_oracle.";

/// Early mutations survive when FDS creates a fresh context for the body phase.
struct HeaderSnapshot(HeaderMutation);

/// Context extensions are not carried into a later FDS body context.
#[derive(Clone)]
struct HeaderPhaseContext;

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

    /// Request-flow hop index from [`super::HOP_INDEX_ENV`], when set.
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
        let current = crate::adapter::collect_request_header_mutations(ctx).unwrap_or_default();
        let states: Vec<(String, String)> = self
            .watch
            .iter()
            .map(|name| {
                let state = if conflicting_phase_mutations(ctx, &current, name) {
                    STATE_AMBIGUOUS.to_owned()
                } else {
                    forwarded_state(ctx, name)
                };
                (name.clone(), state)
            })
            .collect();
        for (name, state) in states {
            ctx.set_metadata(format!("{META_PREFIX}{name}"), state);
        }
    }
}

/// Distinct changes to one header cannot be resolved from phase snapshots alone.
fn conflicting_phase_mutations(ctx: &HttpFilterContext<'_>, current: &HeaderMutation, name: &str) -> bool {
    let Some(previous) = ctx.get_filter_state::<HeaderSnapshot>() else {
        return false;
    };
    let previous = watched_mutations(&previous.0, name);
    let current = watched_mutations(current, name);
    previous != HeaderMutation::default()
        && current != HeaderMutation::default()
        && (ctx.extensions.get::<HeaderPhaseContext>().is_none() || previous != current)
}

/// Compare only the watched header, including whether an operation appends or sets.
fn watched_mutations(mutation: &HeaderMutation, name: &str) -> HeaderMutation {
    HeaderMutation {
        set_headers: mutation
            .set_headers
            .iter()
            .filter(|entry| {
                entry
                    .header
                    .as_ref()
                    .is_some_and(|header| header.key.eq_ignore_ascii_case(name))
            })
            .cloned()
            .collect(),
        remove_headers: mutation
            .remove_headers
            .iter()
            .filter(|header| header.eq_ignore_ascii_case(name))
            .cloned()
            .collect(),
    }
}

/// Effective forwarded state of `name` at this hop, encoded as a header value.
///
/// `fwd:<value>` when the header reaches upstream, `stripped` when an
/// earlier filter removed it, `absent` when it is neither present nor
/// added, and [`STATE_AMBIGUOUS`] when the value is non-text or contested.
fn forwarded_state(ctx: &HttpFilterContext<'_>, name: &str) -> String {
    let Ok(header) = http::HeaderName::try_from(name) else {
        return ForwardedState::Ambiguous.to_string();
    };
    if let Some(state) = mutated_header_state(ctx, &header) {
        return state.to_string();
    }
    // FDS parks early mutations outside the body context. Without a new
    // mutation, retain that observation instead of reverting to client input.
    if let Some(previous) = ctx.get_metadata(&format!("{META_PREFIX}{name}")) {
        return previous.to_owned();
    }
    match ctx.request.headers.get(&header) {
        Some(value) => value.to_str().map_or(ForwardedState::Ambiguous, |text| {
            ForwardedState::Forwarded(text.to_owned())
        }),
        None => ForwardedState::Absent,
    }
    .to_string()
}

/// Observe newly queued mutations before falling back to a previous snapshot.
fn mutated_header_state(ctx: &HttpFilterContext<'_>, header: &http::HeaderName) -> Option<ForwardedState> {
    match ctx.pending_header_value(header) {
        Ok(PendingHeaderResult::Value(value)) => return Some(ForwardedState::Forwarded(value)),
        Ok(PendingHeaderResult::Removed) => return Some(ForwardedState::Stripped),
        Ok(PendingHeaderResult::Absent) => {},
        Err(_) => return Some(ForwardedState::Ambiguous),
    }
    match ctx.resolve_trusted_header(header) {
        Ok(Some(value)) => Some(ForwardedState::Forwarded(value)),
        Ok(None) => None,
        Err(_) => Some(ForwardedState::Ambiguous),
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
        // Baseline snapshot so header-only requests (e.g. GET, no body EOS) are
        // captured. Refined at request-body EOS for body-derived headers.
        let mutation = crate::adapter::collect_request_header_mutations(ctx).unwrap_or_default();
        ctx.insert_filter_state(HeaderSnapshot(mutation));
        ctx.extensions.insert(HeaderPhaseContext);
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
                    .unwrap_or_else(|_| http::HeaderValue::from_static(STATE_ENCODE_ERROR));
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

    #[tokio::test]
    async fn body_snapshot_preserves_early_mutations_and_observes_new_mutations() {
        use crate::{
            adapter,
            server::{HydratedContext, StreamState},
        };

        let oracle = HeaderOracleFilter {
            watch: [
                "x-maas-tenant",
                "x-added",
                "x-model",
                "x-later-removed",
                "x-conflict",
                "x-repeat",
            ]
            .map(str::to_owned)
            .to_vec(),
            hop_index: None,
        };
        let pipeline = praxis_filter::FilterPipeline::build(&mut [], &praxis_filter::FilterRegistry::with_builtins())
            .expect("pipeline");
        let mut request = adapter::envoy_headers_to_request(&[]);
        request
            .headers
            .insert("x-maas-tenant", "spoofed".parse().expect("header"));
        request
            .headers
            .insert("x-later-removed", "original".parse().expect("header"));
        let mut state = StreamState::new();
        let mut ctx = HydratedContext::hydrate(
            state.carried_context.take(),
            adapter::build_filter_context(&pipeline, &request, false),
        )
        .expect("header context");
        ctx.current_filter_id = Some(0);
        ctx.request_headers_to_remove
            .push(http::HeaderName::from_static("x-maas-tenant"));
        ctx.extra_request_headers.push(("x-added".into(), "early".to_owned()));
        ctx.extra_request_headers
            .push(("x-conflict".into(), "early".to_owned()));
        ctx.extra_request_headers.push(("x-repeat".into(), "same".to_owned()));
        assert!(matches!(
            oracle.on_request(&mut ctx).await.expect("snapshot headers"),
            FilterAction::Continue
        ));
        assert!(
            !conflicting_phase_mutations(
                &ctx,
                &adapter::collect_request_header_mutations(&ctx).expect("early mutations"),
                "x-added"
            ),
            "an unchanged same-context observation is not a conflict"
        );
        ctx.dehydrate(&mut state.carried_context).expect("park header state");

        let mut ctx = HydratedContext::hydrate(
            state.carried_context.take(),
            adapter::build_filter_context(&pipeline, &request, false),
        )
        .expect("body context");
        ctx.current_filter_id = Some(0);
        assert!(
            ctx.get_filter_state::<HeaderSnapshot>().is_some(),
            "the pipeline restores the oracle's state"
        );
        assert!(
            ctx.extensions.get::<HeaderPhaseContext>().is_none(),
            "the new context must not inherit its predecessor's marker"
        );
        assert!(
            ctx.request_headers_to_remove.is_empty(),
            "early removals are held outside this context"
        );
        assert!(
            ctx.extra_request_headers.is_empty(),
            "early additions are held outside this context"
        );
        ctx.extra_request_headers
            .push(("x-model".into(), "body-derived".to_owned()));
        ctx.request_headers_to_remove
            .push(http::HeaderName::from_static("x-later-removed"));
        ctx.request_headers_to_remove
            .push(http::HeaderName::from_static("x-conflict"));
        ctx.extra_request_headers.push(("x-repeat".into(), "same".to_owned()));
        assert!(matches!(
            oracle
                .on_request_body(&mut ctx, &mut None, true)
                .await
                .expect("snapshot body EOS"),
            FilterAction::Continue
        ));
        let mut response = adapter::envoy_headers_to_response(&[]);
        ctx.response_header = Some(&mut response);
        assert!(matches!(
            oracle.on_response(&mut ctx).await.expect("publish oracle"),
            FilterAction::Continue
        ));
        for (header, expected) in [
            ("x-maas-tenant", "stripped"),
            ("x-added", "fwd:early"),
            ("x-model", "fwd:body-derived"),
            ("x-later-removed", "stripped"),
            ("x-conflict", "ambiguous"),
            ("x-repeat", "ambiguous"),
        ] {
            assert_eq!(
                ctx.response_header
                    .as_ref()
                    .expect("response")
                    .headers
                    .get(oracle.response_name(header))
                    .expect("oracle header"),
                expected,
                "the body snapshot must reflect the latest effective value of {header}"
            );
        }
    }
}
