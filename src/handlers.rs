// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Per-phase ExtProc message handlers.
//!
//! Each handler owns one protocol phase (request/response headers and body):
//! it marks end-of-stream, converts Envoy messages into filter inputs, and
//! routes to the [`crate::pipeline`] by body mode and filter capabilities.
//! Buffered bodies are accumulated here up to the stream's effective
//! body-accumulation limit and run through the full pipeline at EOS.

use praxis_filter::FilterPipeline;
use praxis_proto::envoy::service::{common::v3::HeaderValue, ext_proc::v3::ProcessingResponse};
use tonic::Status;
use tracing::debug;

use crate::{
    adapter, metrics,
    pipeline::{
        MutationDelivery, RequestPhase, ResponsePhase, passthrough_chunk, process_streamed_body_chunk,
        run_request_header_filters_early, run_request_pipeline, run_response_header_filters_early,
        run_response_pipeline,
    },
    protocol::{PhaseState, ProtocolPhase, duplicate_after_eos},
    response::{self, BodyMode},
    server::StreamState,
};

// -----------------------------------------------------------------------------
// Redelivery Policy
// -----------------------------------------------------------------------------

/// Apply body-phase policy to the phase state observed by [`crate::protocol::EosTracker::check_and_mark`].
///
/// Returns `Ok(None)` to keep processing. For a re-delivery ([`PhaseState::Completed`]),
/// `FULL_DUPLEX_STREAMED` is the only mode where Envoy benignly re-sends the final
/// chunk (>1MB bodies, Envoy 1.35+), so it becomes an ignored no-op (`Ok(Some(empty))`);
/// any other mode never re-delivers, so a duplicate is rejected.
fn handle_body_redelivery(
    entry_state: PhaseState,
    mode: BodyMode,
    phase: ProtocolPhase,
    bytes: usize,
) -> Result<Option<Vec<ProcessingResponse>>, Status> {
    match entry_state {
        PhaseState::Active => Ok(None),
        PhaseState::Completed if mode == BodyMode::FullDuplexStreamed => {
            debug!(?phase, bytes, "ignoring re-delivered FDS body end-of-stream chunk");
            Ok(Some(Vec::new()))
        },
        PhaseState::Completed => Err(duplicate_after_eos(phase)),
    }
}

// -----------------------------------------------------------------------------
// Request Handlers
// -----------------------------------------------------------------------------

/// Handle request headers: parse into [`Request`] and route by body mode.
///
/// For `BUFFERED`, sends an empty `HeadersResponse` — pipeline runs at body EOS.
/// For `STREAMED`, runs filters early and sends mutations in `HeadersResponse`.
/// For `FDS` with body filters, returns no response — full pipeline at body EOS.
/// For `FDS` passthrough, runs header filters early, defers mutations to first chunk.
///
/// [`Request`]: praxis_filter::Request
pub(crate) async fn handle_request_headers(
    pipeline: &FilterPipeline,
    headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if state
        .eos_tracker
        .check_and_mark(ProtocolPhase::RequestHeaders, headers.end_of_stream)?
        == PhaseState::Completed
    {
        // Envoy does not re-deliver headers; a duplicate is a protocol violation.
        return Err(duplicate_after_eos(ProtocolPhase::RequestHeaders));
    }

    let envoy_headers = extract_header_list(&headers);
    state.request = Some(adapter::envoy_headers_to_request(&envoy_headers));

    if headers.end_of_stream {
        return run_request_pipeline(RequestPhase::Headers, pipeline, state).await;
    }

    match state.protocol_config.request_body_mode {
        BodyMode::None | BodyMode::Streamed => {
            state.header_state.request_headers_sent = true;
            run_request_header_filters_early(pipeline, state, MutationDelivery::Send).await
        },
        BodyMode::FullDuplexStreamed if !pipeline.body_capabilities().needs_request_body => {
            run_request_header_filters_early(pipeline, state, MutationDelivery::DeferSilent).await
        },
        BodyMode::FullDuplexStreamed => Ok(Vec::new()),
        _ => Ok(vec![response::request_headers(None)]),
    }
}

/// Handle request body: route by body mode and filter capabilities.
pub(crate) async fn handle_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.request_body_mode;

    if let Some(response) = handle_body_redelivery(
        state
            .eos_tracker
            .check_and_mark(ProtocolPhase::RequestBody, body.end_of_stream)?,
        mode,
        ProtocolPhase::RequestBody,
        body.body.len(),
    )? {
        return Ok(response);
    }

    let needs_body = pipeline.body_capabilities().needs_request_body;

    match (mode, needs_body) {
        (BodyMode::Streamed | BodyMode::FullDuplexStreamed, false) => Ok(passthrough_chunk(&body, state, mode, true)),
        (BodyMode::Streamed, true) => process_streamed_body_chunk(pipeline, body, state, true).await,
        _ => accumulate_request_body(pipeline, body, state).await,
    }
}

