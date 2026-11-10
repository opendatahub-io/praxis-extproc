// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Per-phase ExtProc message handlers.
//!
//! Each handler owns one protocol phase (request/response headers and body):
//! it marks end-of-stream, converts Envoy messages into filter inputs, and
//! routes to the [`crate::pipeline`] by body mode and filter capabilities.
//! Buffered bodies are accumulated up to the stream's effective body limit.
//! Selected-upstream body filters require Envoy's BUFFERED request mode so
//! translation finishes before the body response is sent.

use praxis_filter::FilterPipeline;
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{ProcessingResponse, processing_request},
};
use tonic::Status;
use tracing::{debug, trace, warn};

use crate::{
    adapter, metrics,
    pipeline::{
        MutationDelivery, RequestPhase, ResponsePhase, deferred_header_response, direction_label,
        flush_streamed_body_filters, passthrough_chunk, process_streamed_body_chunk,
        run_bodyless_selected_upstream_pipeline, run_request_header_filters_early, run_request_pipeline,
        run_response_header_filters_early, run_response_pipeline,
    },
    protocol::{PhaseState, ProtocolPhase, duplicate_after_eos, request_type_label},
    response::{self, BodyMode},
    server::StreamState,
};

// -----------------------------------------------------------------------------
// Local Replies
// -----------------------------------------------------------------------------

/// Acknowledge a local reply from an earlier filter without running filters.
///
/// There is no request context for the pipeline to act on, so every response
/// phase continues unchanged, under the same EOS rules as any other stream.
///
/// # Errors
///
/// Returns [`Status::invalid_argument`] for a phase the EOS tracker rejects,
/// and [`Status::internal`] for a request message, which
/// [`crate::protocol::PhaseOrderTracker`] rejects before dispatch.
pub(crate) fn local_reply_passthrough(
    req: &processing_request::Request,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    match req {
        processing_request::Request::ResponseHeaders(h) => local_reply_headers(h, state),
        processing_request::Request::ResponseBody(b) => local_reply_body(b, state),
        processing_request::Request::ResponseTrailers(_) => Ok(vec![response::response_trailers()]),
        processing_request::Request::RequestHeaders(_)
        | processing_request::Request::RequestBody(_)
        | processing_request::Request::RequestTrailers(_) => Err(Status::internal(format!(
            "{} dispatched as part of a local reply",
            request_type_label(req)
        ))),
    }
}

/// Acknowledge the headers of a local reply.
///
/// `FULL_DUPLEX_STREAMED` carries the headers response with the first body
/// chunk, as in [`passthrough_chunk`].
fn local_reply_headers(
    headers: &praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if state
        .eos_tracker
        .check_and_mark(ProtocolPhase::ResponseHeaders, headers.end_of_stream)?
        == PhaseState::Completed
    {
        return Err(duplicate_after_eos(ProtocolPhase::ResponseHeaders));
    }
    metrics::record_local_reply(
        headers
            .headers
            .as_ref()
            .and_then(|hm| adapter::response_status(&hm.headers)),
    );
    debug!("stream opened with response headers, passing the local reply through without filters");

    if !headers.end_of_stream && state.protocol_config.response_body_mode == BodyMode::FullDuplexStreamed {
        return Ok(Vec::new());
    }
    state.header_state.response_headers_sent = true;
    Ok(vec![response::response_headers(None)])
}

/// Acknowledge a local reply body chunk, ignoring the final chunk Envoy re-sends
/// in `FULL_DUPLEX_STREAMED` as [`handle_response_body`] does.
fn local_reply_body(
    body: &praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.response_body_mode;
    let entry_state = state
        .eos_tracker
        .check_and_mark(ProtocolPhase::ResponseBody, body.end_of_stream)?;
    if let Some(response) = handle_body_redelivery(entry_state, mode, ProtocolPhase::ResponseBody, body.body.len())? {
        return Ok(response);
    }
    if mode == BodyMode::FullDuplexStreamed {
        return Ok(passthrough_chunk(body, state, mode, false));
    }
    Ok(response::response_body(None, None, mode, body.end_of_stream))
}

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
#[expect(
    clippy::too_many_lines,
    reason = "body-mode and guardrail admission checks stay together"
)]
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
    if let Some(rejection) = reject_ambiguous_guardrail_selection(&envoy_headers) {
        return Ok(vec![rejection]);
    }
    state.request = Some(adapter::envoy_headers_to_request(&envoy_headers));

    if (state.protocol_config.request_body_mode == BodyMode::FullDuplexStreamed
        || state.protocol_config.response_body_mode == BodyMode::FullDuplexStreamed)
        && state.response_mode_selector.has_selected_guardrail(&envoy_headers)
    {
        warn!("selected guardrails cannot be enforced in FULL_DUPLEX_STREAMED mode");
        return Ok(vec![request_processing_rejection(
            b"selected guardrails are not supported in FULL_DUPLEX_STREAMED mode",
        )]);
    }

    let mut responses = if headers.end_of_stream {
        handle_end_stream_request_headers(pipeline, state).await?
    } else {
        if state.protocol_config.request_body_mode == BodyMode::FullDuplexStreamed
            && pipeline.body_capabilities().needs_selected_upstream_request_body
        {
            warn!("selected-upstream request-body hook cannot run in FULL_DUPLEX_STREAMED mode");
            return Ok(vec![request_processing_rejection(
                b"selected-upstream request-body processing is not supported in FULL_DUPLEX_STREAMED mode",
            )]);
        }

        handle_request_headers_with_body(pipeline, state).await?
    };

    apply_response_mode_override(pipeline, &mut responses, state, &envoy_headers);
    Ok(responses)
}

