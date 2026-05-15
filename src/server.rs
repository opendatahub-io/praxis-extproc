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
use tokio_stream::{StreamExt, wrappers::ReceiverStream};
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
    let mut ctx = adapter::build_filter_context(request);

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
    let mut ctx = adapter::build_filter_context(request);

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
    let mut ctx = adapter::build_filter_context(request);
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
