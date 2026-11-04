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
use praxis_proto::envoy::service::ext_proc::v3::{HeaderMutation, ProcessingResponse};
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

/// Runtime bounds and filter selection for one request-body phase.
#[derive(Clone, Copy)]
struct RequestBodyFilterOptions {
    /// Whether to run ordinary body filters before provider-specific filters.
    run_ordinary_body_filters: bool,
    /// Server-side ceiling for bytes after body filters rewrite the input.
    max_body_accumulation: Option<usize>,
}

/// Execute request pipeline for the given phase.
///
/// Returns headers or body response with mutations.
pub(crate) async fn run_request_pipeline(
    phase: RequestPhase,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    end_of_stream: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    run_request_pipeline_inner(phase, pipeline, state, end_of_stream, true).await
}

/// Run only selected-upstream body validation for a headers-only request.
pub(crate) async fn run_bodyless_selected_upstream_pipeline(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    run_request_pipeline_inner(RequestPhase::Body, pipeline, state, true, false).await
}

/// Execute one request phase with an explicit ordinary-body-filter policy.
#[expect(
    clippy::too_many_lines,
    reason = "request context borrows state across filter phases"
)]
async fn run_request_pipeline_inner(
    phase: RequestPhase,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    eos: bool,
    run_ordinary_body_filters: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        metrics::record_invalid_argument("missing_headers", "request");
        return Err(Status::invalid_argument("request headers not received"));
    };
    let ctx = adapter::build_filter_context(pipeline, request, state.trust_forwarded_for);
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;

    if let Some(immediate) =
        execute_request_filters_if_needed(pipeline, &mut ctx, &mut state.header_state.request_filters_executed).await?
    {
        return Ok(vec![response::immediate(immediate)]);
    }

    if selected_input_exceeds_limit(phase, pipeline, state.request_body.len()) {
        return Ok(vec![selected_input_limit_rejection()]);
    }

    let prior_len = state.request_body.len();
    let max_body_accumulation = state.max_body_accumulation;
    if let Some(imm) = run_request_body_filters_at_eos(
        phase,
        pipeline,
        &mut ctx,
        &mut state.request_body,
        RequestBodyFilterOptions {
            run_ordinary_body_filters,
            max_body_accumulation,
        },
    )
    .await?
    {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_request_header_mutations(&ctx);

    ctx.dehydrate(&mut state.carried_context)?;

    Ok(build_request(phase, mutation, prior_len, eos, state))
}

/// Check the selected input before a body filter can shrink or rewrite it.
fn selected_input_exceeds_limit(phase: RequestPhase, pipeline: &FilterPipeline, body_len: usize) -> bool {
    matches!(phase, RequestPhase::Body)
        && pipeline.body_capabilities().needs_selected_upstream_request_body
        && body_len > pipeline.selected_upstream_request_body_limit()
}

/// Return a local HTTP rejection for an oversized selected-provider input.
fn selected_input_limit_rejection() -> ProcessingResponse {
    metrics::record_body_size_rejection();
    response::immediate(adapter::rejection_to_immediate(&praxis_filter::Rejection::status(413)))
}

/// Execute request-header filters once while preserving saved route/profile metadata.
async fn execute_request_filters_if_needed(
    pipeline: &FilterPipeline,
    ctx: &mut HydratedContext<'_>,
    request_filters_executed: &mut bool,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if *request_filters_executed {
        return Ok(None);
    }
    let action = execute_request(pipeline, ctx).await?;
    let immediate = immediate_from_action(action);
    if immediate.is_none() {
        *request_filters_executed = true;
    }
    Ok(immediate)
}

/// Assemble the request response after request and selected-provider hooks finish.
fn build_request(
    phase: RequestPhase,
    mutation: Option<HeaderMutation>,
    original_len: usize,
    end_of_stream: bool,
    state: &mut StreamState,
) -> Vec<ProcessingResponse> {
    let body = Some(state.request_body.as_slice());

    if matches!(phase, RequestPhase::Body) && state.eos_tracker.request_headers.is_complete() {
        return build_bodyless_selected_request_response(mutation, original_len, state);
    }

    // Emit the authoritative buffer even when empty: a filter that cleared the
    // body must produce an explicit empty body AND content-length: 0. Collapsing
    // empty -> None here would drop both (buffered) or desync CL (FDS+flag).
    build_request_for_phase(
        phase,
        with_content_length(mutation, body, original_len),
        body,
        state.protocol_config.request_body_mode,
        end_of_stream,
    )
}

/// Build the sole headers response or reject bytes that have no body message to carry them.
fn build_bodyless_selected_request_response(
    mutation: Option<HeaderMutation>,
    original_len: usize,
    state: &mut StreamState,
) -> Vec<ProcessingResponse> {
    if !state.request_body.is_empty() {
        let rejection = praxis_filter::Rejection::status(500)
            .with_body(b"selected-upstream request-body filter produced bytes for a headers-only request".to_vec());
        return vec![response::immediate(adapter::rejection_to_immediate(&rejection))];
    }
    state.header_state.request_headers_sent = true;
    let mutation = with_content_length(mutation, Some(state.request_body.as_slice()), original_len);
    vec![response::request_headers(mutation)]
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
    end_of_stream: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        metrics::record_invalid_argument("missing_headers", "request");
        return Err(Status::invalid_argument("request headers not received"));
    };

    let mut resp = state.response.take().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "response");
        Status::invalid_argument("response headers not received")
    })?;

    let ctx = adapter::build_filter_context(pipeline, request, state.trust_forwarded_for);
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
        end_of_stream,
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

    match phase {
        ResponsePhase::Headers => Ok(None),
        ResponsePhase::Body => run_resp_body_filters(pipeline, ctx, response_body, true),
    }
}

/// Set `content-length` when the emitted body differs in size from the original.
///
/// Keeps the declared length in sync with the bytes actually emitted to Envoy,
/// including 0 when a filter clears the body. Left untouched only when the size
/// is unchanged. Honored by Envoy in `BUFFERED` and in `FULL_DUPLEX_STREAMED` with
/// `allow_content_length_header`; ignored (harmlessly) in `STREAMED`, where Envoy
/// strips content-length and switches to chunked encoding.
fn with_content_length(
    mutation: Option<HeaderMutation>,
    body: Option<&[u8]>,
    original_len: usize,
) -> Option<HeaderMutation> {
    match body {
        Some(b) if b.len() != original_len => Some(adapter::set_content_length(mutation, b.len())),
        _ => mutation,
    }
}