/// Accumulate request body chunks, run full pipeline on EOS.
async fn accumulate_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    check_body_limit(state.request_body.len(), body.body.len(), state.max_body_accumulation)?;
    state.request_body.extend_from_slice(&body.body);

    if !body.end_of_stream {
        return Ok(Vec::new());
    }

    run_request_pipeline(RequestPhase::Body, pipeline, state).await
}

// -----------------------------------------------------------------------------
// Response Handlers
// -----------------------------------------------------------------------------

/// Handle response headers: run response filters and respond with mutations.
///
/// For `BUFFERED`, runs filters early and defers mutations to body phase
/// (Envoy honours `CommonResponse.header_mutation` on body responses).
/// For `NONE`, runs filters early and sends mutations immediately because no
/// response body message is delivered.
/// For `STREAMED`, runs filters early and sends mutations immediately
/// (Envoy ignores header mutations on body responses for non-`BUFFERED`).
/// For `FDS` with body filters, returns no response — full pipeline at body EOS.
/// For `FDS` passthrough, runs filters early, defers mutations to first chunk.
#[expect(
    clippy::large_stack_frames,
    reason = "StreamState carries HttpFilterContext fields grown in praxis 0.5.4"
)]
pub(crate) async fn handle_response_headers(
    pipeline: &FilterPipeline,
    headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if state
        .eos_tracker
        .check_and_mark(ProtocolPhase::ResponseHeaders, headers.end_of_stream)?
        == PhaseState::Completed
    {
        // Envoy does not re-deliver headers; a duplicate is a protocol violation.
        return Err(duplicate_after_eos(ProtocolPhase::ResponseHeaders));
    }

    let envoy_headers = extract_header_list(&headers);
    state.response = Some(adapter::envoy_headers_to_response(&envoy_headers));

    if headers.end_of_stream {
        return run_response_pipeline(ResponsePhase::Headers, pipeline, state).await;
    }

    match state.protocol_config.response_body_mode {
        BodyMode::None | BodyMode::Streamed => {
            state.header_state.response_headers_sent = true;
            run_response_header_filters_early(pipeline, state, MutationDelivery::Send).await
        },
        BodyMode::FullDuplexStreamed if !pipeline.body_capabilities().needs_response_body => {
            run_response_header_filters_early(pipeline, state, MutationDelivery::DeferSilent).await
        },
        BodyMode::FullDuplexStreamed => Ok(Vec::new()),
        _ => run_response_header_filters_early(pipeline, state, MutationDelivery::DeferWithResponse).await,
    }
}

/// Handle response body: route by body mode and filter capabilities.
pub(crate) async fn handle_response_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.response_body_mode;

    if let Some(response) = handle_body_redelivery(
        state
            .eos_tracker
            .check_and_mark(ProtocolPhase::ResponseBody, body.end_of_stream)?,
        mode,
        ProtocolPhase::ResponseBody,
        body.body.len(),
    )? {
        return Ok(response);
    }

    let needs_body = pipeline.body_capabilities().needs_response_body;

    match (mode, needs_body) {
        (BodyMode::Streamed | BodyMode::FullDuplexStreamed, false) => Ok(passthrough_chunk(&body, state, mode, false)),
        (BodyMode::Streamed, true) => process_streamed_body_chunk(pipeline, body, state, false).await,
        _ => accumulate_response_body(pipeline, body, state).await,
    }
}

/// Accumulate response body chunks, run full pipeline on EOS.
async fn accumulate_response_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    check_body_limit(state.response_body.len(), body.body.len(), state.max_body_accumulation)?;
    state.response_body.extend_from_slice(&body.body);

    if !body.end_of_stream {
        return Ok(Vec::new());
    }

    run_response_pipeline(ResponsePhase::Body, pipeline, state).await
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Extract the header list from an `HttpHeaders` message.
fn extract_header_list(headers: &praxis_proto::envoy::service::ext_proc::v3::HttpHeaders) -> Vec<HeaderValue> {
    headers
        .headers
        .as_ref()
        .map(|hm| hm.headers.clone())
        .unwrap_or_default()
}