/// Apply the request-specific response mode selected by guardrail configuration.
fn apply_response_mode_override(
    pipeline: &FilterPipeline,
    responses: &mut [ProcessingResponse],
    state: &mut StreamState,
    envoy_headers: &[HeaderValue],
) {
    if state.protocol_config.request_body_mode == BodyMode::FullDuplexStreamed
        || state.protocol_config.response_body_mode == BodyMode::FullDuplexStreamed
        || state.protocol_config.response_body_mode == BodyMode::None
        || state.protocol_config.send_body_without_waiting_for_header_response
    {
        return;
    }

    if matches!(
        pipeline.body_capabilities().response_body_mode,
        praxis_filter::BodyMode::StreamBuffer { .. }
    ) {
        return;
    }

    if state.response_mode_selector.should_stream(envoy_headers) {
        let mut override_applied = false;
        for response in responses {
            override_applied |= response::set_response_mode_streamed(response, state.protocol_config.request_body_mode);
        }
        if override_applied {
            state.protocol_config.response_body_mode = BodyMode::Streamed;
        } else {
            debug!("response mode override skipped because no request-headers response was emitted");
        }
    }
}

/// Run selected-upstream body validation for a request that ended with headers.
async fn handle_end_stream_request_headers(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if pipeline.body_capabilities().needs_selected_upstream_request_body {
        run_bodyless_selected_upstream_pipeline(pipeline, state).await
    } else {
        run_request_pipeline(RequestPhase::Headers, pipeline, state, true).await
    }
}

/// Handle a request header message followed by one or more body messages.
#[expect(
    clippy::too_many_lines,
    reason = "body mode cases and selected-route guard belong together"
)]
async fn handle_request_headers_with_body(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if has_selected_body_filter(pipeline)
        && matches!(
            state.protocol_config.request_body_mode,
            BodyMode::None | BodyMode::Streamed
        )
    {
        return handle_unsupported_selected_request_headers(pipeline, state).await;
    }

    match state.protocol_config.request_body_mode {
        BodyMode::None | BodyMode::Streamed => {
            state.header_state.request_headers_sent = true;
            Box::pin(run_request_header_filters_early(
                pipeline,
                state,
                MutationDelivery::Send,
            ))
            .await
        },
        BodyMode::FullDuplexStreamed if !pipeline.body_capabilities().needs_request_body => {
            Box::pin(run_request_header_filters_early(
                pipeline,
                state,
                MutationDelivery::DeferSilent,
            ))
            .await
        },
        BodyMode::FullDuplexStreamed => Ok(Vec::new()),
        _ => Ok(request_headers_without_body_filter(state)),
    }
}

/// Whether a chain contains a selected-upstream request-body participant.
fn has_selected_body_filter(pipeline: &FilterPipeline) -> bool {
    pipeline.body_capabilities().needs_selected_upstream_request_body
}

/// Acknowledge request headers when body processing is handled later.
fn request_headers_without_body_filter(state: &mut StreamState) -> Vec<ProcessingResponse> {
    state.header_state.request_headers_sent = true;
    vec![response::request_headers(None)]
}

/// Reject unsupported body modes before forwarding, including Envoy-routed requests.
async fn handle_unsupported_selected_request_headers(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let responses = Box::pin(run_request_header_filters_early(
        pipeline,
        state,
        MutationDelivery::DeferSilent,
    ))
    .await?;
    if responses.first().is_some_and(response::is_immediate) {
        return Ok(responses);
    }
    warn!("selected-upstream request-body hook requires BUFFERED mode");
    Ok(vec![request_processing_rejection(
        b"selected-upstream request-body processing requires BUFFERED mode",
    )])
}

/// Handle request body: route by body mode and filter capabilities.
pub(crate) async fn handle_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.request_body_mode;
    state.mark_body_chunk_seen(true);

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
        // STREAMED / FULL_DUPLEX passthrough and streamed-filter paths never
        // fill `state.request_body`, so enforce the accumulation ceiling with a
        // running byte count before forwarding each chunk.
        (BodyMode::Streamed | BodyMode::FullDuplexStreamed, false) => {
            if let Some(rejection) = reject_streamed_request_over_limit(pipeline, state, body.body.len()) {
                return Ok(vec![rejection]);
            }
            Ok(passthrough_chunk(&body, state, mode, true))
        },
        (BodyMode::Streamed, true) => {
            if let Some(rejection) = reject_streamed_request_over_limit(pipeline, state, body.body.len()) {
                return Ok(vec![rejection]);
            }
            process_streamed_body_chunk(pipeline, body, state, true).await
        },
        _ => accumulate_request_body(pipeline, body, state).await,
    }
}

