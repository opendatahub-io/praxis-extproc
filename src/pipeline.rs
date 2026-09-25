// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Filter pipeline execution and `ProcessingResponse` assembly.
//!
//! Runs the Praxis [`FilterPipeline`] for each request/response phase, applies
//! body filters, and turns filter actions and mutations into the header/body
//! responses Envoy expects. Also handles `STREAMED` chunk processing and the
//! deferred delivery of header mutations across phases.
//!
//! [`FilterPipeline`]: praxis_filter::FilterPipeline

use std::mem;

use bytes::Bytes;
use http::HeaderMap;
use praxis_filter::{FilterAction, FilterPipeline, HttpFilterContext, Response};
use praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse;
use tonic::Status;
use tracing::warn;

use crate::{
    adapter, metrics,
    response::{self, BodyMode},
    server::{HydratedContext, StreamState},
};

// -----------------------------------------------------------------------------
// Pipeline Execution
// -----------------------------------------------------------------------------

/// Request filter execution phase.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RequestPhase {
    /// Headers phase (headers EOS=true).
    Headers,

    /// Body phase (body EOS=true).
    Body,
}

/// Execute request pipeline for the given phase.
///
/// Returns headers or body response with mutations.
pub(crate) async fn run_request_pipeline(
    phase: RequestPhase,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        metrics::record_invalid_argument("missing_headers", "request");
        return Err(Status::invalid_argument("request headers not received"));
    };
    let ctx = adapter::build_filter_context(pipeline, request);
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = immediate_from_action(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    let original_len = state.request_body.len();
    let body_reject = run_body_filters(pipeline, &mut ctx, &mut state.request_body, true).await?;
    if let Some(imm) = body_reject {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_request_header_mutations(&ctx);

    ctx.dehydrate(&mut state.carried_context)?;

    // Emit the authoritative buffer even when empty: a filter that cleared the
    // body must produce an explicit empty body AND content-length: 0. Collapsing
    // empty -> None here would drop both (buffered) or desync CL (FDS+flag).
    let body = Some(state.request_body.as_slice());
    Ok(build_request_for_phase(
        phase,
        with_content_length(mutation, body, original_len),
        body,
        state.protocol_config.request_body_mode,
    ))
}

/// Response filter execution phase.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResponsePhase {
    /// Headers phase (response headers EOS=true).
    Headers,

    /// Body phase (response body EOS=true).
    Body,
}

/// Execute response pipeline for the given phase.
///
/// Returns headers or body response with mutations.
#[expect(clippy::too_many_lines, reason = "context borrowing prevents extraction")]
pub(crate) async fn run_response_pipeline(
    phase: ResponsePhase,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        metrics::record_invalid_argument("missing_headers", "request");
        return Err(Status::invalid_argument("request headers not received"));
    };

    let mut resp = state.response.take().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "response");
        Status::invalid_argument("response headers not received")
    })?;

    let ctx = adapter::build_filter_context(pipeline, request);
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;
    let original_headers = capture_original_headers(&resp);
    ctx.response_header = Some(&mut resp);
    ctx.upstream_reached = true;

    let original_len = state.response_body.len();
    if let Some(rejection) = execute_response_pipeline_and_body_filters(
        phase,
        pipeline,
        &mut ctx,
        &mut state.response_body,
        state.header_state.response_filters_executed,
    )
    .await?
    {
        return Ok(vec![response::immediate(rejection)]);
    }

    let current_mutation = adapter::collect_response_header_mutations_diff(&ctx, &original_headers);
    ctx.dehydrate(&mut state.carried_context)?;

    let mutation = match phase {
        ResponsePhase::Headers => current_mutation,
        ResponsePhase::Body => {
            let deferred = state.deferred_response_header_mutation.take();
            merge_mutations(deferred, current_mutation)
        },
    };

    // Emit the authoritative buffer even when empty: a filter that cleared the
    // body must produce an explicit empty body AND content-length: 0. Collapsing
    // empty -> None here would drop both (buffered) or desync CL (FDS+flag).
    let body = Some(state.response_body.as_slice());
    Ok(build_response_for_phase(
        phase,
        with_content_length(mutation, body, original_len),
        body,
        state.protocol_config.response_body_mode,
    ))
}

/// Execute response pipeline and body filters, checking for rejections.
///
/// Returns `Some(ImmediateResponse)` if filters reject the request.
async fn execute_response_pipeline_and_body_filters(
    phase: ResponsePhase,
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    response_body: &mut Vec<u8>,
    filters_executed: bool,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    let should_execute = match phase {
        ResponsePhase::Headers => true,
        ResponsePhase::Body => !filters_executed,
    };

    if should_execute {
        let action = execute_response(pipeline, ctx).await?;
        if let Some(imm) = immediate_from_action(action) {
            return Ok(Some(imm));
        }
    }

    let body_reject = run_resp_body_filters(pipeline, ctx, response_body, true)?;
    Ok(body_reject)
}