/// Build request-phase responses, prepending `HeadersResponse` in FDS mode.
///
/// `end_of_stream` marks the emitted body chunk final. It is `true` on the normal
/// body-EOS path and `false` when trailers close the body (the trailers message
/// ends the stream, and the proto forbids `end_of_stream=true` on a body response
/// whose request chunk was not itself final).
fn build_request_for_phase(
    phase: RequestPhase,
    mutation: Option<HeaderMutation>,
    body: Option<&[u8]>,
    mode: BodyMode,
    end_of_stream: bool,
) -> Vec<ProcessingResponse> {
    match (phase, mode) {
        (RequestPhase::Headers, _) => vec![response::request_headers(mutation)],
        (RequestPhase::Body, BodyMode::FullDuplexStreamed) => {
            let mut r = vec![response::request_headers(mutation)];
            r.extend(response::request_body(body, None, mode, end_of_stream));
            r
        },
        (RequestPhase::Body, _) => response::request_body(body, mutation, mode, end_of_stream),
    }
}

/// Build response-phase responses, prepending `ResponseHeadersResponse` in FDS mode.
///
/// See [`build_request_for_phase`] for the meaning of `end_of_stream`.
fn build_response_for_phase(
    phase: ResponsePhase,
    mutation: Option<HeaderMutation>,
    body: Option<&[u8]>,
    mode: BodyMode,
    end_of_stream: bool,
) -> Vec<ProcessingResponse> {
    match (phase, mode) {
        (ResponsePhase::Headers, _) => vec![response::response_headers(mutation)],
        (ResponsePhase::Body, BodyMode::FullDuplexStreamed) => {
            let mut r = vec![response::response_headers(mutation)];
            r.extend(response::response_body(body, None, mode, end_of_stream));
            r
        },
        (ResponsePhase::Body, _) => response::response_body(body, mutation, mode, end_of_stream),
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

    let mut responses = vec![deferred_header_response(state, is_request)];
    responses.extend(body_responses);
    responses
}

/// Build a `HeadersResponse` carrying the deferred header-phase mutation.
///
/// Consumes the parked `deferred_*_header_mutation` for the direction. Used to
/// flush a header mutation that was held back to ride out on a body message but
/// has not yet gone out.
pub(crate) fn deferred_header_response(state: &mut StreamState, is_request: bool) -> ProcessingResponse {
    let mutation = if is_request {
        state.deferred_request_header_mutation.take()
    } else {
        state.deferred_response_header_mutation.take()
    };
    if is_request {
        response::request_headers(mutation)
    } else {
        response::response_headers(mutation)
    }
}

/// Process a single body chunk in `STREAMED` mode.
///
/// Runs body filters on the chunk and responds immediately. Body filters may
/// derive header mutations from the body, so collect them from the filter
/// context and merge with any deferred header-phase mutation before responding.
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
    let mut ctx = adapter::build_filter_context(pipeline, request, state.trust_forwarded_for);
    // Resolve the fallible response ref before hydrate drains carried state, so an
    // early return here leaves the parked state untouched.
    let original_response_headers = if is_request {
        None
    } else {
        let resp = state.response.as_mut().ok_or_else(|| {
            metrics::record_invalid_argument("missing_headers", "response");
            Status::invalid_argument("response headers not received")
        })?;
        let original = capture_original_headers(resp);
        ctx.response_header = Some(resp);
        ctx.upstream_reached = true;
        Some(original)
    };
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;
    let eos = body.end_of_stream;
    let mut chunk = body.body;
    let original_chunk_had_bytes = !chunk.is_empty();
    let reject = if is_request {
        run_body_filters(pipeline, &mut ctx, &mut chunk, eos).await?
    } else {
        run_resp_body_filters(pipeline, &mut ctx, &mut chunk, eos)?
    };
    if let Some(imm) = reject {
        return Ok(vec![response::immediate(imm)]);
    }
    let current_mutation = if is_request {
        adapter::collect_request_header_mutations(&ctx)
    } else {
        original_response_headers
            .as_ref()
            .and_then(|original| adapter::collect_response_header_mutations_diff(&ctx, original))
    };
    warn_unapplied_header_mutations(is_request, current_mutation.as_ref());
    ctx.dehydrate(&mut state.carried_context)?;
    let body_mode = if is_request {
        state.protocol_config.request_body_mode
    } else {
        state.protocol_config.response_body_mode
    };

    // In streamed mode an empty body mutation is meaningful: a response filter
    // may suppress an upstream chunk while buffering a bounded JSON document.
    // Collapsing that result to `None` tells Envoy to forward the original
    // chunk, which leaks the untranslated provider bytes before the final
    // translated body is emitted.
    let body_data = if original_chunk_had_bytes && chunk.is_empty() {
        Some(chunk.as_slice())
    } else {
        body_data_if_present(&chunk)
    };
    let responses = if is_request {
        response::request_body(body_data, None, body_mode, eos)
    } else {
        response::response_body(body_data, None, body_mode, eos)
    };
    Ok(responses)
}

/// Once-per-process warning: `STREAMED` body filters produced header mutations.
static STREAMED_HEADER_MUTATIONS: OnceWarning = OnceWarning::new();

/// A configuration-mismatch warning that fires once per process.
///
/// The condition it guards depends on the pipeline and on Envoy's processing
/// mode, not on the request, so repeating it for every stream would only flood
/// the log; later occurrences are suppressed.
struct OnceWarning(std::sync::atomic::AtomicBool);

impl OnceWarning {
    /// A warning that has not fired yet.
    const fn new() -> Self {
        Self(std::sync::atomic::AtomicBool::new(false))
    }