/// Count a streamed request chunk toward `max_body_accumulation` without buffering.
///
/// Returns `Some(ImmediateResponse)` with status 413 when the chunk would exceed the ceiling.
fn reject_streamed_request_over_limit(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    chunk_len: usize,
) -> Option<ProcessingResponse> {
    if check_body_limit(state.streamed_request_bytes, chunk_len, state.max_body_accumulation).is_err() {
        return Some(request_body_limit_exceeded(pipeline));
    }
    state.streamed_request_bytes = state.streamed_request_bytes.saturating_add(chunk_len);
    None
}

/// Refuse ambiguous gateway selection metadata before filters can disagree on it.
fn reject_ambiguous_guardrail_selection(headers: &[HeaderValue]) -> Option<ProcessingResponse> {
    let mut names = std::collections::HashSet::new();
    for header in headers {
        let name = header.key.to_ascii_lowercase();
        if !is_guardrail_selection_header(&name) {
            continue;
        }
        let value = if header.raw_value.is_empty() {
            header.value.as_bytes()
        } else {
            &header.raw_value
        };
        if !names.insert(name) || !matches!(value, b"true" | b"false") {
            let rejection = praxis_filter::Rejection::status(400)
                .with_body(b"guardrail selection headers must be unique boolean values".to_vec());
            return Some(response::immediate(adapter::rejection_to_immediate(&rejection)));
        }
    }
    None
}

/// Recognize phase selectors and the controller's SHA-256 check identifiers.
fn is_guardrail_selection_header(name: &str) -> bool {
    name.strip_prefix("x-aigateway-guardrail-").is_some_and(|suffix| {
        matches!(suffix, "input" | "output")
            || (suffix.len() == 64 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit()))
    })
}

/// Build a fail-closed local response for an unsupported request-processing path.
fn request_processing_rejection(body: &'static [u8]) -> ProcessingResponse {
    let rejection = praxis_filter::Rejection::status(500).with_body(body.to_vec());
    response::immediate(adapter::rejection_to_immediate(&rejection))
}

/// Prefer `ImmediateResponse` 413 over gRPC `RESOURCE_EXHAUSTED` so Envoy surfaces a
/// diagnosable client error instead of an empty HTTP 500 under
/// `failure_mode_allow: false` (follow-up to RHOAIENG-102139).
fn request_body_limit_exceeded(pipeline: &FilterPipeline) -> ProcessingResponse {
    let message = if pipeline.body_capabilities().needs_selected_upstream_request_body {
        b"request body exceeds the selected-upstream limit".as_slice()
    } else {
        b"request body exceeds maximum size"
    };
    let rejection = praxis_filter::Rejection::status(413).with_body(message.to_vec());
    response::immediate(adapter::rejection_to_immediate(&rejection))
}

/// Accumulate request body chunks, run full pipeline on EOS.
async fn accumulate_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if let Err(_error) = check_body_limit(state.request_body.len(), body.body.len(), state.max_body_accumulation) {
        return Ok(vec![request_body_limit_exceeded(pipeline)]);
    }
    state.request_body.extend_from_slice(&body.body);

    if !body.end_of_stream && state.protocol_config.request_body_mode != BodyMode::Buffered {
        return Ok(Vec::new());
    }
    state.buffered_request_body_processed = state.protocol_config.request_body_mode == BodyMode::Buffered;
    run_request_pipeline(RequestPhase::Body, pipeline, state, body.end_of_stream).await
}

// -----------------------------------------------------------------------------
// Response Handlers
// -----------------------------------------------------------------------------

/// Handle response headers: run response filters and respond with mutations.
///
/// For `BUFFERED` with body filters, returns no mutations now and runs the full
/// pipeline at body EOS (Envoy honours `CommonResponse.header_mutation` on the
/// body response).
/// For `BUFFERED` without body filters, runs filters early and defers mutations
/// to the body response.
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
        return run_response_pipeline(ResponsePhase::Headers, pipeline, state, true).await;
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
        _ if pipeline.body_capabilities().needs_response_body => Ok(vec![response::response_headers(None)]),
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
    state.mark_body_chunk_seen(false);

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

    run_response_pipeline(ResponsePhase::Body, pipeline, state, true).await
}

// -----------------------------------------------------------------------------
// Trailers
// -----------------------------------------------------------------------------

/// Handle trailers for one direction: release any held-back body work, then
/// acknowledge with a `TrailersResponse`.
pub(crate) async fn handle_trailers(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mut responses = finalize_body_on_trailers(pipeline, state, is_request).await?;
    // Envoy ignores everything after an immediate response, so skip the ack then.
    if !responses.last().is_some_and(response::is_immediate) {
        responses.push(if is_request {
            response::request_trailers()
        } else {
            response::response_trailers()
        });
    }
    Ok(responses)
}