/// Reject body accumulation exceeding the effective limit.
///
/// `limit` is `None` when bounding is disabled via
/// `insecure_options.allow_unbounded_body`, in which case any size is accepted.
fn check_body_limit(current: usize, incoming: usize, limit: Option<usize>) -> Result<(), Status> {
    if let Some(max) = limit
        && current.saturating_add(incoming) > max
    {
        metrics::record_body_size_rejection();
        return Err(Status::resource_exhausted("body exceeds maximum size"));
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use praxis_filter::{FilterAction, HttpFilterContext};

    use super::*;
    use crate::test_support::counter_value;

    #[test]
    fn handle_body_redelivery_ignores_only_fds_duplicates() {
        let fds = BodyMode::FullDuplexStreamed;
        let phase = ProtocolPhase::RequestBody;

        // A fresh message always proceeds.
        let proceed = handle_body_redelivery(PhaseState::Active, fds, phase, 10).unwrap();
        assert!(proceed.is_none());

        // FDS re-delivery: benign no-op (empty response set).
        let noop = handle_body_redelivery(PhaseState::Completed, fds, phase, 10).unwrap();
        assert!(
            noop.is_some_and(|r| r.is_empty()),
            "FDS re-delivery should be an empty no-op"
        );

        // Other modes never re-deliver: a duplicate is a rejected violation.
        for mode in [BodyMode::Streamed, BodyMode::Buffered] {
            let err = handle_body_redelivery(PhaseState::Completed, mode, phase, 10).unwrap_err();
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "duplicate in {mode:?} should be rejected"
            );
            assert!(err.message().contains("after end_of_stream"));
        }
    }

    #[test]
    fn check_body_limit_rejection_records_metric() {
        let count = counter_value("praxis_extproc_body_size_rejections_total", &[], || {
            assert!(
                check_body_limit(
                    crate::config::DEFAULT_MAX_BODY_BYTES,
                    1,
                    Some(crate::config::DEFAULT_MAX_BODY_BYTES)
                )
                .is_err(),
                "exceeding the body limit must be rejected"
            );
        });
        assert_eq!(count, 1, "body-size rejection must increment the counter");
    }

    #[test]
    fn check_body_limit_unbounded_accepts_any_size() {
        assert!(
            check_body_limit(usize::MAX, usize::MAX, None).is_ok(),
            "unbounded limit must accept any accumulation without overflow"
        );
    }

    struct RequestMutationFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for RequestMutationFilter {
        fn name(&self) -> &'static str {
            "request_mutation_probe"
        }

        async fn on_request(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            ctx.request_headers_to_set
                .push(("x-request-probe".parse().unwrap(), "sent".parse().unwrap()));
            Ok(FilterAction::Continue)
        }

        async fn on_response(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }
    }

    impl RequestMutationFilter {
        /// Registry factory for the request-phase protocol test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    struct ResponseMutationFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for ResponseMutationFilter {
        fn name(&self) -> &'static str {
            "response_mutation_probe"
        }

        async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            if let Some(response) = ctx.response_header.as_mut() {
                response.headers.insert("x-response-probe", "sent".parse().unwrap());
            }
            Ok(FilterAction::Continue)
        }
    }

    impl ResponseMutationFilter {
        /// Registry factory for the response-phase protocol test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "protocol regression test deliberately exercises the complete handoff"
    )]
    #[tokio::test]
    async fn none_request_body_mode_delivers_header_mutation_immediately() {
        let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: request_mutation_probe\n",
        )
        .unwrap();
        let mut registry = praxis_filter::FilterRegistry::with_builtins();
        registry
            .register(
                "request_mutation_probe",
                praxis_filter::http_builtin(RequestMutationFilter::from_config),
            )
            .unwrap();
        let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::None;
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        let responses = handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(responses.len(), 1, "NONE must complete the request in headers");
        assert!(state.header_state.request_headers_sent);
        let response = responses.first().and_then(|response| response.response.as_ref());
        assert!(
            matches!(
                response,
                Some(
                    praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::RequestHeaders(headers)
                ) if headers.response.as_ref().is_some_and(|common| {
                    common.clear_route_cache
                        && common.header_mutation.as_ref().is_some_and(|mutation| {
                            mutation.set_headers.iter().any(|header| {
                                header.header.as_ref().is_some_and(|header| {
                                    header.key == "x-request-probe" && header.value == "sent"
                                })
                            })
                        })
                })
            ),
            "NONE must return CommonResponse with the mutation and clear_route_cache"
        );
    }

    #[expect(
        clippy::too_many_lines,
        reason = "protocol regression test deliberately exercises the complete handoff"
    )]
    #[tokio::test]
    async fn none_response_body_mode_delivers_header_mutation_immediately() {
        let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: response_mutation_probe\n",
        )
        .unwrap();
        let mut registry = praxis_filter::FilterRegistry::with_builtins();
        registry
            .register(
                "response_mutation_probe",
                praxis_filter::http_builtin(ResponseMutationFilter::from_config),
            )
            .unwrap();
        let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();
        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::None;
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        let responses = handle_response_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(responses.len(), 1, "NONE must complete the response in headers");
        assert!(state.header_state.response_headers_sent);
        assert!(matches!(
            responses.first().and_then(|response| response.response.as_ref()),
            Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ResponseHeaders(headers))
                if headers.response.as_ref().is_some_and(|common| common
                    .header_mutation
                    .as_ref()
                    .is_some_and(|mutation| mutation.set_headers.iter().any(|header| header
                        .header
                        .as_ref()
                        .is_some_and(|header| header.key == "x-response-probe"))))
        ));
    }
}