    /// Whether this call is the first; every later call returns `false`.
    fn first(&self) -> bool {
        !self.0.swap(true, std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the warning has not fired yet, without firing it.
    fn pending(&self) -> bool {
        !self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Warn (once per process) when body filters changed headers that Envoy will not apply.
///
/// Header mutations on a body response only take effect in `BUFFERED` mode.
/// In `STREAMED` mode the headers were forwarded when the `HeadersResponse`
/// went out, so a filter that derives headers from the body is silently
/// ineffective. Surface that so operators can switch the direction to
/// `BUFFERED` or `FULL_DUPLEX_STREAMED`.
fn warn_unapplied_header_mutations(is_request: bool, mutation: Option<&HeaderMutation>) {
    if !STREAMED_HEADER_MUTATIONS.pending() {
        return;
    }

    if let Some(mutation) = mutation
        && STREAMED_HEADER_MUTATIONS.first()
    {
        warn!(
            direction = if is_request { "request" } else { "response" },
            set_headers = mutation.set_headers.len(),
            remove_headers = mutation.remove_headers.len(),
            "STREAMED body filters produced header mutations, which Envoy applies only in BUFFERED mode; dropped"
        );
    }
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
    fn deliver_request(self, mutation: Option<HeaderMutation>, state: &mut StreamState) -> Vec<ProcessingResponse> {
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
    fn deliver_response(self, mutation: Option<HeaderMutation>, state: &mut StreamState) -> Vec<ProcessingResponse> {
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
    let ctx = adapter::build_filter_context(pipeline, request, state.trust_forwarded_for);
    let mut ctx = HydratedContext::hydrate(state.carried_context.take(), ctx)?;

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = immediate_from_action(action) {
        return Ok(vec![response::immediate(imm)]);
    }
    state.header_state.request_filters_executed = true;

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

    let ctx = adapter::build_filter_context(pipeline, request, state.trust_forwarded_for);
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

/// Run request-body filters at end of stream, skipping the headers-only phase.
///
/// The headers phase is reached only for a headers-only request (headers carried
/// `end_of_stream`), so there is no body to filter. The body phase always runs the
/// hook, even for an empty buffer, so an end-of-stream rejection still fires.
async fn run_request_body_filters_at_eos(
    phase: RequestPhase,
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    request_body: &mut Vec<u8>,
    options: RequestBodyFilterOptions,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    match phase {
        RequestPhase::Headers => Ok(None),
        RequestPhase::Body => {
            if options.run_ordinary_body_filters {
                let rejection = run_body_filters(pipeline, ctx, request_body, true).await?;
                if rejection.is_some() {
                    return Ok(rejection);
                }
            }
            run_selected_upstream_body_filters(pipeline, ctx, request_body, options.max_body_accumulation).await
        },
    }
}

/// Run request body filters if the pipeline has body capabilities.
async fn run_body_filters(
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    body_buf: &mut Vec<u8>,
    eos: bool,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if body_buf.is_empty() && !eos {
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

    Ok(immediate_from_action(action))
}

/// Run selected-upstream body filters after request headers, including Envoy-routed requests.
///
/// The filter's request limit is checked against the incoming body before this hook. Rewritten
/// output uses the server accumulation ceiling so translation overhead does not consume the
/// client's input allowance a second time.
async fn run_selected_upstream_body_filters(
    pipeline: &FilterPipeline,
    ctx: &mut HttpFilterContext<'_>,
    body_buf: &mut Vec<u8>,
    max_body_accumulation: Option<usize>,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if !pipeline.body_capabilities().needs_selected_upstream_request_body {
        return Ok(None);
    }

    let mut body = Some(Bytes::from(mem::take(body_buf)));
    let action = pipeline
        .execute_http_selected_upstream_request_body(ctx, &mut body)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
    if let Some(bytes) = body {
        *body_buf = bytes.to_vec();
    }
    if let Some(immediate) = immediate_from_action(action) {
        return Ok(Some(immediate));
    }
    if pipeline.body_capabilities().any_selected_upstream_request_body_writer
        && max_body_accumulation.is_some_and(|limit| body_buf.len() > limit)
    {
        metrics::record_body_size_rejection();
        return Ok(Some(adapter::rejection_to_immediate(
            &praxis_filter::Rejection::status(413),
        )));
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
    if body_buf.is_empty() && !eos {
        return Ok(None);
    }

    let mut body = Some(Bytes::from(mem::take(body_buf)));
    let action = pipeline
        .execute_http_response_body(ctx, &mut body, eos)
        .map_err(|e| Status::internal(e.to_string()))?;

    if let Some(b) = body {
        *body_buf = b.to_vec();
    }

    Ok(immediate_from_action(action))
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
/// flag set; a reject or terminal response still applies, while body bytes
/// produced here have no message to ride on and are dropped (see
/// [`run_trailing_body`]).
pub(crate) async fn flush_streamed_body_filters(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let request = state.request.as_ref().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "request");
        Status::invalid_argument("request headers not received")
    })?;
    let mut ctx = adapter::build_filter_context(pipeline, request, state.trust_forwarded_for);
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

/// Run body filters once with `end_of_stream` set and no data.
///
/// Bytes produced here can only ride a body message, but trailers already closed
/// the body: the next message is the trailers ack, which carries no body. An
/// immediate action (reject or terminal response) supersedes the body; any other
/// leftover bytes have no message to carry them and are dropped with a warning.
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
fn merge_mutations(deferred: Option<HeaderMutation>, current: Option<HeaderMutation>) -> Option<HeaderMutation> {
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

    fn immediate_status(responses: &[ProcessingResponse]) -> Option<i32> {
        let praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ImmediateResponse(immediate) =
            responses.first()?.response.as_ref()?
        else {
            return None;
        };
        immediate.status.as_ref().map(|status| status.code)
    }

    /// Read the `content-length` value from a header mutation, if present.
    fn content_length_of(mutation: &Option<HeaderMutation>) -> Option<String> {
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
            let result = run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state, true).await;
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
            let result = run_response_pipeline(ResponsePhase::Headers, &pipeline, &mut state, true).await;
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

    /// Filter that rejects from its body hooks.
    struct RejectBodyFilter;
    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for RejectBodyFilter {
        fn name(&self) -> &'static str {
            "reject_body"
        }

        fn request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        fn response_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        async fn on_request(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_request_body(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
            _body: &mut Option<Bytes>,
            _end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Reject(praxis_filter::Rejection::status(403)))
        }

        fn on_response_body(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
            _body: &mut Option<Bytes>,
            _end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Reject(praxis_filter::Rejection::status(403)))
        }
    }
    impl RejectBodyFilter {
        /// Registry factory for `reject_body`.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Suppresses streamed provider chunks until its final translated chunk.
    struct StreamedResponseReplacementFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for StreamedResponseReplacementFilter {
        fn name(&self) -> &'static str {
            "streamed_response_replacement"
        }

        fn response_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_response(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        fn on_response_body(
            &self,
            _: &mut HttpFilterContext<'_>,
            body: &mut Option<Bytes>,
            end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            *body = Some(if end_of_stream {
                Bytes::from_static(b"translated-json")
            } else {
                Bytes::new()
            });
            Ok(FilterAction::Continue)
        }
    }

    impl StreamedResponseReplacementFilter {
        /// Registry factory for streamed response replacement tests.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Suppresses one streamed request chunk before forwarding a later replacement.
    struct StreamedRequestReplacementFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for StreamedRequestReplacementFilter {
        fn name(&self) -> &'static str {
            "streamed_request_replacement"
        }

        fn request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_request_body(
            &self,
            _: &mut HttpFilterContext<'_>,
            body: &mut Option<Bytes>,
            end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            *body = Some(if end_of_stream {
                Bytes::from_static(b"replacement")
            } else {
                Bytes::new()
            });
            Ok(FilterAction::Continue)
        }
    }

    impl StreamedRequestReplacementFilter {
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Rewrites a body using metadata selected in the ExtProc request-header phase.
    struct SelectedBodyRewriteFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for SelectedBodyRewriteFilter {
        fn name(&self) -> &'static str {
            "selected_body_rewrite"
        }

        async fn on_request(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            let provider = if ctx.get_metadata("selected.provider").is_some() {
                "provider-called-twice"
            } else {
                "provider-a"
            };
            ctx.set_metadata("selected.provider", provider);
            ctx.cluster = Some(Arc::from("provider-a"));
            Ok(FilterAction::Continue)
        }

        fn selected_upstream_request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        fn request_body_mode(&self) -> praxis_filter::BodyMode {
            praxis_filter::BodyMode::StreamBuffer { max_bytes: Some(1024) }
        }

        async fn on_selected_upstream_request_body(
            &self,
            ctx: &mut HttpFilterContext<'_>,
            body: &mut Option<Bytes>,
        ) -> Result<praxis_filter::SelectedUpstreamBodyOutcome, praxis_filter::FilterError> {
            let selected = ctx.get_metadata("selected.provider").unwrap_or("missing");
            let bytes = body.as_ref().map_or(&[][..], Bytes::as_ref);
            let rewritten = format!("{}|{selected}", String::from_utf8_lossy(bytes));
            *body = Some(Bytes::from(rewritten));
            Ok(praxis_filter::SelectedUpstreamBodyOutcome::Continue)
        }
    }

    impl SelectedBodyRewriteFilter {
        /// Registry factory for the selected-upstream body-phase test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Leaves an empty body unchanged while still exercising the selected-upstream body hook.
    struct SelectedBodyNoopFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for SelectedBodyNoopFilter {
        fn name(&self) -> &'static str {
            "selected_body_noop"
        }

        async fn on_request(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            ctx.cluster = Some(Arc::from("provider-a"));
            Ok(FilterAction::Continue)
        }

        fn selected_upstream_request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        fn request_body_mode(&self) -> praxis_filter::BodyMode {
            praxis_filter::BodyMode::StreamBuffer { max_bytes: Some(1024) }
        }

        async fn on_selected_upstream_request_body(
            &self,
            _: &mut HttpFilterContext<'_>,
            _: &mut Option<Bytes>,
        ) -> Result<praxis_filter::SelectedUpstreamBodyOutcome, praxis_filter::FilterError> {
            Ok(praxis_filter::SelectedUpstreamBodyOutcome::Continue)
        }
    }

    impl SelectedBodyNoopFilter {
        /// Registry factory for the empty selected-upstream body test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Rejects from the selected-upstream body phase, after request selection.
    struct SelectedBodyRejectFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for SelectedBodyRejectFilter {
        fn name(&self) -> &'static str {
            "selected_body_reject"
        }

        async fn on_request(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            ctx.cluster = Some(Arc::from("provider-a"));
            Ok(FilterAction::Continue)
        }

        fn selected_upstream_request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        fn request_body_mode(&self) -> praxis_filter::BodyMode {
            praxis_filter::BodyMode::StreamBuffer { max_bytes: Some(1024) }
        }

        async fn on_selected_upstream_request_body(
            &self,
            _: &mut HttpFilterContext<'_>,
            _: &mut Option<Bytes>,
        ) -> Result<praxis_filter::SelectedUpstreamBodyOutcome, praxis_filter::FilterError> {
            Ok(praxis_filter::SelectedUpstreamBodyOutcome::Reject(
                praxis_filter::Rejection::status(422).with_body(b"selected body rejected".to_vec()),
            ))
        }
    }

    impl SelectedBodyRejectFilter {
        /// Registry factory for the selected-upstream rejection test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// A chain may register a selected-body participant without choosing a route.
    struct UnselectedBodyFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for UnselectedBodyFilter {
        fn name(&self) -> &'static str {
            "unselected_body"
        }

        async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        fn selected_upstream_request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        async fn on_selected_upstream_request_body(
            &self,
            _: &mut HttpFilterContext<'_>,
            _: &mut Option<Bytes>,
        ) -> Result<praxis_filter::SelectedUpstreamBodyOutcome, praxis_filter::FilterError> {
            Ok(praxis_filter::SelectedUpstreamBodyOutcome::Reject(
                praxis_filter::Rejection::status(422),
            ))
        }
    }

    impl UnselectedBodyFilter {
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Filter that emits body bytes at end of stream.
    struct TrailingByteFilter;
    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for TrailingByteFilter {
        fn name(&self) -> &'static str {
            "trailing_bytes"
        }

        fn request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        async fn on_request(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_request_body(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
            body: &mut Option<Bytes>,
            end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            if end_of_stream {
                *body = Some(Bytes::from_static(b"late"));
            }
            Ok(FilterAction::Continue)
        }
    }
    impl TrailingByteFilter {
        /// Registry factory for `trailing_bytes`.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Registry factory signature for the single-filter test pipelines.
    type FilterFactoryFn =
        fn(&serde_yaml::Value) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError>;

    /// Build a single-filter pipeline from a registry factory.
    fn single_filter_pipeline(name: &'static str, factory: FilterFactoryFn) -> Arc<FilterPipeline> {
        use praxis_filter::FilterRegistry;

        let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(&format!(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: {name}\n"
        ))
        .unwrap();
        let mut registry = FilterRegistry::with_builtins();
        registry.register(name, praxis_filter::http_builtin(factory)).unwrap();
        crate::config::build_pipeline(&cfg, &registry).unwrap()
    }

    /// Pipeline with a rejecting ordinary body filter and a passing selected-upstream hook.
    fn bodyless_selected_pipeline() -> Arc<FilterPipeline> {
        use praxis_filter::FilterRegistry;

        let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: reject_body\n      - filter: selected_body_noop\n",
        )
        .unwrap();
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register(
                "reject_body",
                praxis_filter::http_builtin(RejectBodyFilter::from_config),
            )
            .unwrap();
        registry
            .register(
                "selected_body_noop",
                praxis_filter::http_builtin(SelectedBodyNoopFilter::from_config),
            )
            .unwrap();
        crate::config::build_pipeline(&cfg, &registry).unwrap()
    }

    /// A reject from a request-body filter must short-circuit with an immediate
    /// response and record the immediate-response metric, matching the header
    /// paths (both now route through `immediate_from_action`).
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn request_body_reject_short_circuits_and_records_metric() {
        let pipeline = single_filter_pipeline("reject_body", RejectBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = b"payload".to_vec();

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let responses = {
            let _guard = ::metrics::set_default_local_recorder(&recorder);
            run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
                .await
                .expect("a body reject must produce an immediate response, not an error")
        };
        assert_eq!(
            responses.len(),
            1,
            "a body reject yields exactly one immediate response"
        );
        assert!(
            responses.first().is_some_and(response::is_immediate),
            "a body-filter reject must be delivered as an immediate response"
        );
        assert_eq!(
            snapshot_counter(&snapshotter, "praxis_extproc_immediate_responses_total", &[]),
            1,
            "a body-filter reject must record the immediate-response metric like the header paths"
        );
    }

    /// ExtProc must run selected-upstream body filters after header filters have bound a provider.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn selected_upstream_body_filter_runs_after_request_header_selection() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = b"request".to_vec();

        run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state, false)
            .await
            .expect("request headers should select the provider");
        run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("selected-upstream body phase should run");

        assert_eq!(
            state.request_body, b"request|provider-a",
            "selected provider metadata must reach the body rewrite"
        );
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn envoy_routed_request_runs_selected_upstream_hook() {
        let pipeline = single_filter_pipeline("unselected_body", UnselectedBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = b"unchanged".to_vec();

        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("Envoy-routed request should run the configured body hook");
        assert_eq!(immediate_status(&responses), Some(422));
        assert_eq!(state.request_body, b"unchanged");
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn envoy_routed_body_hook_rejects_unsupported_request_modes() {
        let pipeline = single_filter_pipeline("unselected_body", UnselectedBodyFilter::from_config);
        for mode in [BodyMode::None, BodyMode::Streamed, BodyMode::FullDuplexStreamed] {
            let mut state = StreamState::new();
            state.protocol_config.request_body_mode = mode;
            let headers = crate::handlers::handle_request_headers(
                &pipeline,
                praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
                &mut state,
            )
            .await
            .expect("unsupported body mode should produce a local rejection");
            assert_eq!(
                immediate_status(&headers),
                Some(500),
                "mode {mode:?} must reject before forwarding"
            );
        }
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn envoy_routed_body_hook_enforces_input_limit() {
        let pipeline = single_filter_pipeline("unselected_body", UnselectedBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = vec![b' '; pipeline.selected_upstream_request_body_limit() + 1];
        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("oversized Envoy-routed request should produce a local rejection");
        assert_eq!(immediate_status(&responses), Some(413));
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn selected_upstream_writer_allows_expansion_within_accumulation_limit() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        let input_limit = pipeline.selected_upstream_request_body_limit();
        state.request_body = vec![b'a'; input_limit];
        state.max_body_accumulation = Some(2048);

        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("rewritten output within the server accumulation limit should pass");
        let emitted = responses.first().and_then(emitted_request_body);
        assert_eq!(emitted.map(<[u8]>::len), Some(input_limit + 11));
        assert!(
            emitted.is_some_and(|body| body.len() > input_limit),
            "rewriting may expand beyond the client-input limit"
        );
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn selected_upstream_writer_rejects_output_over_accumulation_limit() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = vec![b'a'; pipeline.selected_upstream_request_body_limit()];
        state.max_body_accumulation = Some(1024);

        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("output over the configured accumulation limit should receive a local reply");
        assert_eq!(immediate_status(&responses), Some(413));
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn selected_upstream_filter_rejects_oversized_input_before_rewrite() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = vec![b'a'; pipeline.selected_upstream_request_body_limit() + 1];

        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("oversized selected input should receive a local reply");
        assert_eq!(immediate_status(&responses), Some(413));
        assert_eq!(
            state.request_body.len(),
            pipeline.selected_upstream_request_body_limit() + 1
        );
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn buffered_selected_request_limit_returns_http_413() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::Buffered;
        state.max_body_accumulation = Some(4);
        let responses = crate::handlers::handle_request_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"oversized".to_vec(),
                end_of_stream: true,
            },
            &mut state,
        )
        .await
        .expect("selected body limit should be an HTTP reply");
        assert_eq!(immediate_status(&responses), Some(413));
        assert!(state.request_body.is_empty());
    }

    /// Selected-upstream hook rejection follows the same immediate-response path as body-hook rejection.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(clippy::panic, reason = "tests")]
    async fn selected_upstream_body_filter_rejection_short_circuits() {
        let pipeline = single_filter_pipeline("selected_body_reject", SelectedBodyRejectFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.request_body = b"request".to_vec();

        run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state, false)
            .await
            .expect("request headers should complete before selected-upstream body handling");
        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("selected-upstream rejection should be returned as an immediate response");

        assert_eq!(responses.len(), 1, "rejection yields exactly one response");
        assert!(
            responses.first().is_some_and(response::is_immediate),
            "selected-upstream rejection must not become a gRPC processing failure"
        );
        let Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ImmediateResponse(
            immediate,
        )) = responses.first().and_then(|response| response.response.as_ref())
        else {
            panic!("selected-upstream rejection must be an ExtProc ImmediateResponse");
        };
        assert_eq!(immediate.status.as_ref().map(|status| status.code), Some(422));
        assert_eq!(immediate.body, "selected body rejected");
    }

    /// Header EOS is a bodyless request and must still run selected-upstream validation.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(clippy::panic, reason = "tests")]
    async fn selected_upstream_body_filter_rejection_runs_for_bodyless_request() {
        let pipeline = single_filter_pipeline("selected_body_reject", SelectedBodyRejectFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::Streamed;

        let responses = crate::handlers::handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders {
                end_of_stream: true,
                ..Default::default()
            },
            &mut state,
        )
        .await
        .expect("bodyless selected-upstream rejection should be an immediate response");

        assert_eq!(responses.len(), 1, "rejection yields exactly one response");
        let Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ImmediateResponse(
            immediate,
        )) = responses.first().and_then(|response| response.response.as_ref())
        else {
            panic!("bodyless selected-upstream rejection must be an ExtProc ImmediateResponse");
        };
        assert_eq!(immediate.status.as_ref().map(|status| status.code), Some(422));
        assert_eq!(immediate.body, "selected body rejected");
    }

    /// Headers-only requests run selected-provider validation without invoking ordinary body filters.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(
        clippy::too_many_lines,
        reason = "the test covers bodyless requests across supported body modes"
    )]
    async fn bodyless_selected_request_skips_regular_body_filters_and_answers_headers_only() {
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response as ResponsePayload;

        let pipeline = bodyless_selected_pipeline();
        for mode in [
            BodyMode::None,
            BodyMode::Buffered,
            BodyMode::Streamed,
            BodyMode::FullDuplexStreamed,
        ] {
            let mut state = StreamState::new();
            state.protocol_config.request_body_mode = mode;
            let responses = crate::handlers::handle_request_headers(
                &pipeline,
                praxis_proto::envoy::service::ext_proc::v3::HttpHeaders {
                    end_of_stream: true,
                    ..Default::default()
                },
                &mut state,
            )
            .await
            .expect("a bodyless request should pass the selected hook");

            assert_eq!(
                responses.len(),
                1,
                "headers-only request in {mode:?} needs one response"
            );
            assert!(
                matches!(
                    responses.first().and_then(|item| item.response.as_ref()),
                    Some(ResponsePayload::RequestHeaders(_))
                ),
                "headers-only request in {mode:?} must not receive a RequestBody response"
            );
        }
    }

    /// Selected-upstream mutation bytes cannot be delivered when headers end the request.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(clippy::panic, reason = "tests assert the explicit rejection response shape")]
    async fn bodyless_selected_request_rejects_unrepresentable_body_bytes() {
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response as ResponsePayload;

        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::Buffered;
        let responses = crate::handlers::handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders {
                end_of_stream: true,
                ..Default::default()
            },
            &mut state,
        )
        .await
        .expect("unrepresentable body bytes should produce a fail-closed local response");

        let Some(ResponsePayload::ImmediateResponse(immediate)) =
            responses.first().and_then(|item| item.response.as_ref())
        else {
            panic!("body bytes without a RequestBody message must be rejected locally");
        };
        assert_eq!(immediate.status.as_ref().map(|status| status.code), Some(500));
        assert!(immediate.body.contains("headers-only request"));
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn bodyful_selected_request_rejects_none_mode_before_header_ack() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::None;
        let responses = crate::handlers::handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .expect("unsafe NONE mode should get a local reply");
        assert_eq!(immediate_status(&responses), Some(500));
        assert!(!state.header_state.request_headers_sent);
    }

    /// Selected-upstream request-body processing cannot use FDS's pass-through body path.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(clippy::panic, reason = "tests assert the explicit rejection response shape")]
    async fn full_duplex_selected_upstream_body_processing_rejects_before_header_ack() {
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response as ResponsePayload;

        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::FullDuplexStreamed;
        let responses = crate::handlers::handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .expect("unsupported full-duplex selection should be rejected locally");

        let Some(ResponsePayload::ImmediateResponse(immediate)) =
            responses.first().and_then(|item| item.response.as_ref())
        else {
            panic!("unsupported full-duplex selection must not acknowledge headers");
        };
        assert_eq!(immediate.status.as_ref().map(|status| status.code), Some(500));
        assert!(immediate.body.contains("FULL_DUPLEX_STREAMED"));
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn streamed_selected_upstream_requires_buffered_mode() {
        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::Streamed;
        let responses = crate::handlers::handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .expect("selected STREAMED mode should receive a local reply");
        assert_eq!(immediate_status(&responses), Some(500));
        assert!(!state.header_state.request_headers_sent);
    }

    fn emitted_request_body(response: &ProcessingResponse) -> Option<&[u8]> {
        use praxis_proto::envoy::service::ext_proc::v3::{
            body_mutation::Mutation, processing_response::Response as ProcessingResponsePayload,
        };

        let ProcessingResponsePayload::RequestBody(body) = response.response.as_ref()? else {
            return None;
        };
        let mutation = body.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref()?;
        match mutation {
            Mutation::Body(bytes) => Some(bytes),
            _ => None,
        }
    }

    fn emitted_response_body(response: &ProcessingResponse) -> Option<&[u8]> {
        use praxis_proto::envoy::service::ext_proc::v3::{
            body_mutation::Mutation, processing_response::Response as ProcessingResponsePayload,
        };

        let ProcessingResponsePayload::ResponseBody(body) = response.response.as_ref()? else {
            return None;
        };
        let mutation = body.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref()?;
        match mutation {
            Mutation::Body(bytes) => Some(bytes),
            _ => None,
        }
    }

    /// Empty streamed replacements must suppress raw provider chunks at Envoy.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(
        clippy::too_many_lines,
        reason = "the test exercises both sides of a streamed JSON replacement"
    )]
    async fn streamed_response_filter_emits_empty_mutation_before_translation() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;

        let pipeline = single_filter_pipeline(
            "streamed_response_replacement",
            StreamedResponseReplacementFilter::from_config,
        );
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        state.protocol_config.response_body_mode = BodyMode::Streamed;

        let suppressed = process_streamed_body_chunk(
            &pipeline,
            HttpBody {
                body: b"raw-provider-json-part".to_vec(),
                end_of_stream: false,
            },
            &mut state,
            false,
        )
        .await
        .expect("the first response chunk should be filtered");
        assert_eq!(suppressed.first().and_then(emitted_response_body), Some(&[][..]));

        let translated = process_streamed_body_chunk(
            &pipeline,
            HttpBody {
                body: b"raw-provider-json-tail".to_vec(),
                end_of_stream: true,
            },
            &mut state,
            false,
        )
        .await
        .expect("the final response chunk should contain only translated JSON");
        assert_eq!(
            translated.first().and_then(emitted_response_body),
            Some(&b"translated-json"[..])
        );
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(clippy::too_many_lines, reason = "the test checks both streamed request mutations")]
    async fn streamed_request_filter_emits_empty_mutation_before_replacement() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;

        let pipeline = single_filter_pipeline(
            "streamed_request_replacement",
            StreamedRequestReplacementFilter::from_config,
        );
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.protocol_config.request_body_mode = BodyMode::Streamed;

        let suppressed = process_streamed_body_chunk(
            &pipeline,
            HttpBody {
                body: b"original".to_vec(),
                end_of_stream: false,
            },
            &mut state,
            true,
        )
        .await
        .expect("the first request chunk should be suppressed");
        assert_eq!(suppressed.first().and_then(emitted_request_body), Some(&[][..]));

        let replaced = process_streamed_body_chunk(
            &pipeline,
            HttpBody {
                body: b"tail".to_vec(),
                end_of_stream: true,
            },
            &mut state,
            true,
        )
        .await
        .expect("the final request chunk should be replaced");
        assert_eq!(
            replaced.first().and_then(emitted_request_body),
            Some(&b"replacement"[..])
        );
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(
        clippy::too_many_lines,
        reason = "the test checks body and trailer response ordering"
    )]
    async fn buffered_selected_body_with_trailers_replies_before_trailers() {
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response as Payload;

        let pipeline = single_filter_pipeline("selected_body_rewrite", SelectedBodyRewriteFilter::from_config);
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::Buffered;
        let headers = crate::handlers::handle_request_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .expect("buffered request headers should be acknowledged");
        assert!(matches!(
            headers.first().and_then(|item| item.response.as_ref()),
            Some(Payload::RequestHeaders(_))
        ));

        let body = crate::handlers::handle_request_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"request".to_vec(),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .expect("buffered body must be translated before Envoy sends trailers");
        assert_eq!(
            body.first().and_then(emitted_request_body),
            Some(&b"request|provider-a"[..])
        );

        let trailers = crate::handlers::handle_trailers(&pipeline, &mut state, true)
            .await
            .expect("trailers should be acknowledged without repeating translation");
        assert_eq!(trailers.len(), 1);
        assert!(matches!(
            trailers.first().and_then(|item| item.response.as_ref()),
            Some(Payload::RequestTrailers(_))
        ));
    }

    /// A reject from a response-body filter must short-circuit too.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn response_body_reject_short_circuits() {
        let pipeline = single_filter_pipeline("reject_body", RejectBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        state.response_body = b"payload".to_vec();

        let responses = run_response_pipeline(ResponsePhase::Body, &pipeline, &mut state, true)
            .await
            .expect("a body reject must produce an immediate response, not an error");
        assert!(
            responses.iter().any(response::is_immediate),
            "a body-filter reject must be delivered as an immediate response"
        );
    }

    /// When trailers close a streamed body and a filter emits bytes at end of
    /// stream, no ExtProc message can carry them; the flush drops them with a
    /// warning and acknowledges the trailers rather than emitting a response.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn trailing_body_bytes_without_carrier_dropped() {
        let pipeline = single_filter_pipeline("trailing_bytes", TrailingByteFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // Run the headers phase first so the filter is marked executed and its
        // context carries into the trailer-close flush.
        run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state, true)
            .await
            .expect("headers phase must succeed");

        let responses = flush_streamed_body_filters(&pipeline, &mut state, true)
            .await
            .expect("undeliverable end-of-stream bytes are dropped, not an error");
        assert!(
            responses.is_empty(),
            "dropped trailing bytes without a rejection yield no response: {responses:?}"
        );
    }

    /// A body filter that rejects at end of stream must fire even when the
    /// buffered body is empty (e.g. trailers closed it with no bytes); otherwise
    /// skipping the empty-buffer EOS call silently drops the rejection.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn empty_buffered_request_body_still_runs_eos_filters() {
        let pipeline = single_filter_pipeline("reject_body", RejectBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        // request_body left empty, as when trailers close a body with no bytes.
        let responses = run_request_pipeline(RequestPhase::Body, &pipeline, &mut state, true)
            .await
            .expect("an empty-body reject must produce an immediate response, not an error");
        assert!(
            responses.iter().any(response::is_immediate),
            "an empty buffered body must still trigger the body-filter rejection: {responses:?}"
        );
    }

    /// Response-side mirror: an empty buffered response body must still give
    /// response-body filters their end-of-stream call.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn empty_buffered_response_body_still_runs_eos_filters() {
        let pipeline = single_filter_pipeline("reject_body", RejectBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        // response_body left empty.
        let responses = run_response_pipeline(ResponsePhase::Body, &pipeline, &mut state, true)
            .await
            .expect("an empty-body reject must produce an immediate response, not an error");
        assert!(
            responses.iter().any(response::is_immediate),
            "an empty buffered response body must still trigger the body-filter rejection: {responses:?}"
        );
    }

    /// A headers-only response (headers EOS=true, no body message, e.g. a local
    /// reply) must not run body filters on the never-streamed body: doing so
    /// would let a body filter reject or rewrite a response that has no body,
    /// swallowing the headers-only reply.
    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    async fn headers_only_response_does_not_run_body_filters() {
        let pipeline = single_filter_pipeline("reject_body", RejectBodyFilter::from_config);
        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        // response_body left empty: the response is headers-only.
        let responses = run_response_pipeline(ResponsePhase::Headers, &pipeline, &mut state, true)
            .await
            .expect("a headers-only response must pass through, not error");
        assert!(
            !responses.iter().any(response::is_immediate),
            "a headers-only response must not trigger a body-filter rejection: {responses:?}"
        );
    }

    /// A multi-valued response header no filter touched must not read as a
    /// mutation. Snapshotting through a map that folds duplicate names
    /// (last-one-wins) would drop entries and make an untouched `Set-Cookie`
    /// look changed, forcing a spurious re-emit and route re-evaluation.
    #[test]
    #[expect(clippy::expect_used, reason = "tests")]
    fn capture_preserves_multivalued_headers_no_spurious_mutation() {
        use praxis_filter::FilterRegistry;

        let mut resp = Response {
            status: http::StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.append(
            http::header::SET_COOKIE,
            "session=a".parse().expect("valid header value"),
        );
        resp.headers
            .append(http::header::SET_COOKIE, "theme=b".parse().expect("valid header value"));

        // Snapshot exactly as the response phase does, before any filter runs.
        let original = capture_original_headers(&resp);

        let pipeline = FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).expect("empty pipeline");
        let request = adapter::envoy_headers_to_request(&[]);
        let mut ctx = adapter::build_filter_context(&pipeline, &request, false);
        ctx.response_header = Some(&mut resp);

        let mutation = adapter::collect_response_header_mutations_diff(&ctx, &original);

        assert!(
            mutation.is_none(),
            "an untouched multi-valued Set-Cookie must yield no mutation, but got: {mutation:?}"
        );
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
        run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state, true)
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
        run_response_pipeline(ResponsePhase::Headers, &pipeline, &mut state, true)
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
        let mut context = adapter::build_filter_context(&pipeline, &request, false);
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
        let ctx = adapter::build_filter_context(&pipeline, &request, false);
        let mut hydrated = HydratedContext::hydrate(Some(CarriedContext::default()), ctx).unwrap();
        hydrated.cluster = Some(Arc::from("selected-cluster"));
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
            cluster,
            branch_iterations,
            executed_filter_indices,
            filter_metadata,
            filter_state,
        } = &carried;
        assert_eq!(cluster.as_deref(), Some("selected-cluster"));
        assert_eq!(branch_iterations.get("branch-a"), Some(&3));
        assert_eq!(executed_filter_indices, &vec![true, false, true]);
        assert_eq!(filter_metadata.get("carry.meta").map(String::as_str), Some("kept"));
        assert!(filter_state.contains_key(&7));