/// Complete a body phase that trailers, not `end_of_stream`, close.
///
/// Envoy signals the end of a body carrying trailers with the trailers message
/// itself. Work held back for end-of-stream — the accumulated `BUFFERED` body or
/// a `STREAMED` filter's end-of-stream call — must run now or the stream stalls.
async fn finalize_body_on_trailers(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    if !state.body_open(is_request) {
        return Ok(Vec::new());
    }

    let (phase, mode, needs_body) = trailing_body_phase(pipeline, state, is_request);
    state.eos_tracker.mark_complete(phase);

    match mode {
        BodyMode::Streamed if needs_body => Box::pin(flush_streamed_body_filters(pipeline, state, is_request)).await,
        BodyMode::FullDuplexStreamed if needs_body => {
            Box::pin(run_trailing_fds_body(pipeline, state, is_request)).await
        },
        BodyMode::FullDuplexStreamed => Ok(flush_deferred_fds_headers(state, is_request)),
        BodyMode::Buffered => {
            if is_request && state.buffered_request_body_processed {
                return Ok(Vec::new());
            }
            let responses = Box::pin(run_trailing_body_pipeline(pipeline, state, is_request)).await?;
            Ok(rejection_only(responses, is_request))
        },
        _ => Ok(Vec::new()),
    }
}

/// Run the FDS body pipeline when trailers close the body.
///
/// Emits the deferred headers response and the accumulated body as a streamed
/// chunk. A trailer-only body (no chunk arrived and filters produced no bytes)
/// has nothing to stream, so the empty body message is dropped to avoid a
/// spurious chunk and only the headers response is sent.
async fn run_trailing_fds_body(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let chunk_seen = state.body_chunk_seen(is_request);
    let responses = run_trailing_body_pipeline(pipeline, state, is_request).await?;
    let produced_body = if is_request {
        !state.request_body.is_empty()
    } else {
        !state.response_body.is_empty()
    };
    if chunk_seen || produced_body {
        return Ok(responses);
    }
    trace!(
        direction = direction_label(is_request),
        "trailers closed an FDS body with no body message received; emitting headers only"
    );
    Ok(responses.into_iter().filter(|r| !response::is_body(r)).collect())
}

/// Run the body pipeline for one direction when trailers close the body.
///
/// The emitted body chunk carries `end_of_stream=false`: the trailers message,
/// not the chunk, ends the stream.
async fn run_trailing_body_pipeline(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    if is_request {
        run_request_pipeline(RequestPhase::Body, pipeline, state, false).await
    } else {
        run_response_pipeline(ResponsePhase::Body, pipeline, state, false).await
    }
}

/// Flush an FDS passthrough header mutation deferred at header time, if no body
/// chunk has yet carried it out.
fn flush_deferred_fds_headers(state: &mut StreamState, is_request: bool) -> Vec<ProcessingResponse> {
    if state.header_state.take_first_chunk(is_request) {
        vec![deferred_header_response(state, is_request)]
    } else {
        Vec::new()
    }
}

/// Resolve the body phase, configured body mode, and body need for one direction.
fn trailing_body_phase(
    pipeline: &FilterPipeline,
    state: &StreamState,
    is_request: bool,
) -> (ProtocolPhase, BodyMode, bool) {
    let caps = pipeline.body_capabilities();
    if is_request {
        (
            ProtocolPhase::RequestBody,
            state.protocol_config.request_body_mode,
            caps.needs_request_body || caps.needs_selected_upstream_request_body,
        )
    } else {
        (
            ProtocolPhase::ResponseBody,
            state.protocol_config.response_body_mode,
            caps.needs_response_body,
        )
    }
}