/// Set `content-length` when the emitted body differs in size from the original.
///
/// Keeps the declared length in sync with the bytes actually emitted to Envoy,
/// including 0 when a filter clears the body. Left untouched only when the size
/// is unchanged. Honored by Envoy in `BUFFERED` and in `FULL_DUPLEX_STREAMED` with
/// `allow_content_length_header`; ignored (harmlessly) in `STREAMED`, where Envoy
/// strips content-length and switches to chunked encoding.
fn with_content_length(
    mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
    body: Option<&[u8]>,
    original_len: usize,
) -> Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation> {
    match body {
        Some(b) if b.len() != original_len => Some(adapter::set_content_length(mutation, b.len())),
        _ => mutation,
    }
}

/// Build request-phase responses, prepending `HeadersResponse` in FDS mode.
fn build_request_for_phase(
    phase: RequestPhase,
    mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
    body: Option<&[u8]>,
    mode: BodyMode,
) -> Vec<ProcessingResponse> {
    match (phase, mode) {
        (RequestPhase::Headers, _) => vec![response::request_headers(mutation)],
        (RequestPhase::Body, BodyMode::FullDuplexStreamed) => {
            let mut r = vec![response::request_headers(mutation)];
            // Assembled body emitted at EOS.
            r.extend(response::request_body(body, None, mode, true));
            r
        },
        (RequestPhase::Body, _) => response::request_body(body, mutation, mode, true),
    }
}

/// Build response-phase responses, prepending `ResponseHeadersResponse` in FDS mode.
fn build_response_for_phase(
    phase: ResponsePhase,
    mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
    body: Option<&[u8]>,
    mode: BodyMode,
) -> Vec<ProcessingResponse> {
    match (phase, mode) {
        (ResponsePhase::Headers, _) => vec![response::response_headers(mutation)],
        (ResponsePhase::Body, BodyMode::FullDuplexStreamed) => {
            let mut r = vec![response::response_headers(mutation)];
            // Assembled body emitted at EOS.
            r.extend(response::response_body(body, None, mode, true));
            r
        },
        (ResponsePhase::Body, _) => response::response_body(body, mutation, mode, true),
    }
}

// -----------------------------------------------------------------------------
// Streamed Body Chunk Handlers
// -----------------------------------------------------------------------------

/// Forward a body chunk without filter execution.
///
/// Used when no filters declared body access — the chunk passes through
/// unchanged. On the first chunk, prepends the deferred `HeadersResponse`
/// carrying any header mutations from the header phase.
pub(crate) fn passthrough_chunk(
    body: &praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
    mode: BodyMode,
    is_request: bool,
) -> Vec<ProcessingResponse> {
    let body_data = body_data_if_present(&body.body);
    // Propagate the source chunk's EOS: Envoy may split a body across multiple
    // messages, and the wire format (streamed vs. replacement) is chosen by
    // `mode` inside `response::request_body`/`response_body`.
    let body_responses = if is_request {
        response::request_body(body_data, None, mode, body.end_of_stream)
    } else {
        response::response_body(body_data, None, mode, body.end_of_stream)
    };

    if !state.header_state.take_first_chunk(is_request) {
        return body_responses;
    }

    let mutation = if is_request {
        state.deferred_request_header_mutation.take()
    } else {
        state.deferred_response_header_mutation.take()
    };
    let hdr = if is_request {
        response::request_headers(mutation)
    } else {
        response::response_headers(mutation)
    };
    let mut responses = vec![hdr];
    responses.extend(body_responses);
    responses
}

/// Process a single body chunk in `STREAMED` mode.
///
/// Runs body filters on the chunk and responds immediately.
/// Header mutations are sent at header time for `STREAMED`, so
/// `deferred_*_header_mutation` will be `None` here.
#[expect(
    clippy::too_many_lines,
    reason = "Reusable for request and response processing, better than 2 different functions"
)]
pub(crate) async fn process_streamed_body_chunk(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let request = state.request.as_ref().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "request");
        Status::invalid_argument("request headers not received")
    })?;
    let mut ctx = adapter::build_filter_context(pipeline, request);
    // Resolve the fallible response ref before hydrate drains carried state, so an
    // early return here leaves the parked state untouched.
    if !is_request {
        let resp = state.response.as_mut().ok_or_else(|| {
            metrics::record_invalid_argument("missing_headers", "response");
            Status::invalid_argument("response headers not received")
        })?;
        ctx.response_header = Some(resp);
        ctx.upstream_reached = true;
    }
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;
    let eos = body.end_of_stream;
    let mut chunk = body.body;
    let reject = if is_request {
        run_body_filters(pipeline, &mut ctx, &mut chunk, eos).await?
    } else {
        run_resp_body_filters(pipeline, &mut ctx, &mut chunk, eos)?
    };
    if let Some(imm) = reject {
        return Ok(vec![response::immediate(imm)]);
    }
    ctx.dehydrate(&mut state.carried_context)?;
    let (mutation, body_mode) = if is_request {
        (
            state.deferred_request_header_mutation.take(),
            state.protocol_config.request_body_mode,
        )
    } else {
        (
            state.deferred_response_header_mutation.take(),
            state.protocol_config.response_body_mode,
        )
    };

    let body_data = body_data_if_present(&chunk);
    let responses = if is_request {
        response::request_body(body_data, mutation, body_mode, eos)
    } else {
        response::response_body(body_data, mutation, body_mode, eos)
    };
    Ok(responses)
}

/// How header mutations are delivered after early filter execution.
pub(crate) enum MutationDelivery {
    /// Send mutations immediately in the `HeadersResponse`.
    Send,