        // hydrate must restore every field into a freshly built context,
        // consuming the parked value and emptying the slot.
        let fresh = adapter::build_filter_context(&pipeline, &request, false);
        let hydrated = HydratedContext::hydrate(Some(carried), fresh).unwrap();
        assert_eq!(hydrated.cluster.as_deref(), Some("selected-cluster"));
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
        let fresh = adapter::build_filter_context(&pipeline, &request, false);

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
        let first = adapter::build_filter_context(&pipeline, &request, false);
        let _hydrated = HydratedContext::hydrate(slot.take(), first).unwrap();
        assert!(slot.is_none(), "hydrate's take must leave the slot None");

        // Second hydrate finds None and must error instead of carrying empty state.
        let second = adapter::build_filter_context(&pipeline, &request, false);
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
        let fresh = adapter::build_filter_context(&pipeline, &request, false);
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

    /// Body filter that derives a routing header from the request body,
    /// modelling BBR's `model_to_header`. Declares `ReadOnly` body access so
    /// the pipeline reports `needs_request_body`, and sets the header only in
    /// the body phase -- never at header time.
    struct ModelHeaderProbe;
    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for ModelHeaderProbe {
        fn name(&self) -> &'static str {
            "model_header_probe"
        }

        async fn on_request(
            &self,
            _ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        fn request_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadOnly
        }