/// Keep only an `ImmediateResponse` from a pipeline run that had no body message
/// to answer.
///
/// A BUFFERED body that turns out to be empty never produces a body message, so
/// header and body mutations have nothing to ride on; a rejection still applies.
fn rejection_only(responses: Vec<ProcessingResponse>, is_request: bool) -> Vec<ProcessingResponse> {
    let immediate: Vec<ProcessingResponse> = responses.into_iter().filter(response::is_immediate).collect();
    if immediate.is_empty() {
        warn!(
            direction = direction_label(is_request),
            "trailers closed an empty BUFFERED body; pipeline header mutations have no message to apply to and are dropped"
        );
    }
    immediate
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
#[expect(clippy::indexing_slicing, reason = "tests assert the single response")]
#[expect(clippy::too_many_lines, reason = "tests construct full protocol requests")]
mod tests {
    use praxis_filter::{FilterAction, HttpFilterContext};
    use praxis_proto::envoy::service::ext_proc::v3::{
        BodySendMode as ExtProcBodySendMode, HeaderMap as ExtProcHeaderMap, HttpHeaders as ExtProcHttpHeaders,
    };

    use super::*;
    use crate::test_support::counter_value;

    #[tokio::test]
    async fn request_headers_stream_response_when_no_output_guardrail_is_selected() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);
        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-input".to_owned(),
                        value: "true".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0].mode_override.as_ref().map(|mode| mode.response_body_mode),
            Some(ExtProcBodySendMode::Streamed.into())
        );
        assert_eq!(
            responses[0].mode_override.as_ref().map(|mode| mode.request_body_mode),
            Some(ExtProcBodySendMode::Buffered.into())
        );
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::Streamed);
    }

    #[tokio::test]
    async fn request_headers_keep_buffered_response_when_output_guardrail_is_selected() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);
        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-output".to_owned(),
                        value: "true".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(responses.len(), 1);
        assert!(responses[0].mode_override.is_none());
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::Buffered);
    }

    #[test]
    fn response_mode_override_does_not_change_state_without_a_response() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-output: "true" }
        phase: { request: false, response: true }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::Buffered;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        let mut responses = Vec::new();
        apply_response_mode_override(
            &pipeline,
            &mut responses,
            &mut state,
            &[HeaderValue {
                key: "x-aigateway-guardrail-input".to_owned(),
                value: "true".to_owned(),
                raw_value: Vec::new(),
            }],
        );

        assert!(responses.is_empty());
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::Buffered);
    }

    #[test]
    fn response_mode_override_preserves_none_response_mode() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(concat!(
            "filter_chains:\n",
            "  - name: ipp\n",
            "    filters:\n",
            "      - filter: ai_guardrails\n",
            "        phase: { request: true, response: false }\n",
        ))
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::None;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);
        let mut responses = vec![response::request_headers(None)];

        apply_response_mode_override(&pipeline, &mut responses, &mut state, &[]);

        assert!(responses[0].mode_override.is_none());
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::None);
    }

    #[test]
    fn response_mode_override_preserves_buffered_mode_when_envoy_does_not_wait() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(concat!(
            "filter_chains:\n",
            "  - name: ipp\n",
            "    filters:\n",
            "      - filter: ai_guardrails\n",
            "        phase: { request: true, response: false }\n",
        ))
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.send_body_without_waiting_for_header_response = true;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);
        let mut responses = vec![response::request_headers(None)];

        apply_response_mode_override(&pipeline, &mut responses, &mut state, &[]);

        assert!(responses[0].mode_override.is_none());
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::Buffered);
    }

    #[tokio::test]
    async fn request_headers_keep_buffered_response_for_other_response_body_filters() {
        let pipeline_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            "
filter_chains:
  - name: main
    filters:
      - filter: json_body
        response_remove:
          - /internal
",
        )
        .unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        assert!(matches!(
            pipeline.body_capabilities().response_body_mode,
            praxis_filter::BodyMode::StreamBuffer { .. }
        ));
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase: { request: true, response: false }
",
        )
        .unwrap();
        let mut state = StreamState::new();
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);
        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-input".to_owned(),
                        value: "true".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();

        assert_eq!(responses.len(), 1);
        assert!(responses[0].mode_override.is_none());
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::Buffered);
    }

    #[expect(
        clippy::too_many_lines,
        reason = "regression test exercises guardrail mode selection and buffered response mutation"
    )]
    #[tokio::test]
    async fn buffered_response_filter_removes_json_field_for_input_only_guardrail() {
        let pipeline_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            "
filter_chains:
  - name: main
    filters:
      - filter: json_body
        response_remove:
          - /internal
",
        )
        .unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase: { request: true, response: false }
