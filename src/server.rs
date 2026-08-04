// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! gRPC [`ExternalProcessor`] implementation for Praxis filter pipelines.
//!
//! Receives Envoy ExtProc messages, translates them into Praxis filter
//! pipeline invocations, and returns header/body mutations or immediate
//! responses.
//!
//! [`ExternalProcessor`]: praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessor

use std::{collections::HashMap, mem, pin::Pin, sync::Arc, time::Instant};

use bytes::Bytes;
use praxis_filter::{FilterAction, FilterPipeline, HttpFilterContext, Request, Response};
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        ProcessingRequest, ProcessingResponse, external_processor_server::ExternalProcessor, processing_request,
    },
};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request as TonicRequest, Response as TonicResponse, Status, Streaming};
use tracing::{debug, error, warn};

use crate::{adapter, metrics, response};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Maximum accumulated body size before rejecting.
const MAX_BODY_ACCUMULATION: usize = 10_485_760; // 10 MiB

/// Channel buffer size for the response stream.
const RESPONSE_CHANNEL_SIZE: usize = 16;

// -----------------------------------------------------------------------------
// Types
// -----------------------------------------------------------------------------

/// Output stream type for the `Process` RPC.
type ProcessStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ProcessingResponse, Status>> + Send>>;

// -----------------------------------------------------------------------------
// PraxisExtProc
// -----------------------------------------------------------------------------

/// Praxis ExtProc gRPC service.
///
/// Holds a shared [`FilterPipeline`] and executes it for each
/// incoming gRPC stream.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
pub struct PraxisExtProc {
    /// Shared filter pipeline.
    pipeline: Arc<FilterPipeline>,
}

impl PraxisExtProc {
    /// Create a new ExtProc service backed by the given pipeline.
    pub fn new(pipeline: Arc<FilterPipeline>) -> Self {
        Self { pipeline }
    }
}

#[tonic::async_trait]
impl ExternalProcessor for PraxisExtProc {
    type ProcessStream = ProcessStream;

    /// Handle a bidirectional ExtProc stream from Envoy.
    ///
    /// # Errors
    ///
    /// Returns [`Status`] on stream or pipeline errors.
    async fn process(
        &self,
        request: TonicRequest<Streaming<ProcessingRequest>>,
    ) -> Result<TonicResponse<Self::ProcessStream>, Status> {
        let pipeline = Arc::clone(&self.pipeline);
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(RESPONSE_CHANNEL_SIZE);

        tokio::spawn(async move {
            if let Err(e) = handle_stream(&pipeline, &mut inbound, &tx).await {
                error!(error = %e, "stream processing failed");
                drop(tx.send(Err(e)).await);
            }
        });

        let stream = ReceiverStream::new(rx);
        let out: Self::ProcessStream = Box::pin(stream);
        Ok(TonicResponse::new(out))
    }
}

// -----------------------------------------------------------------------------
// Stream Handler
// -----------------------------------------------------------------------------

/// Process all messages on a single ExtProc stream.
///
/// Accumulates request/response body chunks and runs the Praxis filter
/// pipeline at the appropriate phase boundaries.
async fn handle_stream(
    pipeline: &FilterPipeline,
    inbound: &mut Streaming<ProcessingRequest>,
    tx: &mpsc::Sender<Result<ProcessingResponse, Status>>,
) -> Result<(), Status> {
    let start = Instant::now();
    let mut stream_state = StreamState::new();

    let result = process_messages(pipeline, inbound, tx, &mut stream_state).await;

    metrics::record_request(start.elapsed().as_secs_f64());

    result
}