        async fn on_request_body(
            &self,
            ctx: &mut HttpFilterContext<'_>,
            body: &mut Option<Bytes>,
            _end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            if body.as_ref().is_some_and(|b| !b.is_empty()) {
                ctx.request_headers_to_set.push((
                    "x-gateway-model-name".parse().unwrap(),
                    "internal-model".parse().unwrap(),
                ));
            }
            Ok(FilterAction::Continue)
        }
    }
    impl ModelHeaderProbe {
        /// Registry factory for `model_header_probe`.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// A body-derived header set in the body phase must NOT be emitted under
    /// `STREAMED`.
    ///
    /// Envoy already forwarded the headers upstream, so a header mutation on a
    /// body message is dropped and re-routing cannot happen; emitting it (with
    /// `clear_route_cache`) would only promise a routing change that never
    /// occurs. The chunk itself is still forwarded, and a once-per-process
    /// warning is the operator's signal to switch to BUFFERED or full-duplex.
    #[tokio::test]
    #[expect(clippy::too_many_lines, reason = "test sets up a pipeline, filter, and assertions")]
    async fn streamed_request_body_derived_header_mutations_are_not_emitted() {
        use praxis_filter::FilterRegistry;
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response as ProtoResponse;

        let cfg: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: model_header_probe\n")
                .unwrap();
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register(
                "model_header_probe",
                praxis_filter::http_builtin(ModelHeaderProbe::from_config),
            )
            .unwrap();
        let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();
        assert!(
            pipeline.body_capabilities().needs_request_body,
            "probe must opt into the request body so STREAMED routes through process_streamed_body_chunk"
        );

        let mut state = StreamState::new();
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        state.protocol_config.request_body_mode = BodyMode::Streamed;

        let payload = br#"{"model":"internal-model"}"#.to_vec();
        let body = praxis_proto::envoy::service::ext_proc::v3::HttpBody {
            body: payload.clone(),
            end_of_stream: true,
        };
        let responses = process_streamed_body_chunk(&pipeline, body, &mut state, true)
            .await
            .unwrap();

        let common = responses
            .iter()
            .find_map(|r| match &r.response {
                Some(ProtoResponse::RequestBody(b)) => b.response.as_ref(),
                _ => None,
            })
            .unwrap();

        assert!(
            common.header_mutation.is_none(),
            "Envoy ignores header mutations on STREAMED body responses, so none must be emitted"
        );
        assert!(
            !common.clear_route_cache,
            "no header mutation is emitted, so the route cache must not be cleared"
        );
        assert!(
            common.body_mutation.is_some(),
            "the body chunk itself must still be forwarded"
        );
    }
}