",
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::Buffered;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        let request_responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![
                        HeaderValue {
                            key: ":method".to_owned(),
                            value: "POST".to_owned(),
                            raw_value: Vec::new(),
                        },
                        HeaderValue {
                            key: ":path".to_owned(),
                            value: "/v1/chat/completions".to_owned(),
                            raw_value: Vec::new(),
                        },
                        HeaderValue {
                            key: "x-aigateway-guardrail-input".to_owned(),
                            value: "true".to_owned(),
                            raw_value: Vec::new(),
                        },
                    ],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(request_responses.len(), 1);
        assert!(request_responses[0].mode_override.is_none());
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::Buffered);

        let response_headers = handle_response_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![
                        HeaderValue {
                            key: ":status".to_owned(),
                            value: "200".to_owned(),
                            raw_value: Vec::new(),
                        },
                        HeaderValue {
                            key: "content-type".to_owned(),
                            value: "application/json".to_owned(),
                            raw_value: Vec::new(),
                        },
                        HeaderValue {
                            key: "content-length".to_owned(),
                            value: "35".to_owned(),
                            raw_value: Vec::new(),
                        },
                    ],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();
        assert_eq!(response_headers.len(), 1);
        assert!(!state.header_state.response_filters_executed);

        let upstream_body = br#"{"internal":"remove","safe":"keep"}"#;
        let body_responses = handle_response_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: upstream_body.to_vec(),
                end_of_stream: true,
            },
            &mut state,
        )
        .await
        .unwrap();

        let body = body_responses
            .iter()
            .find_map(|response| match response.response.as_ref() {
                Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ResponseBody(body)) => {
                    body.response
                        .as_ref()
                        .and_then(|common| common.body_mutation.as_ref())
                        .and_then(|mutation| match mutation.mutation.as_ref() {
                            Some(praxis_proto::envoy::service::ext_proc::v3::body_mutation::Mutation::Body(body)) => {
                                Some(body.clone())
                            },
                            _ => None,
                        })
                },
                _ => None,
            });
        assert!(body.is_some(), "buffered response filter must emit a body mutation");
        let body = body.unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json, serde_json::json!({"safe": "keep"}));
        assert_eq!(body.len(), upstream_body.len());

        let content_length = body_responses
            .iter()
            .find_map(|response| match response.response.as_ref() {
                Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ResponseBody(body)) => {
                    body.response
                        .as_ref()
                        .and_then(|common| common.header_mutation.as_ref())
                        .and_then(|mutation| {
                            mutation.set_headers.iter().find_map(|header| {
                                let header = header.header.as_ref()?;
                                header
                                    .key
                                    .eq_ignore_ascii_case("content-length")
                                    .then(|| header.value.clone())
                            })
                        })
                },
                _ => None,
            });
        assert!(
            content_length.is_none(),
            "json_body response_remove must preserve the committed content length"
        );
    }

    #[tokio::test]
    async fn selected_guardrail_is_rejected_before_body_processing_in_fds() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
        phase: { request: true, response: false }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::FullDuplexStreamed;
        state.protocol_config.response_body_mode = BodyMode::FullDuplexStreamed;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-input".to_owned(),
                        value: "true".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();

        assert!(responses.first().is_some_and(response::is_immediate));
    }

    async fn selected_guardrail_response_for_modes(
        request_body_mode: BodyMode,
        response_body_mode: BodyMode,
    ) -> Vec<ProcessingResponse> {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
        phase: { request: true, response: false }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = request_body_mode;
        state.protocol_config.response_body_mode = response_body_mode;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-input".to_owned(),
                        value: "true".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn selected_guardrail_is_rejected_when_either_body_mode_is_fds() {
        for (request_body_mode, response_body_mode) in [
            (BodyMode::FullDuplexStreamed, BodyMode::Buffered),
            (BodyMode::Buffered, BodyMode::FullDuplexStreamed),
        ] {
            let responses = selected_guardrail_response_for_modes(request_body_mode, response_body_mode).await;
            assert!(responses.first().is_some_and(response::is_immediate));
        }
    }

    #[tokio::test]
    async fn unconditional_guardrail_is_rejected_before_body_processing_in_fds() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            "
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        phase:
          request: true
          response: false
",
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::FullDuplexStreamed;
        state.protocol_config.response_body_mode = BodyMode::FullDuplexStreamed;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap { headers: Vec::new() }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();

        assert!(responses.first().is_some_and(response::is_immediate));
    }

    #[tokio::test]
    async fn unselected_guardrail_is_allowed_in_fds() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
        phase: { request: true, response: false }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::FullDuplexStreamed;
        state.protocol_config.response_body_mode = BodyMode::FullDuplexStreamed;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-input".to_owned(),
                        value: "false".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();

        assert!(!responses.iter().any(response::is_immediate));
        assert!(responses.iter().all(|response| response.mode_override.is_none()));
        assert_eq!(state.protocol_config.request_body_mode, BodyMode::FullDuplexStreamed);
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::FullDuplexStreamed);
    }

    #[tokio::test]
    async fn unselected_guardrail_is_allowed_in_fds_with_end_of_stream() {
        let pipeline_config: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n").unwrap();
        let pipeline =
            crate::config::build_pipeline(&pipeline_config, &praxis_filter::FilterRegistry::with_builtins()).unwrap();
        let selector_config: crate::config::ExtProcConfig = serde_yaml::from_str(
            r#"
filter_chains:
  - name: ipp
    filters:
      - filter: ai_guardrails
        conditions:
          - when:
              headers: { x-aigateway-guardrail-input: "true" }
        phase: { request: true, response: false }
"#,
        )
        .unwrap();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::FullDuplexStreamed;
        state.protocol_config.response_body_mode = BodyMode::FullDuplexStreamed;
        state.response_mode_selector =
            crate::response_mode::ResponseModeSelector::from_config(&selector_config.filter_chains);

        let responses = handle_request_headers(
            &pipeline,
            ExtProcHttpHeaders {
                headers: Some(ExtProcHeaderMap {
                    headers: vec![HeaderValue {
                        key: "x-aigateway-guardrail-input".to_owned(),
                        value: "false".to_owned(),
                        raw_value: Vec::new(),
                    }],
                }),
                end_of_stream: true,
            },
            &mut state,
        )
        .await
        .unwrap();

        assert!(!responses.iter().any(response::is_immediate));
        assert!(responses.iter().all(|response| response.mode_override.is_none()));
        assert_eq!(state.protocol_config.request_body_mode, BodyMode::FullDuplexStreamed);
        assert_eq!(state.protocol_config.response_body_mode, BodyMode::FullDuplexStreamed);
    }

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

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(
        clippy::too_many_lines,
        reason = "the test checks both sides of the request accumulation limit"
    )]
    async fn request_body_accumulation_accepts_boundary_and_rejects_before_extend() {
        let pipeline = FilterPipeline::build(&mut [], &praxis_filter::FilterRegistry::with_builtins())
            .expect("empty test pipeline");
        let mut state = StreamState::new();
        state.max_body_accumulation = Some(4);
        state.protocol_config.request_body_mode = BodyMode::Streamed;

        let exact = accumulate_request_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"abcd".to_vec(),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .expect("a body exactly at the configured limit must be accepted");
        assert!(exact.is_empty(), "an incomplete body waits for its final chunk");
        assert_eq!(state.request_body, b"abcd");

        let rejected = accumulate_request_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"e".to_vec(),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .expect("over-limit bodies return ImmediateResponse 413, not a gRPC error");
        assert!(
            rejected.iter().any(|response| {
                matches!(
                    &response.response,
                    Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ImmediateResponse(
                        immediate
                    )) if immediate.status.as_ref().map(|status| status.code) == Some(413)
                )
            }),
            "over-limit request body must be an ImmediateResponse 413, got: {rejected:?}"
        );
        assert_eq!(
            state.request_body, b"abcd",
            "the rejected chunk must not extend the buffer"
        );
    }

    #[tokio::test]
    #[expect(clippy::expect_used, reason = "tests")]
    #[expect(
        clippy::too_many_lines,
        reason = "the test checks both sides of the streamed passthrough limit"
    )]
    async fn streamed_passthrough_enforces_max_body_bytes() {
        let pipeline = FilterPipeline::build(&mut [], &praxis_filter::FilterRegistry::with_builtins())
            .expect("empty test pipeline");
        let mut state = StreamState::new();
        state.max_body_accumulation = Some(4);
        state.protocol_config.request_body_mode = BodyMode::Streamed;

        let accepted = handle_request_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"abcd".to_vec(),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .expect("body at the limit must be forwarded");
        assert!(
            !accepted.iter().any(response::is_immediate),
            "body at the limit must not be rejected"
        );
        assert_eq!(state.streamed_request_bytes, 4);
        assert!(
            state.request_body.is_empty(),
            "streamed passthrough must not buffer the body"
        );

        let rejected = handle_request_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"e".to_vec(),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .expect("over-limit streamed bodies return ImmediateResponse 413");
        assert!(
            rejected.iter().any(|response| {
                matches!(
                    &response.response,
                    Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ImmediateResponse(
                        immediate
                    )) if immediate.status.as_ref().map(|status| status.code) == Some(413)
                )
            }),
            "over-limit streamed request must be ImmediateResponse 413, got: {rejected:?}"
        );
        assert_eq!(
            state.streamed_request_bytes, 4,
            "the rejected chunk must not advance the streamed byte count"
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

    /// Response filter that upgrades the response body mode dynamically in
    /// `on_response` (as `ai_guardrails` does) and only mutates the body in
    /// `on_response_body` when it observes that upgrade. If the two hooks run on
    /// different contexts, the upgrade is lost and the mutation never fires.
    struct ResponseBodyBufferProbe;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for ResponseBodyBufferProbe {
        fn name(&self) -> &'static str {
            "response_body_buffer_probe"
        }

        async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            ctx.set_response_body_mode(praxis_filter::BodyMode::StreamBuffer { max_bytes: Some(1024) });
            Ok(FilterAction::Continue)
        }

        fn response_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadWrite
        }

        fn on_response_body(
            &self,
            ctx: &mut HttpFilterContext<'_>,
            body: &mut Option<bytes::Bytes>,
            end_of_stream: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            if end_of_stream && matches!(ctx.response_body_mode, praxis_filter::BodyMode::StreamBuffer { .. }) {
                *body = Some(bytes::Bytes::from_static(b"REPLACED"));
            }
            Ok(FilterAction::Continue)
        }
    }

    impl ResponseBodyBufferProbe {
        /// Registry factory for the buffered response-body regression test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// A response filter that reads the response body (so the pipeline reports
    /// `needs_response_body`) and sets a header in `on_response`.
    struct ResponseBodyProbeFilter;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for ResponseBodyProbeFilter {
        fn name(&self) -> &'static str {
            "response_body_probe"
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

        fn response_body_access(&self) -> praxis_filter::BodyAccess {
            praxis_filter::BodyAccess::ReadOnly
        }

        fn on_response_body(
            &self,
            _: &mut HttpFilterContext<'_>,
            _: &mut Option<bytes::Bytes>,
            _: bool,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            Ok(FilterAction::Continue)
        }
    }

    impl ResponseBodyProbeFilter {
        /// Registry factory for the FDS response-trailers test.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    /// Regression: under BUFFERED, a response filter that sets its body mode in
    /// `on_response` must still have `on_response_body` observe it. The response
    /// pipeline must therefore defer to body EOS (like the request side) rather than
    /// run `on_response` early on a throwaway header-time context.
    #[expect(
        clippy::too_many_lines,
        reason = "regression test exercises the full buffered response handoff"
    )]
    #[tokio::test]
    async fn buffered_response_body_filter_runs_at_body_eos_with_dynamic_mode() {
        let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: response_body_buffer_probe\n",
        )
        .unwrap();
        let mut registry = praxis_filter::FilterRegistry::with_builtins();
        registry
            .register(
                "response_body_buffer_probe",
                praxis_filter::http_builtin(ResponseBodyBufferProbe::from_config),
            )
            .unwrap();
        let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();

        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::Buffered;
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // Response headers (not EOS): the pipeline must be deferred, not run early.
        handle_response_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .unwrap();
        assert!(
            !state.header_state.response_filters_executed,
            "a body-dependent buffered response pipeline must defer to body EOS, not run filters at header time"
        );

        // Buffered body (EOS): on_response upgrades the mode and on_response_body,
        // sharing the same context, replaces the body.
        let body_responses = handle_response_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"original-upstream-body".to_vec(),
                end_of_stream: true,
            },
            &mut state,
        )
        .await
        .unwrap();

        let replaced = body_responses.iter().any(|r| {
            matches!(
                r.response.as_ref(),
                Some(praxis_proto::envoy::service::ext_proc::v3::processing_response::Response::ResponseBody(b))
                    if b.response.as_ref().and_then(|c| c.body_mutation.as_ref()).is_some_and(|m| matches!(
                        &m.mutation,
                        Some(praxis_proto::envoy::service::ext_proc::v3::body_mutation::Mutation::Body(bytes))
                            if bytes.as_slice() == b"REPLACED"
                    ))
            )
        });
        assert!(
            replaced,
            "buffered response body filter must apply its mutation; the StreamBuffer upgrade was dropped: {body_responses:?}"
        );
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

    #[expect(clippy::too_many_lines, reason = "end-to-end response-side trailers regression test")]
    #[tokio::test]
    async fn fds_response_body_closed_by_trailers_streams_body_and_acks() {
        use praxis_proto::envoy::service::ext_proc::v3::{body_mutation, processing_response::Response as R};

        let cfg: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: response_body_probe\n")
                .unwrap();
        let mut registry = praxis_filter::FilterRegistry::with_builtins();
        registry
            .register(
                "response_body_probe",
                praxis_filter::http_builtin(ResponseBodyProbeFilter::from_config),
            )
            .unwrap();
        let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();

        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::FullDuplexStreamed;
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // FDS response headers (EOS=false) with a body-reading filter: everything is
        // deferred to the body phase, so nothing goes out yet.
        let deferred = handle_response_headers(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
            &mut state,
        )
        .await
        .unwrap();
        assert!(
            deferred.is_empty(),
            "FDS response headers with a body filter must defer"
        );

        // A body chunk arrives but is not end_of_stream; it accumulates silently.
        let chunk = handle_response_body(
            &pipeline,
            praxis_proto::envoy::service::ext_proc::v3::HttpBody {
                body: b"hi".to_vec(),
                end_of_stream: false,
            },
            &mut state,
        )
        .await
        .unwrap();
        assert!(
            chunk.is_empty(),
            "a non-final FDS body chunk accumulates without a response"
        );

        // Trailers close the body: flush headers + streamed body (eos=false) + ack.
        let responses = handle_trailers(&pipeline, &mut state, false).await.unwrap();

        assert!(
            responses
                .iter()
                .any(|r| matches!(&r.response, Some(R::ResponseHeaders(h))
                if h.response.as_ref().is_some_and(|c| c.header_mutation.as_ref().is_some_and(|m| m
                    .set_headers
                    .iter()
                    .any(|sh| sh.header.as_ref().is_some_and(|hv| hv.key == "x-response-probe")))))),
            "the deferred response headers response with the filter mutation must be flushed: {responses:?}"
        );
        let streamed: Vec<_> = responses
            .iter()
            .filter_map(|r| match &r.response {
                Some(R::ResponseBody(b)) => b
                    .response
                    .as_ref()
                    .and_then(|c| c.body_mutation.as_ref())
                    .and_then(|m| m.mutation.as_ref()),
                _ => None,
            })
            .filter_map(|m| match m {
                body_mutation::Mutation::StreamedResponse(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            streamed.len(),
            1,
            "exactly one streamed body chunk expected: {responses:?}"
        );
        assert!(
            streamed.iter().all(|s| s.body == b"hi" && !s.end_of_stream),
            "the accumulated body must stream once with end_of_stream=false: {responses:?}"
        );
        assert!(
            responses
                .iter()
                .any(|r| matches!(&r.response, Some(R::ResponseTrailers(_)))),
            "the trailers must still be acknowledged: {responses:?}"
        );
    }

    #[test]
    fn local_reply_records_one_metric_per_stream_by_status_class() {
        for (status, class) in [(Some("401"), "4xx"), (Some("200"), "2xx"), (None, "other")] {
            let count = counter_value("praxis_extproc_local_replies_total", &[("status_class", class)], || {
                let mut state = StreamState::new();
                state.protocol_config.response_body_mode = BodyMode::Buffered;
                for message in [local_reply_headers(status), local_reply_body()] {
                    state.phase_order.check_and_advance(&message).unwrap();
                    local_reply_passthrough(&message, &mut state).unwrap();
                }
            });

            assert_eq!(count, 1, "a {status:?} local reply must be counted once as {class}");
        }
    }
    #[test]
    fn local_reply_passthrough_rejects_request_messages() {
        let request = processing_request::Request::RequestHeaders(
            praxis_proto::envoy::service::ext_proc::v3::HttpHeaders::default(),
        );
        let result = local_reply_passthrough(&request, &mut StreamState::new());

        assert!(
            matches!(&result, Err(e) if e.code() == tonic::Code::Internal),
            "a request message must never be acknowledged as a local reply: {result:?}"
        );
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    fn local_reply_headers(status: Option<&str>) -> processing_request::Request {
        use praxis_proto::envoy::service::ext_proc::v3::{HeaderMap, HttpHeaders};

        let headers = status
            .map(|value| HeaderValue {
                key: ":status".to_owned(),
                value: value.to_owned(),
                raw_value: Vec::new(),
            })
            .into_iter()
            .collect();
        processing_request::Request::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap { headers }),
            end_of_stream: false,
        })
    }

    fn local_reply_body() -> processing_request::Request {
        processing_request::Request::ResponseBody(praxis_proto::envoy::service::ext_proc::v3::HttpBody {
            body: b"denied".to_vec(),
            end_of_stream: true,
        })
    }
}