/// Receive and process all messages on the stream.
#[expect(
    clippy::cognitive_complexity,
    reason = "stream loop is intentionally flat; splitting obscures channel lifecycle"
)]
async fn process_messages(
    pipeline: &FilterPipeline,
    inbound: &mut Streaming<ProcessingRequest>,
    tx: &mpsc::Sender<Result<ProcessingResponse, Status>>,
    stream_state: &mut StreamState,
) -> Result<(), Status> {
    while let Some(result) = inbound.next().await {
        let msg = result.map_err(|e| Status::internal(e.to_string()))?;

        let Some(req) = msg.request else {
            warn!("received ProcessingRequest with no request field");
            continue;
        };

        let req_type = request_type_label(&req);
        debug!(phase = req_type, "received ProcessingRequest");

        let responses = dispatch_request(pipeline, req, stream_state).await?;
        debug!(phase = req_type, count = responses.len(), "sending responses");

        for resp in responses {
            if tx.send(Ok(resp)).await.is_err() {
                debug!("response channel closed, ending stream");
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Dispatch a single ExtProc request variant to the appropriate handler.
#[expect(
    clippy::large_stack_frames,
    reason = "async match over ProcessingRequest variants exceeds stack threshold"
)]
async fn dispatch_request(
    pipeline: &FilterPipeline,
    req: processing_request::Request,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    match req {
        processing_request::Request::RequestHeaders(h) => handle_request_headers(pipeline, h, state).await,
        processing_request::Request::RequestBody(b) => handle_request_body(pipeline, b, state).await,
        processing_request::Request::ResponseHeaders(h) => handle_response_headers(pipeline, h, state).await,
        processing_request::Request::ResponseBody(b) => handle_response_body(pipeline, b, state).await,
        processing_request::Request::RequestTrailers(_) => Ok(vec![response::request_trailers()]),
        processing_request::Request::ResponseTrailers(_) => Ok(vec![response::response_trailers()]),
    }
}

// -----------------------------------------------------------------------------
// EOS Tracking
// -----------------------------------------------------------------------------

/// Protocol phase identifier for EOS tracking.
#[derive(Debug, Copy, Clone)]
enum ProtocolPhase {
    /// Request headers phase.
    RequestHeaders,
    /// Request body phase.
    RequestBody,
    /// Response headers phase.
    ResponseHeaders,
    /// Response body phase.
    ResponseBody,
}

/// EOS marker state for a single phase.
#[derive(Debug, Default, Copy, Clone)]
enum EosMarker {
    /// No EOS received yet.
    #[default]
    NotReceived,
    /// EOS has been received.
    Received,
}

impl EosMarker {
    /// Check if EOS was already received.
    const fn is_received(self) -> bool {
        matches!(self, Self::Received)
    }

    /// Mark as received.
    fn mark_received(&mut self) {
        *self = Self::Received;
    }
}

/// Tracks end-of-stream status for each protocol phase.
#[derive(Debug, Default)]
struct EosTracker {
    /// Request headers EOS marker.
    request_headers: EosMarker,
    /// Request body EOS marker.
    request_body: EosMarker,
    /// Response headers EOS marker.
    response_headers: EosMarker,
    /// Response body EOS marker.
    response_body: EosMarker,
}

impl EosTracker {
    /// Validate and mark end-of-stream for a protocol phase.
    ///
    /// # Errors
    ///
    /// Returns [`Status::invalid_argument`] if duplicate EOS is detected.
    fn check_and_mark(&mut self, phase: ProtocolPhase, received_eos: bool) -> Result<(), Status> {
        let marker = match phase {
            ProtocolPhase::RequestHeaders => &mut self.request_headers,
            ProtocolPhase::RequestBody => &mut self.request_body,
            ProtocolPhase::ResponseHeaders => &mut self.response_headers,
            ProtocolPhase::ResponseBody => &mut self.response_body,
        };

        if received_eos && marker.is_received() {
            return Err(Status::invalid_argument(format!(
                "duplicate end_of_stream in {phase:?} phase"
            )));
        }
        if received_eos {
            marker.mark_received();
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// Phase Handlers
// -----------------------------------------------------------------------------

/// Handle request headers: parse into [`Request`] and respond immediately.
///
/// When body is expected (`end_of_stream=false`), the pipeline runs
/// later when the body arrives. We still respond to headers now
/// because Envoy waits for a headers response before sending body.
///
/// [`Request`]: praxis_filter::Request
async fn handle_request_headers(
    pipeline: &FilterPipeline,
    headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    state
        .eos_tracker
        .check_and_mark(ProtocolPhase::RequestHeaders, headers.end_of_stream)?;

    let envoy_headers = extract_header_list(&headers);
    state.request = Some(adapter::envoy_headers_to_request(&envoy_headers));

    if headers.end_of_stream {
        return run_request_pipeline(pipeline, state).await;
    }

    Ok(vec![response::request_headers(None)])
}

/// Handle request body: accumulate chunks, run pipeline on EOS.
async fn handle_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    state
        .eos_tracker
        .check_and_mark(ProtocolPhase::RequestBody, body.end_of_stream)?;

    check_body_limit(state.request_body.len(), body.body.len())?;
    state.request_body.extend_from_slice(&body.body);

    if !body.end_of_stream {
        return Ok(Vec::new());
    }

    run_request_pipeline(pipeline, state).await
}

/// Handle response headers: run response filters and respond with mutations.
///
/// Response header mutations must be sent in this phase because Envoy
/// sends headers to the client after receiving our reply. Body-phase
/// mutations on headers are too late.
async fn handle_response_headers(
    pipeline: &FilterPipeline,
    headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    state
        .eos_tracker
        .check_and_mark(ProtocolPhase::ResponseHeaders, headers.end_of_stream)?;

    let envoy_headers = extract_header_list(&headers);
    state.response = Some(adapter::envoy_headers_to_response(&envoy_headers));

    if headers.end_of_stream {
        return run_response_pipeline(pipeline, state).await;
    }

    let mutation = run_response_header_filters(pipeline, state).await?;
    Ok(vec![response::response_headers(mutation)])
}

/// Handle response body: accumulate chunks, run pipeline on EOS.
async fn handle_response_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    state
        .eos_tracker
        .check_and_mark(ProtocolPhase::ResponseBody, body.end_of_stream)?;

    check_body_limit(state.response_body.len(), body.body.len())?;
    state.response_body.extend_from_slice(&body.body);

    if !body.end_of_stream {
        return Ok(Vec::new());
    }

    run_response_pipeline(pipeline, state).await
}

// -----------------------------------------------------------------------------
// Pipeline Execution
// -----------------------------------------------------------------------------

/// Run request filters and optional body filters, collecting mutations.
async fn run_request_pipeline(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if state.request.is_none() {
        return Err(Status::internal("no request headers"));
    }

    let responses = run_request_filters(pipeline, state).await?;

    Ok(responses)
}

/// Execute request-phase filters and body filters, then collect mutations.
async fn run_request_filters(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Err(Status::internal("no request headers"));
    };
    let mut ctx = adapter::build_filter_context(pipeline, request);

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = check_reject(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    let body_reject = run_body_filters(pipeline, &mut ctx, &mut state.request_body).await?;
    if let Some(imm) = body_reject {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_request_header_mutations(&ctx);
    let body_data = body_data_if_present(&state.request_body);

    let responses = if body_data.is_some() {
        response::request_body(body_data, mutation)
    } else {
        vec![response::request_headers(mutation)]
    };

    state.executed_filter_indices = mem::take(&mut ctx.executed_filter_indices);
    state.branch_iterations = mem::take(&mut ctx.branch_iterations);
    state.filter_metadata = mem::take(&mut ctx.filter_metadata);

    Ok(responses)
}

/// Run response filters and optional body filters, collecting mutations.
async fn run_response_pipeline(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if state.request.is_none() {
        return Err(Status::internal("no request headers"));
    }

    let mut resp = state
        .response
        .take()
        .ok_or_else(|| Status::internal("no response headers"))?;

    let responses = run_response_filters(pipeline, state, &mut resp).await?;

    Ok(responses)
}

/// Execute response-phase filters and body filters, then collect mutations.
///
/// Skips response filter re-execution when headers were already
/// processed by [`run_response_header_filters`]; only body filters
/// run in that case.
async fn run_response_filters(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    resp: &mut Response,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Err(Status::internal("no request headers"));
    };
    let mut ctx = adapter::build_filter_context(pipeline, request);

    state.restore_request_ctx(&mut ctx);
    let original_headers = capture_original_headers(resp);
    ctx.response_header = Some(resp);

    if !state.response_filters_executed {
        let action = execute_response(pipeline, &mut ctx).await?;
        if let Some(imm) = check_reject(action) {
            return Ok(vec![response::immediate(imm)]);
        }
    }

    let body_reject = run_resp_body_filters(pipeline, &mut ctx, &mut state.response_body)?;
    if let Some(imm) = body_reject {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_response_header_mutations_diff(&ctx, &original_headers);
    let body_data = body_data_if_present(&state.response_body);

    if body_data.is_some() {
        Ok(response::response_body(body_data, mutation))
    } else {
        Ok(vec![response::response_headers(mutation)])
    }
}

/// Run response filters at header time and return header mutations.
///
/// This executes the response pipeline early so mutations can be
/// included in the `ResponseHeaders` reply. Body processing runs
/// separately when the body arrives.
async fn run_response_header_filters(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Ok(None);
    };
    let mut ctx = adapter::build_filter_context(pipeline, request);
    state.restore_request_ctx(&mut ctx);

    let Some(resp) = state.response.as_mut() else {
        return Ok(None);
    };

    let original_headers = capture_original_headers(resp);
    ctx.response_header = Some(resp);

    let action = execute_response(pipeline, &mut ctx).await?;
    if let Some(imm) = check_reject(action) {
        return Err(Status::aborted(imm.body));
    }

    state.response_filters_executed = true;

    Ok(adapter::collect_response_header_mutations_diff(&ctx, &original_headers))
}

/// Capture response header names and values before filter execution.
fn capture_original_headers(resp: &Response) -> HashMap<String, String> {
    resp.headers
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
        .collect()
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

/// Convert a [`FilterAction::Reject`] into an `ImmediateResponse`.
fn check_reject(action: FilterAction) -> Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse> {
    if let FilterAction::Reject(rejection) = action {
        metrics::record_immediate_response();
        Some(adapter::rejection_to_immediate(&rejection))
    } else {
        None
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
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if body_buf.is_empty() {
        return Ok(None);
    }

    let mut body = Some(Bytes::from(mem::take(body_buf)));
    let action = pipeline
        .execute_http_request_body(ctx, &mut body, true)
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
) -> Result<Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse>, Status> {
    if body_buf.is_empty() {
        return Ok(None);
    }

    let mut body = Some(Bytes::from(mem::take(body_buf)));
    let action = pipeline
        .execute_http_response_body(ctx, &mut body, true)
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
// StreamState
// -----------------------------------------------------------------------------

/// Per-stream state accumulated across ExtProc phases.
#[derive(Debug, Default)]
struct StreamState {
    /// Re-entrance counters from request-phase branch chains.
    branch_iterations: HashMap<Arc<str>, u32>,

    /// Executed filter indices from request phase.
    executed_filter_indices: Vec<bool>,

    /// Metadata carried from request to response phase.
    filter_metadata: HashMap<String, String>,

    /// Converted request from the headers phase.
    request: Option<Request>,

    /// Accumulated request body bytes.
    request_body: Vec<u8>,

    /// Converted response from the response headers phase.
    response: Option<Response>,

    /// Accumulated response body bytes.
    response_body: Vec<u8>,

    /// Whether response-phase filters already ran at header time.
    response_filters_executed: bool,

    /// End-of-stream tracking for protocol safety.
    eos_tracker: EosTracker,
}

impl StreamState {
    /// Create a new empty stream state.
    fn new() -> Self {
        Self::default()
    }

    /// Restore filter execution state into a response context.
    fn restore_request_ctx(&self, ctx: &mut HttpFilterContext<'_>) {
        ctx.executed_filter_indices.clone_from(&self.executed_filter_indices);
        ctx.branch_iterations.clone_from(&self.branch_iterations);
        ctx.filter_metadata.clone_from(&self.filter_metadata);
    }
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

/// Reject body accumulation exceeding [`MAX_BODY_ACCUMULATION`].
fn check_body_limit(current: usize, incoming: usize) -> Result<(), Status> {
    if current + incoming > MAX_BODY_ACCUMULATION {
        return Err(Status::resource_exhausted("body exceeds maximum size"));
    }
    Ok(())
}

/// Return a body slice reference if the buffer is non-empty.
fn body_data_if_present(buf: &[u8]) -> Option<&[u8]> {
    if buf.is_empty() { None } else { Some(buf) }
}

/// Label string for a request variant, used in debug logging.
fn request_type_label(req: &processing_request::Request) -> &'static str {
    match req {
        processing_request::Request::RequestHeaders(_) => "request_headers",
        processing_request::Request::RequestBody(_) => "request_body",
        processing_request::Request::ResponseHeaders(_) => "response_headers",
        processing_request::Request::ResponseBody(_) => "response_body",
        processing_request::Request::RequestTrailers(_) => "request_trailers",
        processing_request::Request::ResponseTrailers(_) => "response_trailers",
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eos_marker_default_is_not_received() {
        let marker = EosMarker::default();
        assert!(!marker.is_received(), "default marker should not be received");
    }

    #[test]
    fn eos_marker_mark_received_sets_received() {
        let mut marker = EosMarker::default();
        marker.mark_received();
        assert!(marker.is_received(), "marker should be received after marking");
    }

    #[test]
    fn eos_tracker_default_all_not_received() {
        let tracker = EosTracker::default();
        assert!(!tracker.request_headers.is_received());
        assert!(!tracker.request_body.is_received());
        assert!(!tracker.response_headers.is_received());
        assert!(!tracker.response_body.is_received());
    }

    #[test]
    fn eos_tracker_first_eos_succeeds() {
        let mut tracker = EosTracker::default();

        // First EOS in each phase should succeed
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());
        assert!(tracker.check_and_mark(ProtocolPhase::ResponseHeaders, true).is_ok());
        assert!(tracker.check_and_mark(ProtocolPhase::ResponseBody, true).is_ok());
    }

    #[test]
    fn eos_tracker_duplicate_eos_fails() {
        let mut tracker = EosTracker::default();

        // Mark first EOS
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());

        // Duplicate should fail
        let result = tracker.check_and_mark(ProtocolPhase::RequestHeaders, true);
        assert!(result.is_err(), "duplicate EOS should fail");

        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("duplicate end_of_stream"));
            assert!(err.message().contains("RequestHeaders"));
        }
    }

    #[test]
    fn eos_tracker_duplicate_eos_in_each_phase_fails() {
        let mut tracker = EosTracker::default();

        // Test duplicate detection in each phase independently
        let phases = [
            ProtocolPhase::RequestHeaders,
            ProtocolPhase::RequestBody,
            ProtocolPhase::ResponseHeaders,
            ProtocolPhase::ResponseBody,
        ];

        for phase in phases {
            // First EOS succeeds
            assert!(tracker.check_and_mark(phase, true).is_ok());

            // Duplicate fails
            let result = tracker.check_and_mark(phase, true);
            assert!(result.is_err(), "duplicate EOS should fail for {phase:?}");

            if let Err(err) = result {
                assert_eq!(err.code(), tonic::Code::InvalidArgument);
                assert!(err.message().contains("duplicate end_of_stream"));
            }
        }
    }

    #[test]
    fn eos_tracker_false_eos_is_noop() {
        let mut tracker = EosTracker::default();

        // Calling with received_eos=false should be a no-op
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, false).is_ok());
        assert!(!tracker.request_headers.is_received(), "marker should stay NotReceived");

        // Can still mark it later
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());
        assert!(tracker.request_headers.is_received(), "marker should now be Received");
    }

    #[test]
    fn eos_tracker_phases_are_independent() {
        let mut tracker = EosTracker::default();

        // Mark EOS in request headers
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());

        // Other phases should still allow first EOS
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());
        assert!(tracker.check_and_mark(ProtocolPhase::ResponseHeaders, true).is_ok());
        assert!(tracker.check_and_mark(ProtocolPhase::ResponseBody, true).is_ok());

        // All should be marked
        assert!(tracker.request_headers.is_received());
        assert!(tracker.request_body.is_received());
        assert!(tracker.response_headers.is_received());
        assert!(tracker.response_body.is_received());
    }

    #[test]
    fn eos_tracker_multiple_false_then_true() {
        let mut tracker = EosTracker::default();

        // Multiple false calls should all be no-ops
        for _ in 0..5 {
            assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, false).is_ok());
            assert!(!tracker.request_body.is_received());
        }

        // First true should succeed
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());
        assert!(tracker.request_body.is_received());

        // Subsequent false should still be ok (but marker stays Received)
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, false).is_ok());
        assert!(tracker.request_body.is_received());

        // Subsequent true should fail
        let result = tracker.check_and_mark(ProtocolPhase::RequestBody, true);
        assert!(result.is_err(), "duplicate EOS should fail");

        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
    }

    #[test]
    fn eos_tracker_error_message_includes_phase() {
        let mut tracker = EosTracker::default();

        // Mark each phase and verify error message includes phase name
        let test_cases = [
            (ProtocolPhase::RequestHeaders, "RequestHeaders"),
            (ProtocolPhase::RequestBody, "RequestBody"),
            (ProtocolPhase::ResponseHeaders, "ResponseHeaders"),
            (ProtocolPhase::ResponseBody, "ResponseBody"),
        ];

        for (phase, expected_name) in test_cases {
            assert!(tracker.check_and_mark(phase, true).is_ok(), "first EOS should succeed");

            let result = tracker.check_and_mark(phase, true);
            assert!(result.is_err(), "duplicate EOS should fail");

            if let Err(err) = result {
                assert!(
                    err.message().contains(expected_name),
                    "error for {:?} should contain '{}', got: {}",
                    phase,
                    expected_name,
                    err.message()
                );
            }
        }
    }
}