    /// Defer mutations — send empty `HeadersResponse` now.
    DeferWithResponse,

    /// Defer mutations — send no response (FDS passthrough).
    DeferSilent,
}

impl MutationDelivery {
    /// Package mutation into responses per delivery strategy.
    fn deliver_request(
        self,
        mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
        state: &mut StreamState,
    ) -> Vec<ProcessingResponse> {
        match self {
            Self::Send => vec![response::request_headers(mutation)],
            Self::DeferWithResponse => {
                state.deferred_request_header_mutation = mutation;
                vec![response::request_headers(None)]
            },
            Self::DeferSilent => {
                state.deferred_request_header_mutation = mutation;
                Vec::new()
            },
        }
    }

    /// Package mutation into responses per delivery strategy.
    fn deliver_response(
        self,
        mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
        state: &mut StreamState,
    ) -> Vec<ProcessingResponse> {
        match self {
            Self::Send => vec![response::response_headers(mutation)],
            Self::DeferWithResponse => {
                state.deferred_response_header_mutation = mutation;
                vec![response::response_headers(None)]
            },
            Self::DeferSilent => {
                state.deferred_response_header_mutation = mutation;
                Vec::new()
            },
        }
    }
}

/// Run request header filters early and deliver mutations per strategy.
pub(crate) async fn run_request_header_filters_early(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    delivery: MutationDelivery,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Ok(delivery.deliver_request(None, state));
    };
    let ctx = adapter::build_filter_context(pipeline, request);
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = immediate_from_action(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_request_header_mutations(&ctx);
    ctx.dehydrate(&mut state.carried_context)?;

    Ok(delivery.deliver_request(mutation, state))
}

/// Run response header filters early and deliver mutations per strategy.
pub(crate) async fn run_response_header_filters_early(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    delivery: MutationDelivery,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Ok(delivery.deliver_response(None, state));
    };

    let ctx = adapter::build_filter_context(pipeline, request);
    let Some(resp) = state.response.as_mut() else {
        return Ok(delivery.deliver_response(None, state));
    };

    // Hydrate only after the guard so an early return does not lose parked state.
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;
    let original_headers = capture_original_headers(resp);
    ctx.response_header = Some(resp);
    ctx.upstream_reached = true;

    let action = execute_response(pipeline, &mut ctx).await?;
    if let Some(imm) = immediate_from_action(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    state.header_state.response_filters_executed = true;
    let mutation = adapter::collect_response_header_mutations_diff(&ctx, &original_headers);
    ctx.dehydrate(&mut state.carried_context)?;

    Ok(delivery.deliver_response(mutation, state))
}

/// Capture response headers before filter execution.
fn capture_original_headers(resp: &Response) -> HeaderMap {
    resp.headers.clone()
}

/// Execute the request-phase pipeline.
async fn execute_request(pipeline: &FilterPipeline, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, Status> {
    pipeline
        .execute_http_request(ctx)
        .await
        .map_err(|e| Status::internal(e.to_string()))
}

/// Execute the response-phase pipeline.
async fn execute_response(pipeline: &FilterPipeline, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, Status> {
    pipeline
        .execute_http_response(ctx)
        .await
        .map_err(|e| Status::internal(e.to_string()))
}

/// Convert a short-circuiting [`FilterAction`] into an `ImmediateResponse`.
///
/// Both `Reject` and `TerminalResponse` end the stream with a local reply at
/// Envoy; every other action means the pipeline continues.
fn immediate_from_action(
    action: FilterAction,
) -> Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse> {
    match action {
        FilterAction::Reject(rejection) => {
            metrics::record_immediate_response();
            Some(adapter::rejection_to_immediate(&rejection))
        },
        FilterAction::TerminalResponse(terminal) => {
            metrics::record_immediate_response();
            Some(adapter::terminal_response_to_immediate(&terminal))
        },
        _ => None,
    }
}

// -----------------------------------------------------------------------------
// Filters
// -----------------------------------------------------------------------------

/// Run request body filters if the pipeline has body capabilities.
async fn run_body_filters(
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    body_buf: &mut Vec<u8>,
    eos: bool,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if body_buf.is_empty() {
        return Ok(None);
    }

    let mut body = Some(Bytes::from(mem::take(body_buf)));
    let action = pipeline
        .execute_http_request_body(ctx, &mut body, eos)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

    if let Some(b) = body {
        *body_buf = b.to_vec();
    }

    if let FilterAction::Reject(rejection) = action {
        return Ok(Some(adapter::rejection_to_immediate(&rejection)));
    }

    Ok(None)
}

/// Run response body filters (synchronous, per Pingora constraint).
fn run_resp_body_filters(
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    body_buf: &mut Vec<u8>,
    eos: bool,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if body_buf.is_empty() {
        return Ok(None);
    }

    let mut body = Some(Bytes::from(mem::take(body_buf)));
    let action = pipeline
        .execute_http_response_body(ctx, &mut body, eos)
        .map_err(|e| Status::internal(e.to_string()))?;

    if let Some(b) = body {
        *body_buf = b.to_vec();
    }

    if let FilterAction::Reject(rejection) = action {
        return Ok(Some(adapter::rejection_to_immediate(&rejection)));
    }

    Ok(None)
}

// -----------------------------------------------------------------------------
// Trailer-Closed Body Flush
// -----------------------------------------------------------------------------

/// Give STREAMED body filters their end-of-stream call when trailers close the
/// body.
///
/// Every chunk was answered as it arrived but none carried `end_of_stream`, so
/// filters that finish at end of stream (access logging, token accounting) would
/// otherwise never run it. Run the body filters once more with no data and the
/// flag set; any body bytes they produce have no message to ride on, but a
/// rejection still applies.
pub(crate) async fn flush_streamed_body_filters(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let request = state.request.as_ref().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "request");
        Status::invalid_argument("request headers not received")
    })?;
    let mut ctx = adapter::build_filter_context(pipeline, request);
    // Resolve the fallible response ref before hydrate drains carried state, so an
    // early return here leaves the parked state untouched.
    if !is_request {
        let resp = state.response.as_mut().ok_or_else(|| {
            metrics::record_invalid_argument("missing_headers", "response");
            Status::invalid_argument("response headers not received")
        })?;
        ctx.response_header = Some(resp);
        ctx.upstream_reached = true;
    }
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;
    let action = run_trailing_body(pipeline, &mut ctx, is_request).await?;
    ctx.dehydrate(&mut state.carried_context)?;
    Ok(immediate_from_action(action)
        .map(response::immediate)
        .into_iter()
        .collect())
}

/// Run body filters once with `end_of_stream` set and no data, warning if they
/// emit bytes that no message can carry.
async fn run_trailing_body(
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    is_request: bool,
) -> Result<FilterAction, Status> {
    let mut body: Option<Bytes> = None;
    let action = if is_request {
        pipeline.execute_http_request_body(ctx, &mut body, true).await
    } else {
        pipeline.execute_http_response_body(ctx, &mut body, true)
    }
    .map_err(|e| Status::internal(e.to_string()))?;

    if body.as_ref().is_some_and(|b| !b.is_empty()) {
        warn!(
            direction = direction_label(is_request),
            "body filters produced bytes at end of stream after trailers; no message can carry them"
        );
    }
    Ok(action)
}

/// Direction label for diagnostics.
pub(crate) const fn direction_label(is_request: bool) -> &'static str {
    if is_request { "request" } else { "response" }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Return a body slice reference if the buffer is non-empty.
fn body_data_if_present(buf: &[u8]) -> Option<&[u8]> {
    if buf.is_empty() { None } else { Some(buf) }
}

/// Merge deferred header mutations with current mutations.
///
/// When both are present, combines their `set_headers` and `remove_headers` vectors.
fn merge_mutations(
    deferred: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
    current: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
) -> Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation> {
    match (deferred, current) {
        (None, None) => None,
        (Some(m), None) | (None, Some(m)) => Some(m),
        (Some(mut d), Some(c)) => {
            d.set_headers.extend(c.set_headers);
            d.remove_headers.extend(c.remove_headers);
            Some(d)
        },
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::test_support::snapshot_counter;

    /// Read the `content-length` value from a header mutation, if present.
    fn content_length_of(
        mutation: &Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,
    ) -> Option<String> {
        mutation.as_ref()?.set_headers.iter().find_map(|h| {
            let hv = h.header.as_ref()?;
            hv.key.eq_ignore_ascii_case("content-length").then(|| hv.value.clone())
        })
    }

    #[test]
    fn immediate_from_action_converts_terminal_response() {
        let terminal = praxis_filter::TerminalResponse::new(200).with_body(Bytes::from_static(b"done"));
        let imm = immediate_from_action(FilterAction::TerminalResponse(Box::new(terminal))).unwrap();
        assert_eq!(imm.status.unwrap().code, 200, "terminal status carried through");
        assert_eq!(imm.body, "done", "terminal body carried through");
    }

    #[test]
    fn immediate_from_action_converts_reject() {
        let imm = immediate_from_action(FilterAction::Reject(praxis_filter::Rejection::status(403))).unwrap();
        assert_eq!(imm.status.unwrap().code, 403, "reject status carried through");
    }

    #[test]
    fn immediate_from_action_passes_through_continue() {
        assert!(
            immediate_from_action(FilterAction::Continue).is_none(),
            "Continue must not short-circuit"
        );
    }

    #[test]
    fn with_content_length_sets_on_resize() {
        let mutation = with_content_length(None, Some(b"shorter"), 100);
        assert_eq!(
            content_length_of(&mutation).as_deref(),
            Some("7"),
            "should declare new length"
        );
    }

    #[test]
    fn with_content_length_sets_on_shrink() {
        // A non-empty emitted body shrunk from a larger original.
        let mutation = with_content_length(None, Some(b"x"), 50);
        assert_eq!(content_length_of(&mutation).as_deref(), Some("1"));
    }

    #[test]
    fn with_content_length_noop_when_unchanged() {
        let mutation = with_content_length(None, Some(b"same"), 4);
        assert!(
            content_length_of(&mutation).is_none(),
            "unchanged size needs no correction"
        );
    }

    #[test]
    fn with_content_length_noop_without_body() {
        let mutation = with_content_length(None, None, 0);
        assert!(mutation.is_none(), "no emitted body means no content-length change");
    }

    /// Clearing a previously non-empty body must declare `content-length: 0`.
    ///
    /// The pipeline tails now pass the authoritative buffer as `Some` even when
    /// empty, so `with_content_length` sees emitted len 0 != original and emits
    /// the correction. Skipping it would leave a stale length against an empty
    /// body -- a request-smuggling vector under FDS `allow_content_length_header`.
    #[test]
    fn with_content_length_corrects_when_body_cleared() {
        let mutation = with_content_length(None, Some(b""), 100);
        assert_eq!(
            content_length_of(&mutation).as_deref(),
            Some("0"),
            "clearing the body must declare content-length: 0"
        );
    }

    #[tokio::test]
    async fn run_request_pipeline_missing_headers_records_metric() {
        use praxis_filter::FilterRegistry;

        let pipeline = FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).unwrap();
        let mut state = StreamState::new();

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        {
            let _guard = ::metrics::set_default_local_recorder(&recorder);
            let result = run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state).await;
            assert!(result.is_err(), "missing request headers must be rejected");
        }
        let count = snapshot_counter(
            &snapshotter,
            "praxis_extproc_invalid_argument_total",
            &[("reason", "missing_headers"), ("detail", "request")],
        );
        assert_eq!(count, 1, "missing request headers must increment the counter");
    }

    #[tokio::test]
    async fn run_response_pipeline_missing_response_headers_records_metric() {
        use praxis_filter::FilterRegistry;

        let pipeline = FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).unwrap();
        let mut state = StreamState::new();
        // Request headers present, response headers absent: isolates the response branch.
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        {
            let _guard = ::metrics::set_default_local_recorder(&recorder);
            let result = run_response_pipeline(ResponsePhase::Headers, &pipeline, &mut state).await;
            assert!(result.is_err(), "missing response headers must be rejected");
        }
        let count = snapshot_counter(
            &snapshotter,
            "praxis_extproc_invalid_argument_total",
            &[("reason", "missing_headers"), ("detail", "response")],
        );
        assert_eq!(count, 1, "missing response headers must increment the counter");
    }

    /// Typed marker a probe filter stashes on request and reads on response.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Probe(u64);
    /// Sentinel value carried through `filter_state`.
    const PROBE_VALUE: u64 = 0x00C0_FFEE;
    /// Value the probe observed in `on_response` (0 if state was lost).
    static PROBE_OBSERVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    /// `upstream_reached` as the probe saw it in `on_request`.
    static PROBE_UPSTREAM_ON_REQUEST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    /// `upstream_reached` as the probe saw it in `on_response`.
    static PROBE_UPSTREAM_ON_RESPONSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    /// Filter that stores `Probe` on request and reports it back on response.
    struct ProbeFilter;
    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for ProbeFilter {
        fn name(&self) -> &'static str {
            "state_probe"
        }

        async fn on_request(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            ctx.insert_filter_state(Probe(PROBE_VALUE));
            PROBE_UPSTREAM_ON_REQUEST.store(ctx.upstream_reached, std::sync::atomic::Ordering::SeqCst);
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            let observed = ctx.get_filter_state::<Probe>().map_or(0, |p| p.0);
            PROBE_OBSERVED.store(observed, std::sync::atomic::Ordering::SeqCst);
            PROBE_UPSTREAM_ON_RESPONSE.store(ctx.upstream_reached, std::sync::atomic::Ordering::SeqCst);
            Ok(FilterAction::Continue)
        }
    }
    impl ProbeFilter {
        /// Registry factory for `state_probe`.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// A pipeline of just the `state_probe` filter.
    fn state_probe_pipeline() -> Arc<FilterPipeline> {
        use praxis_filter::FilterRegistry;

        let cfg: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: state_probe\n")
                .unwrap();
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register("state_probe", praxis_filter::http_builtin(ProbeFilter::from_config))
            .unwrap();
        crate::config::build_pipeline(&cfg, &registry).unwrap()
    }

    /// Removes temporary files created by protocol integration tests even
    /// when an assertion fails before the normal cleanup path runs.
    struct TempFiles(Vec<std::path::PathBuf>);

    impl Drop for TempFiles {
        fn drop(&mut self) {
            for path in &self.0 {
                drop(std::fs::remove_file(path));
            }
        }
    }

    #[tokio::test]
    async fn filter_state_survives_request_to_response_phase() {
        use std::sync::atomic::Ordering;

        PROBE_OBSERVED.store(0, Ordering::SeqCst);
        let pipeline = state_probe_pipeline();
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // Request phase stores state; it must persist into StreamState.
        run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state)
            .await
            .unwrap();
        assert!(
            state.carried_context.as_ref().unwrap().filter_state.contains_key(&0),
            "request-phase filter_state must persist into StreamState"
        );
        assert!(
            !PROBE_UPSTREAM_ON_REQUEST.load(Ordering::SeqCst),
            "request filters must not see upstream_reached before a response phase"
        );

        // Response phase restores state; the probe surfaces what it observed.
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        run_response_pipeline(ResponsePhase::Headers, &pipeline, &mut state)
            .await
            .unwrap();
        assert_eq!(
            PROBE_OBSERVED.load(Ordering::SeqCst),
            PROBE_VALUE,
            "on_response must see state stored in on_request; 0 means it was dropped at the phase boundary"
        );
        assert!(
            PROBE_UPSTREAM_ON_RESPONSE.load(Ordering::SeqCst),
            "a response phase means Envoy reached the upstream; response filters must see upstream_reached"
        );
    }

    #[expect(
        clippy::too_many_lines,
        clippy::expect_used,
        clippy::panic,
        reason = "protocol handoff regression test deliberately exercises the complete filter chain"
    )]
    #[tokio::test]
    async fn post_auth_handoff_selects_provider_replaces_spoof_and_clears_route_cache() {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let overlay_path = std::env::temp_dir().join(format!("praxis-extproc-overlay-{suffix}.json"));
        let credential_path = std::env::temp_dir().join(format!("praxis-extproc-credential-{suffix}"));
        let _temporary_files = TempFiles(vec![overlay_path.clone(), credential_path.clone()]);
        std::fs::write(&overlay_path, r#"{
            "local_site":"local",
            "candidates":[{
                "kind":"inference_model",
                "name":"demo",
                "site":"local",
                "cluster":"provider-provider-a",
                "fresh":true,
                "stable_id":"provider-provider-a",
                "credential":{"strategy":"bearer_token","secretRef":{"name":"provider-a-secret","namespace":"tenant-a","key":"api-key"}}
            }]
        }"#).unwrap();
        std::fs::write(&credential_path, "projected-provider-token").unwrap();

        let config: crate::config::ExtProcConfig = serde_yaml::from_str(&format!(
            "filter_chains:\n  - name: post-auth\n    filters:\n      - filter: intelligent_route\n        overlay_file: {}\n        model_header: X-Gateway-Model-Name\n        provider_hop_clusters: [provider-provider-a]\n      - filter: credential_inject\n        credentials:\n          - name: provider-a-secret\n            namespace: tenant-a\n            key: api-key\n            strategy: bearer_token\n            file: {}\n",
            overlay_path.display(), credential_path.display()
        )).unwrap();
        let pipeline = crate::config::build_pipeline(&config, &praxis_ai_filters::build_ai_registry()).unwrap();
        let header = |key: &str, value: &str| praxis_proto::envoy::service::common::v3::HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: Vec::new(),
        };
        let request = adapter::envoy_headers_to_request(&[
            header(":method", "POST"),
            header(":path", "/tenant-a/demo/v1/chat/completions"),
            header("X-Gateway-Model-Name", "demo"),
            header("x-ai-routing-candidate", "provider-provider-b"),
            header("x-ai-routing-request-id", "attacker"),
            header("x-ai-routing-revision", "attacker"),
            header("authorization", "Bearer caller"),
            header("x-api-key", "caller-key"),
        ]);
        let mut context = adapter::build_filter_context(&pipeline, &request);
        let _action = pipeline.execute_http_request(&mut context).await.unwrap();
        let mutation = adapter::collect_request_header_mutations(&context).expect("handoff must mutate headers");

        let set = mutation
            .set_headers
            .iter()
            .filter_map(|header| header.header.as_ref())
            .collect::<Vec<_>>();
        assert!(
            set.iter()
                .any(|header| header.key == "x-ai-routing-candidate" && header.value == "provider-provider-a")
        );
        assert!(
            set.iter()
                .any(|header| header.key == "authorization" && header.value == "Bearer projected-provider-token")
        );
        for name in [
            "x-ai-routing-candidate",
            "x-ai-routing-request-id",
            "x-ai-routing-revision",
            "authorization",
            "x-api-key",
        ] {
            assert!(
                mutation.remove_headers.iter().any(|removed| removed == name),
                "missing removal for {name}"
            );
        }

        let response = response::request_headers(Some(mutation));
        let common = match response.response.unwrap() {
            praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::RequestHeaders(headers) => {
                headers.response.unwrap()
            },
            other => panic!("unexpected response variant: {other:?}"),
        };
        assert!(
            common.clear_route_cache,
            "trusted provider handoff must clear Envoy's route cache"
        );
    }

    /// Probe that stashes state on the request side and records what it
    /// observes on the response side.
    struct CarryProbe;

    impl CarryProbe {
        /// Response header surfacing the observed value on the terminal path.
        const OBSERVED_HEADER: &'static str = "x-carry-observed-state";
        /// Records whether the response side observed the request metadata.
        const OBSERVED_META_KEY: &'static str = "carry_probe.observed_meta";
        /// Records the `filter_state` value the response side observed.
        const OBSERVED_STATE_KEY: &'static str = "carry_probe.observed_state";
        /// Breadcrumb the request side leaves for the response side.
        const REQUEST_KEY: &'static str = "carry_probe.request";

        /// Stash typed state + a metadata breadcrumb on the request side.
        fn stash(ctx: &mut HttpFilterContext<'_>) {
            ctx.insert_filter_state(Probe(PROBE_VALUE));
            ctx.set_metadata(Self::REQUEST_KEY, "seen");
        }

        /// Record what the response side observed of the carry, into metadata
        /// and, when response headers are present, as a header.
        fn observe(ctx: &mut HttpFilterContext<'_>) {
            let state = ctx.get_filter_state::<Probe>().map_or(0, |p| p.0);
            let meta = u8::from(ctx.get_metadata(Self::REQUEST_KEY).is_some());
            ctx.set_metadata(Self::OBSERVED_STATE_KEY, state.to_string());
            ctx.set_metadata(Self::OBSERVED_META_KEY, meta.to_string());
            if let Some(resp) = ctx.response_header.as_deref_mut() {
                resp.headers
                    .insert(Self::OBSERVED_HEADER, state.to_string().parse().unwrap());
            }
        }

        /// Registry factory for `carry_probe`.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for CarryProbe {
        fn name(&self) -> &'static str {
            "carry_probe"
        }

        fn request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadOnly
        }

        fn response_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadOnly
        }

        async fn on_request(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Self::stash(ctx);
            Ok(FilterAction::Continue)
        }

        async fn on_request_body(
            &self,
            ctx: &mut HttpFilterContext<'_>,
            _body: &mut Option<Bytes>,
            _eos: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Self::stash(ctx);
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Self::observe(ctx);
            Ok(FilterAction::Continue)
        }

        fn on_response_body(
            &self,
            ctx: &mut HttpFilterContext<'_>,
            _body: &mut Option<Bytes>,
            _eos: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Self::observe(ctx);
            Ok(FilterAction::Continue)
        }
    }

    /// Build a single-filter pipeline containing `carry_probe`.
    fn carry_probe_pipeline() -> Arc<FilterPipeline> {
        use praxis_filter::FilterRegistry;

        let cfg: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: carry_probe\n")
                .unwrap();
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register("carry_probe", praxis_filter::http_builtin(CarryProbe::from_config))
            .unwrap();
        crate::config::build_pipeline(&cfg, &registry).unwrap()
    }

    /// Assert the response side observed both carried maps from the request side.
    fn assert_carry_observed(state: &StreamState) {
        let carried = state.carried_context.as_ref().unwrap();
        assert_eq!(
            carried
                .filter_metadata
                .get(CarryProbe::OBSERVED_STATE_KEY)
                .map(String::as_str),
            Some(PROBE_VALUE.to_string().as_str()),
            "response side must observe request-side filter_state carried across the phase boundary"
        );
        assert_eq!(
            carried
                .filter_metadata
                .get(CarryProbe::OBSERVED_META_KEY)
                .map(String::as_str),
            Some("1"),
            "response side must observe request-side filter_metadata carried across the phase boundary"
        );
    }

    #[tokio::test]
    async fn carried_state_survives_streamed_body_phases() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;

        let pipeline = carry_probe_pipeline();
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // Streamed request chunk stashes state; capture must persist it.
        let req_chunk = HttpBody {
            body: b"req".to_vec(),
            end_of_stream: true,
        };
        process_streamed_body_chunk(&pipeline, req_chunk, &mut state, true)
            .await
            .unwrap();
        assert!(
            state.carried_context.as_ref().unwrap().filter_state.contains_key(&0),
            "streamed request-body filter_state must persist into StreamState"
        );

        // Streamed response chunk builds a fresh ctx; hydrate must restore it.
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        let resp_chunk = HttpBody {
            body: b"resp".to_vec(),
            end_of_stream: true,
        };
        process_streamed_body_chunk(&pipeline, resp_chunk, &mut state, false)
            .await
            .unwrap();

        assert_carry_observed(&state);
    }

    #[tokio::test]
    async fn carried_state_survives_early_header_phases() {
        let pipeline = carry_probe_pipeline();
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // Early request-header execution stashes state; capture must persist it.
        run_request_header_filters_early(&pipeline, &mut state, MutationDelivery::Send)
            .await
            .unwrap();
        assert!(
            state.carried_context.as_ref().unwrap().filter_state.contains_key(&0),
            "early request-header filter_state must persist into StreamState"
        );

        // Early response-header execution (the buffered external_metering flow) restores it.
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        run_response_header_filters_early(&pipeline, &mut state, MutationDelivery::DeferWithResponse)
            .await
            .unwrap();

        assert_carry_observed(&state);
    }

    #[tokio::test]
    async fn early_response_headers_without_response_preserves_carried_state() {
        let pipeline = carry_probe_pipeline();
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state
            .carried_context
            .as_mut()
            .unwrap()
            .filter_metadata
            .insert("probe".to_owned(), "kept".to_owned());

        // response is None: the guard must return before hydrate drains carried state.
        run_response_header_filters_early(&pipeline, &mut state, MutationDelivery::DeferWithResponse)
            .await
            .unwrap();

        assert_eq!(
            state
                .carried_context
                .as_ref()
                .unwrap()
                .filter_metadata
                .get("probe")
                .map(String::as_str),
            Some("kept"),
            "response-None early return must not drain carried state"
        );
    }

    #[tokio::test]
    async fn streamed_response_body_without_response_preserves_carried_state() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;

        let pipeline = carry_probe_pipeline();
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        // response is intentionally left None.
        state
            .carried_context
            .as_mut()
            .unwrap()
            .filter_metadata
            .insert("probe".to_owned(), "kept".to_owned());

        let chunk = HttpBody {
            body: b"resp".to_vec(),
            end_of_stream: true,
        };
        let result = process_streamed_body_chunk(&pipeline, chunk, &mut state, false).await;

        assert!(
            matches!(&result, Err(e) if e.code() == tonic::Code::InvalidArgument),
            "a response-body chunk without response headers must error"
        );
        assert_eq!(
            state
                .carried_context
                .as_ref()
                .unwrap()
                .filter_metadata
                .get("probe")
                .map(String::as_str),
            Some("kept"),
            "the early error must not drain carried state"
        );
    }

    /// Completeness guard: every carried field survives a round trip.
    #[expect(
        clippy::too_many_lines,
        reason = "one guard enumerates every carried field across hydrate and dehydrate"
    )]
    #[test]
    fn carried_state_round_trips_every_field() {
        use crate::server::CarriedContext;

        let pipeline = carry_probe_pipeline();
        let request = adapter::envoy_headers_to_request(&[]);

        // Seed every carried field on a hydrated context, the way a filter would
        // (through DerefMut), so `hydrate` stays the only way to build one.
        let ctx = adapter::build_filter_context(&pipeline, &request);
        let mut hydrated = HydratedContext::hydrate(Some(CarriedContext::default()), ctx).unwrap();
        hydrated.branch_iterations.insert(Arc::from("branch-a"), 3);
        hydrated.executed_filter_indices = vec![true, false, true];
        hydrated
            .filter_metadata
            .insert("carry.meta".to_owned(), "kept".to_owned());
        hydrated.filter_state.insert(7, Box::new(Probe(PROBE_VALUE)));

        // Capture (`dehydrate`) must move every field into the empty slot.
        let mut slot = None;
        hydrated.dehydrate(&mut slot).unwrap();
        let carried = slot.take().unwrap();

        // Enumerate every carried field: a new field on `CarriedContext` fails to
        // compile here until it is asserted.
        let CarriedContext {
            branch_iterations,
            executed_filter_indices,
            filter_metadata,
            filter_state,
        } = &carried;
        assert_eq!(branch_iterations.get("branch-a"), Some(&3));
        assert_eq!(executed_filter_indices, &vec![true, false, true]);
        assert_eq!(filter_metadata.get("carry.meta").map(String::as_str), Some("kept"));
        assert!(filter_state.contains_key(&7));

        // hydrate must restore every field into a freshly built context,
        // consuming the parked value and emptying the slot.
        let fresh = adapter::build_filter_context(&pipeline, &request);
        let hydrated = HydratedContext::hydrate(Some(carried), fresh).unwrap();
        assert_eq!(hydrated.branch_iterations.get("branch-a"), Some(&3));
        assert_eq!(hydrated.executed_filter_indices, vec![true, false, true]);
        assert_eq!(hydrated.get_metadata("carry.meta"), Some("kept"));
        let restored = hydrated
            .filter_state
            .get(&7)
            .and_then(|any| any.downcast_ref::<Probe>());
        assert_eq!(restored, Some(&Probe(PROBE_VALUE)), "hydrate must restore filter_state");
    }

    /// A missing parked context (`None`) means a prior phase never restored its
    /// state: hydrate must surface it as an error instead of silently carrying an
    /// empty context.
    #[test]
    fn hydrate_reports_missing_carried_context() {
        let pipeline = carry_probe_pipeline();
        let request = adapter::envoy_headers_to_request(&[]);
        let fresh = adapter::build_filter_context(&pipeline, &request);

        let result = HydratedContext::hydrate(None, fresh);
        assert!(
            matches!(&result, Err(e) if e.code() == tonic::Code::Internal),
            "hydrate with no parked context must report an internal error"
        );
    }

    /// Hydrating twice from the same slot errors: the first `take` drains it to
    /// `None`, so the second hydrate has nothing to pour in.
    #[test]
    fn second_hydrate_from_drained_slot_errors() {
        use crate::server::CarriedContext;

        let pipeline = carry_probe_pipeline();
        let request = adapter::envoy_headers_to_request(&[]);
        let mut slot = Some(CarriedContext::default());

        // First hydrate drains the slot to None.
        let first = adapter::build_filter_context(&pipeline, &request);
        let _hydrated = HydratedContext::hydrate(slot.take(), first).unwrap();
        assert!(slot.is_none(), "hydrate's take must leave the slot None");

        // Second hydrate finds None and must error instead of carrying empty state.
        let second = adapter::build_filter_context(&pipeline, &request);
        let result = HydratedContext::hydrate(slot.take(), second);
        assert!(
            matches!(&result, Err(e) if e.code() == tonic::Code::Internal),
            "a second hydrate from a drained slot must report an internal error"
        );
    }

    /// Dehydrating into a slot that still holds parked state means a prior phase
    /// never drained it: dehydrate must surface it as an error instead of
    /// silently overwriting the parked context.
    #[test]
    fn dehydrate_reports_occupied_slot() {
        use crate::server::CarriedContext;

        let pipeline = carry_probe_pipeline();
        let request = adapter::envoy_headers_to_request(&[]);
        let fresh = adapter::build_filter_context(&pipeline, &request);
        let hydrated = HydratedContext::hydrate(Some(CarriedContext::default()), fresh).unwrap();

        // The slot is still occupied: a prior phase failed to drain it.
        let mut slot = Some(CarriedContext::default());
        slot.as_mut()
            .unwrap()
            .filter_metadata
            .insert("kept".to_owned(), "yes".to_owned());

        let result = hydrated.dehydrate(&mut slot);
        assert!(
            matches!(&result, Err(e) if e.code() == tonic::Code::Internal),
            "dehydrate into an occupied slot must report an internal error"
        );
        assert_eq!(
            slot.as_ref().unwrap().filter_metadata.get("kept").map(String::as_str),
            Some("yes"),
            "a rejected dehydrate must not overwrite the parked context"
        );
    }
}
