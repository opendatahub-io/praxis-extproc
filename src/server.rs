// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! gRPC [`ExternalProcessor`] implementation for Praxis filter pipelines.
//!
//! Receives Envoy ExtProc messages, translates them into Praxis filter
//! pipeline invocations, and returns header/body mutations or immediate
//! responses.
//!
//! [`ExternalProcessor`]: praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessor

use std::{mem, pin::Pin, sync::Arc, time::Instant};

use bytes::Bytes;
use http::HeaderMap;
use praxis_core::config::BodyLimitsConfig;
use praxis_filter::{FilterAction, FilterPipeline, HttpFilterContext, Request, Response};
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        ProcessingRequest, ProcessingResponse, ProtocolConfiguration, external_processor_server::ExternalProcessor,
        processing_request,
    },
};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request as TonicRequest, Response as TonicResponse, Status, Streaming};
use tracing::{debug, error, warn};

use crate::{
    adapter, metrics,
    response::{self, BodyMode},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Channel buffer size for the response stream.
const RESPONSE_CHANNEL_SIZE: usize = 16;

/// Once-per-process warning: body filters configured under NONE body mode.
static NONE_MODE_BODY_FILTERS: OnceWarning = OnceWarning::new();

/// Once-per-process warning: a stream opened without `protocol_config`.
static MISSING_PROTOCOL_CONFIG: OnceWarning = OnceWarning::new();

/// Once-per-process warning: STREAMED body filters produced header mutations.
static STREAMED_HEADER_MUTATIONS: OnceWarning = OnceWarning::new();

// -----------------------------------------------------------------------------
// OnceWarning
// -----------------------------------------------------------------------------

/// A configuration-mismatch warning that fires once per process.
///
/// The conditions these guard depend on the pipeline and on Envoy's
/// processing mode, not on the request, so repeating them for every stream
/// would only flood the log; later occurrences are logged at debug.
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

/// Parsed protocol configuration from Envoy.
///
/// Extracted from the first `ProcessingRequest` message's `protocol_config` field.
#[derive(Debug, Clone, Default)]
struct ProtocolConfig {
    /// Request body processing mode.
    request_body_mode: BodyMode,
    /// Response body processing mode.
    response_body_mode: BodyMode,
    /// Whether body is sent immediately without waiting for header response.
    ///
    /// Only applies to `STREAMED` body mode per Envoy spec; ignored for other
    /// modes. `FULL_DUPLEX_STREAMED` inherently streams body without waiting.
    ///
    /// See: `ProtocolConfiguration.send_body_without_waiting_for_header_response`
    #[expect(dead_code, reason = "captured for future STREAMED delayed-response implementation")]
    send_body_without_waiting: bool,
}

impl TryFrom<ProtocolConfiguration> for ProtocolConfig {
    type Error = String;

    fn try_from(proto_cfg: ProtocolConfiguration) -> Result<Self, Self::Error> {
        Ok(Self {
            request_body_mode: BodyMode::try_from(proto_cfg.request_body_mode)
                .map_err(|e| format!("request_body_mode: {e}"))?,
            response_body_mode: BodyMode::try_from(proto_cfg.response_body_mode)
                .map_err(|e| format!("response_body_mode: {e}"))?,
            send_body_without_waiting: proto_cfg.send_body_without_waiting_for_header_response,
        })
    }
}

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
    /// Ceilings on the body bytes assembled per stream and direction.
    limits: BodyLimitsConfig,

    /// Shared filter pipeline.
    pipeline: Arc<FilterPipeline>,
}

impl PraxisExtProc {
    /// Create a new ExtProc service backed by the given pipeline with default body limits.
    pub fn new(pipeline: Arc<FilterPipeline>) -> Self {
        Self::with_limits(pipeline, BodyLimitsConfig::default())
    }

    /// Create a new ExtProc service with explicit body limits.
    pub fn with_limits(pipeline: Arc<FilterPipeline>, limits: BodyLimitsConfig) -> Self {
        Self { limits, pipeline }
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
        let limits = self.limits.clone();
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(RESPONSE_CHANNEL_SIZE);

        tokio::spawn(async move {
            if let Err(e) = handle_stream(&pipeline, limits, &mut inbound, &tx).await {
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
    limits: BodyLimitsConfig,
    inbound: &mut Streaming<ProcessingRequest>,
    tx: &mpsc::Sender<Result<ProcessingResponse, Status>>,
) -> Result<(), Status> {
    let start = Instant::now();
    let mut stream_state = StreamState::new(pipeline, limits);

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
    let mut first_message_processed = false;

    while let Some(result) = inbound.next().await {
        let msg = result.map_err(|e| Status::internal(e.to_string()))?;

        apply_protocol_config(stream_state, msg.protocol_config, first_message_processed)?;
        first_message_processed = true;

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

/// Apply the `protocol_config` carried on a stream message, if any.
///
/// # Errors
///
/// Returns [`Status::invalid_argument`] if the configuration arrives on a
/// later message or requests unsupported body modes.
fn apply_protocol_config(
    stream_state: &mut StreamState,
    proto_cfg: Option<ProtocolConfiguration>,
    first_message_processed: bool,
) -> Result<(), Status> {
    match proto_cfg {
        Some(_) if first_message_processed => Err(Status::invalid_argument(
            "protocol_config may only be sent on the first stream message",
        )),
        Some(cfg) => config_from_first_message(stream_state, cfg),
        // Envoy releases before 1.33 never send protocol_config, so the body
        // modes are unknown and BUFFERED is assumed. Such an Envoy must be
        // configured with BUFFERED body modes for its streams to work.
        None if !first_message_processed => {
            if MISSING_PROTOCOL_CONFIG.first() {
                warn!(
                    "first message carries no protocol_config; assuming BUFFERED body modes (Envoy < 1.33 must be configured with BUFFERED)"
                );
            } else {
                debug!("no protocol_config on first message; assuming BUFFERED body modes");
            }
            Ok(())
        },
        None => Ok(()),
    }
}

/// Parses `protocol_config` from first message.
///
/// # Errors
///
/// Returns [`Status::invalid_argument`] if unsupported body modes are requested.
fn config_from_first_message(stream_state: &mut StreamState, proto_cfg: ProtocolConfiguration) -> Result<(), Status> {
    stream_state.protocol_config = ProtocolConfig::try_from(proto_cfg).map_err(Status::invalid_argument)?;
    debug!(
        request_mode = ?stream_state.protocol_config.request_body_mode,
        response_mode = ?stream_state.protocol_config.response_body_mode,
        "ExtProc protocol configuration received from Envoy"
    );
    Ok(())
}

/// Which direction of the exchange a message belongs to.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum PhaseSide {
    /// Request-side phases (headers, body, trailers).
    Request,
    /// Response-side phases (headers, body, trailers).
    Response,
}

/// Ordered position within one direction's phase sequence.
///
/// The derived ordering is `Headers` < `Body` < `Trailers`.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PhaseStep {
    /// Headers phase.
    Headers,
    /// Body phase.
    Body,
    /// Trailers phase.
    Trailers,
}

/// Direction and step of a message within its direction's sequence.
///
/// Each direction advances monotonically (headers → body → trailers), but the
/// two directions are independent: in `FULL_DUPLEX_STREAMED` Envoy interleaves
/// request-body chunks with response processing, so ordering is enforced per
/// direction rather than globally.
const fn message_order(req: &processing_request::Request) -> (PhaseSide, PhaseStep) {
    match req {
        processing_request::Request::RequestHeaders(_) => (PhaseSide::Request, PhaseStep::Headers),
        processing_request::Request::RequestBody(_) => (PhaseSide::Request, PhaseStep::Body),
        processing_request::Request::RequestTrailers(_) => (PhaseSide::Request, PhaseStep::Trailers),
        processing_request::Request::ResponseHeaders(_) => (PhaseSide::Response, PhaseStep::Headers),
        processing_request::Request::ResponseBody(_) => (PhaseSide::Response, PhaseStep::Body),
        processing_request::Request::ResponseTrailers(_) => (PhaseSide::Response, PhaseStep::Trailers),
    }
}

/// Tracks per-direction phase progression to reject out-of-order messages.
///
/// Each direction advances monotonically (`Headers` → `Body` → `Trailers`); the
/// two are independent so `FULL_DUPLEX_STREAMED` interleaving is allowed.
/// Response messages are gated on request headers having been seen.
#[derive(Debug, Default)]
struct PhaseOrderTracker {
    /// Furthest request-side step seen.
    request: Option<PhaseStep>,
    /// Furthest response-side step seen.
    response: Option<PhaseStep>,
    /// Whether request headers have been received, gating response processing.
    request_headers_seen: bool,
}

impl PhaseOrderTracker {
    /// Validate a message's position and advance the tracker.
    ///
    /// Equal steps are allowed (repeated body chunks); duplicate-EOS is caught
    /// per-phase by [`EosTracker`]. Called before any handler mutates state, so a
    /// rejection leaves no partial per-stream state.
    ///
    /// # Errors
    ///
    /// Returns [`Status::invalid_argument`] when `req` regresses within its
    /// direction, or when a response message precedes request headers.
    fn check_and_advance(&mut self, req: &processing_request::Request) -> Result<(), Status> {
        let (side, step) = message_order(req);
        let current = match side {
            PhaseSide::Request => &mut self.request,
            PhaseSide::Response => {
                if !self.request_headers_seen {
                    return Err(Status::invalid_argument(format!(
                        "out-of-order ExtProc message: {} arrived before request headers",
                        request_type_label(req)
                    )));
                }
                &mut self.response
            },
        };
        let invalid_transition = match *current {
            None => step != PhaseStep::Headers,
            Some(prev) => step < prev || (step == prev && step != PhaseStep::Body),
        };

        if invalid_transition {
            return Err(Status::invalid_argument(format!(
                "out-of-order ExtProc message: invalid {side:?} phase transition to {}",
                request_type_label(req)
            )));
        }
        *current = Some(step);

        if matches!(req, processing_request::Request::RequestHeaders(_)) {
            self.request_headers_seen = true;
        }
        Ok(())
    }
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
    state.phase_order.check_and_advance(&req)?;

    match req {
        processing_request::Request::RequestHeaders(h) => handle_request_headers(pipeline, h, state).await,
        processing_request::Request::RequestBody(b) => handle_request_body(pipeline, b, state).await,
        processing_request::Request::ResponseHeaders(h) => handle_response_headers(pipeline, h, state).await,
        processing_request::Request::ResponseBody(b) => handle_response_body(pipeline, b, state).await,
        processing_request::Request::RequestTrailers(_) => handle_trailers(pipeline, state, true).await,
        processing_request::Request::ResponseTrailers(_) => handle_trailers(pipeline, state, false).await,
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

/// End-of-stream lifecycle state of a single protocol phase.
///
/// Doubles as the outcome of [`EosTracker::check_and_mark`], which returns the
/// phase's state *on entry*: [`PhaseState::Completed`] means `end_of_stream` was
/// already seen, so the current message is a re-delivery.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
enum PhaseState {
    /// No `end_of_stream` seen yet; the phase is still being processed.
    #[default]
    Active,
    /// `end_of_stream` received; the phase is complete.
    Completed,
}

impl PhaseState {
    /// Whether the phase has completed (`end_of_stream` seen).
    const fn is_complete(self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// Tracks end-of-stream status for each protocol phase.
#[derive(Debug, Default)]
struct EosTracker {
    /// Request headers phase state.
    request_headers: PhaseState,
    /// Request body phase state.
    request_body: PhaseState,
    /// Response headers phase state.
    response_headers: PhaseState,
    /// Response body phase state.
    response_body: PhaseState,
}

impl EosTracker {
    /// Current state of a phase.
    fn phase_state(&self, phase: ProtocolPhase) -> PhaseState {
        match phase {
            ProtocolPhase::RequestHeaders => self.request_headers,
            ProtocolPhase::RequestBody => self.request_body,
            ProtocolPhase::ResponseHeaders => self.response_headers,
            ProtocolPhase::ResponseBody => self.response_body,
        }
    }

    /// Detect re-delivery and mark end-of-stream for a protocol phase.
    ///
    /// Returns the phase's state *on entry*: [`PhaseState::Completed`] means the
    /// message is a re-delivery, leaving the benign-vs-violation policy to the
    /// caller (which knows the body mode). A message on a body phase whose headers
    /// phase already ended is always a genuine sequencing violation, rejected here.
    ///
    /// # Errors
    ///
    /// Returns [`Status::invalid_argument`] if a body message arrives after its
    /// headers phase has ended.
    fn check_and_mark(&mut self, phase: ProtocolPhase, received_eos: bool) -> Result<PhaseState, Status> {
        // An already-completed phase means this message is a re-delivery; the
        // caller decides whether that is benign (FDS) or a violation.
        if self.phase_state(phase).is_complete() {
            return Ok(PhaseState::Completed);
        }

        // For body phases: a message after the corresponding headers phase ended
        // is a genuine sequencing violation regardless of mode.
        let headers_completed = match phase {
            ProtocolPhase::RequestBody => self.request_headers.is_complete(),
            ProtocolPhase::ResponseBody => self.response_headers.is_complete(),
            ProtocolPhase::RequestHeaders | ProtocolPhase::ResponseHeaders => false,
        };

        if headers_completed {
            return Err(Status::invalid_argument(format!(
                "received {phase:?} message after headers end_of_stream was already marked"
            )));
        }

        if received_eos {
            self.mark_complete(phase);
        }

        Ok(PhaseState::Active)
    }

    /// Mark a phase complete.
    ///
    /// Used for the end-of-stream flag and for trailers, which close a body
    /// phase without any body message carrying the flag.
    fn mark_complete(&mut self, phase: ProtocolPhase) {
        match phase {
            ProtocolPhase::RequestHeaders => self.request_headers = PhaseState::Completed,
            ProtocolPhase::RequestBody => self.request_body = PhaseState::Completed,
            ProtocolPhase::ResponseHeaders => self.response_headers = PhaseState::Completed,
            ProtocolPhase::ResponseBody => self.response_body = PhaseState::Completed,
        }
    }
}

// -----------------------------------------------------------------------------
// Phase Handlers
// -----------------------------------------------------------------------------

/// Error for a message re-delivered after its phase already completed.
fn duplicate_after_eos(phase: ProtocolPhase) -> Status {
    Status::invalid_argument(format!(
        "received {phase:?} message after end_of_stream was already marked"
    ))
}

/// Whether a headers message is the complete message for its direction.
///
/// In `NONE` body mode Envoy never sends a body, so the headers message is
/// complete even when `end_of_stream` is false; the pipeline must run now or
/// it never runs at all.
fn headers_complete_message(end_of_stream: bool, mode: BodyMode) -> bool {
    end_of_stream || mode == BodyMode::None
}

/// Whether a body message carries the complete body for its direction.
///
/// In `BUFFERED` mode Envoy delivers the entire body in a single message and
/// waits for the `BodyResponse` before it sends anything else. Its
/// `end_of_stream` is false only when trailers follow, so waiting for a
/// later end-of-stream message would deadlock the stream. Streamed modes
/// really do deliver the body in several messages.
fn body_complete_message(end_of_stream: bool, mode: BodyMode) -> bool {
    end_of_stream || mode == BodyMode::Buffered
}

/// Warn (once per process) when body filters are configured but Envoy will never send the body.
fn warn_body_never_sent(direction: &str, needs_body: bool) {
    if !needs_body {
        return;
    }
    if NONE_MODE_BODY_FILTERS.first() {
        warn!(
            direction,
            "pipeline declares body filters but Envoy body mode is NONE; body filters will not run"
        );
    } else {
        debug!(direction, "body filters skipped: Envoy body mode is NONE");
    }
}

/// Apply body-phase policy to the phase state observed by [`EosTracker::check_and_mark`].
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

/// Handle request headers: parse into [`Request`] and route by body mode.
///
/// For `NONE`, the headers are the complete request — pipeline runs now.
/// For `BUFFERED`, sends an empty `HeadersResponse` — pipeline runs at body EOS.
/// For `STREAMED`, runs filters early and sends mutations in `HeadersResponse`.
/// For `FDS` with body filters, returns no response — full pipeline at body EOS.
/// For `FDS` passthrough, runs header filters early, defers mutations to first chunk.
///
/// [`Request`]: praxis_filter::Request
async fn handle_request_headers(
    pipeline: &FilterPipeline,
    headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.request_body_mode;
    let complete = headers_complete_message(headers.end_of_stream, mode);

    if state
        .eos_tracker
        .check_and_mark(ProtocolPhase::RequestHeaders, complete)?
        == PhaseState::Completed
    {
        // Envoy does not re-deliver headers; a duplicate is a protocol violation.
        return Err(duplicate_after_eos(ProtocolPhase::RequestHeaders));
    }

    let envoy_headers = extract_header_list(&headers);
    state.request = Some(adapter::envoy_headers_to_request(&envoy_headers));

    if complete {
        if !headers.end_of_stream {
            warn_body_never_sent("request", pipeline.body_capabilities().needs_request_body);
        }
        return run_request_pipeline(RequestPhase::Headers, pipeline, state).await;
    }

    match mode {
        BodyMode::FullDuplexStreamed if !pipeline.body_capabilities().needs_request_body => {
            run_request_header_filters_early(pipeline, state, MutationDelivery::DeferSilent).await
        },
        BodyMode::FullDuplexStreamed => Ok(Vec::new()),
        BodyMode::Streamed => {
            state.header_state.request_headers_sent = true;
            run_request_header_filters_early(pipeline, state, MutationDelivery::Send).await
        },
        _ => Ok(vec![response::request_headers(None)]),
    }
}

/// Handle request body: route by body mode and filter capabilities.
async fn handle_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.request_body_mode;
    let complete = body_complete_message(body.end_of_stream, mode);

    if let Some(response) = handle_body_redelivery(
        state.eos_tracker.check_and_mark(ProtocolPhase::RequestBody, complete)?,
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
        _ => accumulate_request_body(pipeline, body, complete, state).await,
    }
}

/// Accumulate request body chunks, run full pipeline once the body is complete.
async fn accumulate_request_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    complete: bool,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if let Some(imm) = body_limit_exceeded(
        state.request_body.len(),
        body.body.len(),
        state.limits.max_request_bytes,
        "request",
    ) {
        return Ok(vec![response::immediate(imm)]);
    }
    state.request_body.extend_from_slice(&body.body);

    if !complete {
        return Ok(Vec::new());
    }

    run_request_pipeline(RequestPhase::Body(BodyEnd::EndOfStream), pipeline, state).await
}

/// Handle response headers: run response filters and respond with mutations.
///
/// For `NONE`, the headers are the complete response — pipeline runs now.
/// For `BUFFERED`, runs filters early and defers mutations to body phase
/// (Envoy honours `CommonResponse.header_mutation` on body responses).
/// For `STREAMED`, runs filters early and sends mutations immediately
/// (Envoy ignores header mutations on body responses for non-`BUFFERED`).
/// For `FDS` with body filters, returns no response — full pipeline at body EOS.
/// For `FDS` passthrough, runs filters early, defers mutations to first chunk.
async fn handle_response_headers(
    pipeline: &FilterPipeline,
    headers: praxis_proto::envoy::service::ext_proc::v3::HttpHeaders,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.response_body_mode;
    let complete = headers_complete_message(headers.end_of_stream, mode);

    if state
        .eos_tracker
        .check_and_mark(ProtocolPhase::ResponseHeaders, complete)?
        == PhaseState::Completed
    {
        // Envoy does not re-deliver headers; a duplicate is a protocol violation.
        return Err(duplicate_after_eos(ProtocolPhase::ResponseHeaders));
    }

    let envoy_headers = extract_header_list(&headers);
    state.response = Some(adapter::envoy_headers_to_response(&envoy_headers));

    if complete {
        if !headers.end_of_stream {
            warn_body_never_sent("response", pipeline.body_capabilities().needs_response_body);
        }
        return run_response_pipeline(ResponsePhase::Headers, pipeline, state).await;
    }

    match mode {
        BodyMode::FullDuplexStreamed if !pipeline.body_capabilities().needs_response_body => {
            run_response_header_filters_early(pipeline, state, MutationDelivery::DeferSilent).await
        },
        BodyMode::FullDuplexStreamed => Ok(Vec::new()),
        BodyMode::Streamed => {
            state.header_state.response_headers_sent = true;
            run_response_header_filters_early(pipeline, state, MutationDelivery::Send).await
        },
        _ => run_response_header_filters_early(pipeline, state, MutationDelivery::DeferWithResponse).await,
    }
}

/// Handle response body: route by body mode and filter capabilities.
async fn handle_response_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mode = state.protocol_config.response_body_mode;
    let complete = body_complete_message(body.end_of_stream, mode);

    if let Some(response) = handle_body_redelivery(
        state
            .eos_tracker
            .check_and_mark(ProtocolPhase::ResponseBody, complete)?,
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
        _ => accumulate_response_body(pipeline, body, complete, state).await,
    }
}

/// Accumulate response body chunks, run full pipeline once the body is complete.
async fn accumulate_response_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    complete: bool,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    if let Some(imm) = body_limit_exceeded(
        state.response_body.len(),
        body.body.len(),
        state.limits.max_response_bytes,
        "response",
    ) {
        return Ok(vec![response::immediate(imm)]);
    }
    state.response_body.extend_from_slice(&body.body);

    if !complete {
        return Ok(Vec::new());
    }

    run_response_pipeline(ResponsePhase::Body(BodyEnd::EndOfStream), pipeline, state).await
}

// -----------------------------------------------------------------------------
// Trailers
// -----------------------------------------------------------------------------

/// Handle trailers for one direction: release anything held for the body, then acknowledge.
async fn handle_trailers(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let mut responses = finalize_body_on_trailers(pipeline, state, is_request).await?;
    if !responses.last().is_some_and(response::is_immediate) {
        responses.push(if is_request {
            response::request_trailers()
        } else {
            response::response_trailers()
        });
    }
    Ok(responses)
}

/// Complete a body phase when trailers, not `end_of_stream`, close it.
///
/// Envoy signals the end of a body that carries trailers with the trailers
/// message itself. Everything held back for end-of-stream — the deferred
/// headers response, the accumulated body, the pipeline run — must be
/// released now or the stream stalls until Envoy's message timeout.
async fn finalize_body_on_trailers(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    if !state.body_open(is_request) {
        return Ok(Vec::new());
    }

    let caps = pipeline.body_capabilities();
    let (phase, mode, needs_body) = if is_request {
        (
            ProtocolPhase::RequestBody,
            state.protocol_config.request_body_mode,
            caps.needs_request_body,
        )
    } else {
        (
            ProtocolPhase::ResponseBody,
            state.protocol_config.response_body_mode,
            caps.needs_response_body,
        )
    };
    state.eos_tracker.mark_complete(phase);

    match (mode, needs_body) {
        (BodyMode::FullDuplexStreamed, true) => run_body_pipeline(BodyEnd::Trailers, pipeline, state, is_request).await,
        (BodyMode::FullDuplexStreamed, false) => Ok(deferred_headers_response(state, is_request).into_iter().collect()),
        (BodyMode::Buffered, _) => Ok(rejection_only(
            run_body_pipeline(BodyEnd::Trailers, pipeline, state, is_request).await?,
            is_request,
        )),
        _ => Ok(Vec::new()),
    }
}

/// Run the body-phase pipeline for one direction.
async fn run_body_pipeline(
    end: BodyEnd,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    if is_request {
        run_request_pipeline(RequestPhase::Body(end), pipeline, state).await
    } else {
        run_response_pipeline(ResponsePhase::Body(end), pipeline, state).await
    }
}

/// The headers response deferred to a direction's first body chunk, if not yet sent.
///
/// Carries the header mutations collected when the filters ran early; the
/// first caller per direction gets it, later callers get `None`.
fn deferred_headers_response(state: &mut StreamState, is_request: bool) -> Option<ProcessingResponse> {
    if !state.header_state.take_first_chunk(is_request) {
        return None;
    }
    Some(if is_request {
        response::request_headers(state.deferred_request_header_mutation.take())
    } else {
        response::response_headers(state.deferred_response_header_mutation.take())
    })
}

/// Keep only an `ImmediateResponse` from a pipeline run that had no body message to answer.
///
/// A BUFFERED body that turns out to be empty never produces a body message,
/// so header and body mutations have nothing to ride on; a rejection still
/// applies at any point in the stream.
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

/// Log label for a stream direction.
fn direction_label(is_request: bool) -> &'static str {
    if is_request { "request" } else { "response" }
}

// -----------------------------------------------------------------------------
// Pipeline Execution
// -----------------------------------------------------------------------------

/// How a body phase was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyEnd {
    /// The final body message carried `end_of_stream`.
    EndOfStream,
    /// A trailers message closed the body.
    Trailers,
}

/// Request filter execution phase.
#[derive(Debug, Clone, Copy)]
enum RequestPhase {
    /// Headers phase (headers EOS=true).
    Headers,
    /// Body phase, once the body is complete.
    Body(BodyEnd),
}

/// Execute request pipeline for the given phase.
///
/// Returns headers or body response with mutations.
async fn run_request_pipeline(
    phase: RequestPhase,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Err(Status::invalid_argument("request headers not received"));
    };
    let mut ctx = adapter::PhaseContext::new(pipeline, request, &mut state.context);

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
enum ResponsePhase {
    /// Headers phase (response headers EOS=true).
    Headers,
    /// Body phase, once the body is complete.
    Body(BodyEnd),
}

/// Execute response pipeline for the given phase.
///
/// Returns headers or body response with mutations.
#[expect(clippy::too_many_lines, reason = "context borrowing prevents extraction")]
async fn run_response_pipeline(
    phase: ResponsePhase,
    pipeline: &FilterPipeline,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Err(Status::invalid_argument("request headers not received"));
    };

    let mut resp = state
        .response
        .take()
        .ok_or_else(|| Status::invalid_argument("response headers not received"))?;

    let mut ctx = adapter::PhaseContext::new(pipeline, request, &mut state.context);
    let original_headers = resp.headers.clone();
    ctx.response_header = Some(&mut resp);

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

    let mutation = match phase {
        ResponsePhase::Headers => current_mutation,
        ResponsePhase::Body(_) => {
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
        ResponsePhase::Body(_) => !filters_executed,
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
        (RequestPhase::Body(end), BodyMode::FullDuplexStreamed) => {
            let mut r = vec![response::request_headers(mutation)];
            r.extend(assembled_body_responses(end, body, mode, true));
            r
        },
        (RequestPhase::Body(_), _) => response::request_body(body, mutation, mode, true),
    }
}

/// Streamed responses for a body the server assembled from FDS chunks.
///
/// At end-of-stream the body is always emitted, even when empty, so the
/// final chunk carries the flag. When trailers closed the body the chunks
/// must not claim end-of-stream — Envoy applies that flag to the HTTP stream
/// ahead of the trailers — and an empty body has nothing to forward.
fn assembled_body_responses(
    end: BodyEnd,
    body: Option<&[u8]>,
    mode: BodyMode,
    is_request: bool,
) -> Vec<ProcessingResponse> {
    match end {
        BodyEnd::EndOfStream => response::body_responses(body, None, is_request, mode, true),
        BodyEnd::Trailers => body
            .filter(|b| !b.is_empty())
            .map(|b| response::body_responses(Some(b), None, is_request, mode, false))
            .unwrap_or_default(),
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
        (ResponsePhase::Body(end), BodyMode::FullDuplexStreamed) => {
            let mut r = vec![response::response_headers(mutation)];
            r.extend(assembled_body_responses(end, body, mode, false));
            r
        },
        (ResponsePhase::Body(_), _) => response::response_body(body, mutation, mode, true),
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
fn passthrough_chunk(
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

    let Some(hdr) = deferred_headers_response(state, is_request) else {
        return body_responses;
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
async fn process_streamed_body_chunk(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    let request = state
        .request
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("request headers not received"))?;
    // The response-side check needs a header snapshot; take it only on the
    // final chunk and only until the once-per-process warning has fired.
    let original_response_headers = (!is_request && body.end_of_stream && STREAMED_HEADER_MUTATIONS.pending())
        .then(|| state.response.as_ref().map(|r| r.headers.clone()))
        .flatten();
    let mut ctx = adapter::PhaseContext::new(pipeline, request, &mut state.context);
    if !is_request {
        let resp = state
            .response
            .as_mut()
            .ok_or_else(|| Status::invalid_argument("response headers not received"))?;
        ctx.response_header = Some(resp);
    }
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
    warn_unapplied_header_mutations(&ctx, is_request, original_response_headers.as_ref());
    drop(ctx);
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

/// Warn (once per process) when body filters changed headers that Envoy will not apply.
///
/// Header mutations on a body response only take effect in `BUFFERED` mode.
/// In `STREAMED` mode the headers were forwarded when the `HeadersResponse`
/// went out, so a filter that derives headers from the body (for example
/// `model_to_header`) is silently ineffective. Surface that so operators
/// can switch the direction to `BUFFERED` or `FULL_DUPLEX_STREAMED`.
fn warn_unapplied_header_mutations(
    ctx: &HttpFilterContext<'_>,
    is_request: bool,
    original_response_headers: Option<&HeaderMap>,
) {
    if !STREAMED_HEADER_MUTATIONS.pending() {
        return;
    }
    let mutation = if is_request {
        adapter::collect_request_header_mutations(ctx)
    } else {
        original_response_headers.and_then(|original| adapter::collect_response_header_mutations_diff(ctx, original))
    };
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
enum MutationDelivery {
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
async fn run_request_header_filters_early(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    delivery: MutationDelivery,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Ok(delivery.deliver_request(None, state));
    };
    let mut ctx = adapter::PhaseContext::new(pipeline, request, &mut state.context);

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = immediate_from_action(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_request_header_mutations(&ctx);
    drop(ctx);

    Ok(delivery.deliver_request(mutation, state))
}

/// Run response header filters early and deliver mutations per strategy.
async fn run_response_header_filters_early(
    pipeline: &FilterPipeline,
    state: &mut StreamState,
    delivery: MutationDelivery,
) -> Result<Vec<ProcessingResponse>, Status> {
    let Some(request) = state.request.as_ref() else {
        return Ok(delivery.deliver_response(None, state));
    };

    let Some(resp) = state.response.as_mut() else {
        return Ok(delivery.deliver_response(None, state));
    };

    let original_headers = resp.headers.clone();
    let mut ctx = adapter::PhaseContext::new(pipeline, request, &mut state.context);
    ctx.response_header = Some(resp);

    let action = execute_response(pipeline, &mut ctx).await?;
    if let Some(imm) = immediate_from_action(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    state.header_state.response_filters_executed = true;
    let mutation = adapter::collect_response_header_mutations_diff(&ctx, &original_headers);
    drop(ctx);

    Ok(delivery.deliver_response(mutation, state))
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
/// Both `Reject` and `TerminalResponse` end the request with a local reply
/// at Envoy. `Release` and `BodyDone` mean `Continue` outside their body
/// contexts by contract.
fn immediate_from_action(
    action: FilterAction,
) -> Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse> {
    let immediate = match action {
        FilterAction::Reject(rejection) => adapter::rejection_to_immediate(&rejection),
        FilterAction::TerminalResponse(terminal) => adapter::terminal_response_to_immediate(&terminal),
        FilterAction::Continue | FilterAction::Release | FilterAction::BodyDone => return None,
    };
    metrics::record_immediate_response();
    Some(immediate)
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

    Ok(immediate_from_action(action))
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

    Ok(immediate_from_action(action))
}

// -----------------------------------------------------------------------------
// StreamState
// -----------------------------------------------------------------------------

/// Tracks header response delivery and filter execution across phases.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-direction flags, not a state machine"
)]
struct HeaderDeliveryState {
    /// Whether response-phase filters already ran at header time.
    response_filters_executed: bool,
    /// Whether the deferred request `HeadersResponse` has been sent.
    request_headers_sent: bool,
    /// Whether the deferred response `HeadersResponse` has been sent.
    response_headers_sent: bool,
}

impl HeaderDeliveryState {
    /// Mark direction as sent; returns `true` on first call per direction.
    fn take_first_chunk(&mut self, is_request: bool) -> bool {
        let sent = if is_request {
            &mut self.request_headers_sent
        } else {
            &mut self.response_headers_sent
        };
        if *sent {
            return false;
        }
        *sent = true;
        true
    }
}

/// Per-stream state accumulated across ExtProc phases.
#[derive(Debug)]
struct StreamState {
    /// Filter context state threaded through every phase of the stream.
    context: adapter::CarriedContext,

    /// Ceilings on the body bytes assembled per direction.
    limits: BodyLimitsConfig,

    /// Converted request from the headers phase.
    request: Option<Request>,

    /// Accumulated request body bytes.
    request_body: Vec<u8>,

    /// Converted response from the response headers phase.
    response: Option<Response>,

    /// Accumulated response body bytes.
    response_body: Vec<u8>,

    /// Header delivery tracking across phases.
    header_state: HeaderDeliveryState,

    /// End-of-stream tracking for protocol safety.
    eos_tracker: EosTracker,

    /// Protocol configuration parsed from Envoy's first message.
    protocol_config: ProtocolConfig,

    /// Deferred request header mutation for FDS passthrough mode.
    deferred_request_header_mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,

    /// Deferred response header mutation for BUFFERED or FDS passthrough mode.
    deferred_response_header_mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,

    /// Per-direction phase ordering guard.
    phase_order: PhaseOrderTracker,
}

impl StreamState {
    /// Create a new empty stream state with default protocol configuration.
    fn new(pipeline: &FilterPipeline, limits: BodyLimitsConfig) -> Self {
        Self {
            context: adapter::CarriedContext::new(pipeline),
            limits,
            request: None,
            request_body: Vec::new(),
            response: None,
            response_body: Vec::new(),
            header_state: HeaderDeliveryState::default(),
            eos_tracker: EosTracker::default(),
            protocol_config: ProtocolConfig::default(),
            deferred_request_header_mutation: None,
            deferred_response_header_mutation: None,
            phase_order: PhaseOrderTracker::default(),
        }
    }

    /// Whether the request body phase is still waiting to be closed.
    ///
    /// False before any headers arrived, once the headers said there is no
    /// body, or once the body completed.
    fn request_body_open(&self) -> bool {
        self.request.is_some()
            && !self.eos_tracker.request_headers.is_complete()
            && !self.eos_tracker.request_body.is_complete()
    }

    /// Whether the body phase of a direction is still waiting to be closed.
    fn body_open(&self, is_request: bool) -> bool {
        if is_request {
            self.request_body_open()
        } else {
            self.response_body_open()
        }
    }

    /// Whether the response body phase is still waiting to be closed.
    ///
    /// See [`request_body_open`](Self::request_body_open).
    fn response_body_open(&self) -> bool {
        self.response.is_some()
            && !self.eos_tracker.response_headers.is_complete()
            && !self.eos_tracker.response_body.is_complete()
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

/// Reject a body that would exceed `limit` once `incoming` bytes are added.
///
/// Answers with a local `413` rather than a stream error: a stream error
/// lets a fail-open Envoy forward the oversized body unfiltered, whereas an
/// `ImmediateResponse` is enforced in every failure mode.
fn body_limit_exceeded(
    current: usize,
    incoming: usize,
    limit: Option<usize>,
    direction: &str,
) -> Option<praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse> {
    let max = limit?;
    if current.saturating_add(incoming) <= max {
        return None;
    }
    warn!(direction, max, "body exceeds the configured limit; rejecting with 413");
    immediate_from_action(FilterAction::Reject(
        praxis_filter::Rejection::status(413).with_body(format!("{direction} body exceeds {max} bytes")),
    ))
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
    use super::*;

    #[test]
    fn phase_state_default_is_active() {
        let state = PhaseState::default();
        assert_eq!(state, PhaseState::Active, "default phase state should be Active");
        assert!(!state.is_complete(), "default phase state should not be complete");
    }

    #[test]
    fn eos_tracker_default_all_active() {
        let tracker = EosTracker::default();
        assert!(
            !tracker.request_headers.is_complete(),
            "request_headers should be Active"
        );
        assert!(!tracker.request_body.is_complete(), "request_body should be Active");
        assert!(
            !tracker.response_headers.is_complete(),
            "response_headers should be Active"
        );
        assert!(!tracker.response_body.is_complete(), "response_body should be Active");
    }

    #[test]
    fn eos_tracker_first_eos_succeeds() {
        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok(),
            "first EOS in RequestHeaders should succeed"
        );

        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok(),
            "first EOS in RequestBody should succeed"
        );

        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::ResponseHeaders, true).is_ok(),
            "first EOS in ResponseHeaders should succeed"
        );

        let mut tracker = EosTracker::default();
        assert!(
            tracker.check_and_mark(ProtocolPhase::ResponseBody, true).is_ok(),
            "first EOS in ResponseBody should succeed"
        );
    }

    #[test]
    fn eos_tracker_duplicate_eos_reports_duplicate() {
        let mut tracker = EosTracker::default();

        // First EOS is a fresh message to process.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).unwrap(),
            PhaseState::Active
        );

        // Re-delivery is reported as Completed (policy is left to the caller).
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).unwrap(),
            PhaseState::Completed,
            "re-delivery should report Completed"
        );
    }

    #[test]
    fn eos_tracker_duplicate_eos_in_each_phase_reports_duplicate() {
        // Test re-delivery detection in each phase independently.
        // Use separate trackers since body phases are blocked after header EOS.
        let phases = [
            ProtocolPhase::RequestHeaders,
            ProtocolPhase::RequestBody,
            ProtocolPhase::ResponseHeaders,
            ProtocolPhase::ResponseBody,
        ];

        for phase in phases {
            let mut tracker = EosTracker::default();

            assert_eq!(
                tracker.check_and_mark(phase, true).unwrap(),
                PhaseState::Active,
                "first EOS should be Active for {phase:?}"
            );

            assert_eq!(
                tracker.check_and_mark(phase, true).unwrap(),
                PhaseState::Completed,
                "re-delivery should report Completed for {phase:?}"
            );
        }
    }

    /// Assert every message classifies to `side` with strictly increasing steps.
    fn assert_monotonic(side: PhaseSide, msgs: &[processing_request::Request]) {
        let orders = msgs.iter().map(message_order).collect::<Vec<_>>();
        assert!(
            orders.iter().all(|(s, _)| *s == side),
            "messages must classify as {side:?}, got {orders:?}"
        );
        assert!(
            orders
                .windows(2)
                .all(|w| w.first().map(|t| t.1) < w.last().map(|t| t.1)),
            "steps must be strictly increasing, got {orders:?}"
        );
    }

    #[test]
    fn message_order_is_monotonic_within_each_direction() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders, HttpTrailers};
        use processing_request::Request;

        assert_monotonic(
            PhaseSide::Request,
            &[
                Request::RequestHeaders(HttpHeaders::default()),
                Request::RequestBody(HttpBody::default()),
                Request::RequestTrailers(HttpTrailers::default()),
            ],
        );
        assert_monotonic(
            PhaseSide::Response,
            &[
                Request::ResponseHeaders(HttpHeaders::default()),
                Request::ResponseBody(HttpBody::default()),
                Request::ResponseTrailers(HttpTrailers::default()),
            ],
        );
    }

    #[test]
    fn phase_order_allows_forward_and_repeated_phases() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::RequestBody(HttpBody::default()),
            Request::RequestBody(HttpBody::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::ResponseBody(HttpBody::default()),
        ] {
            assert!(
                tracker.check_and_advance(&req).is_ok(),
                "forward/repeated sequence must be accepted, rejected at {req:?}"
            );
        }
    }

    #[test]
    fn phase_order_allows_full_duplex_interleaving() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        // FULL_DUPLEX_STREAMED: request-body chunks may interleave with response
        // processing. RequestHeaders -> RequestBody -> ResponseHeaders -> RequestBody
        // is a legal sequence and must not be rejected.
        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::RequestBody(HttpBody::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::RequestBody(HttpBody::default()),
        ] {
            assert!(
                tracker.check_and_advance(&req).is_ok(),
                "interleaved sequence must be accepted, rejected at {req:?}"
            );
        }
    }

    #[test]
    fn phase_order_rejects_within_direction_regression() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        for req in [
            Request::RequestHeaders(HttpHeaders::default()),
            Request::ResponseHeaders(HttpHeaders::default()),
            Request::ResponseBody(HttpBody::default()),
        ] {
            assert!(tracker.check_and_advance(&req).is_ok());
        }

        // ResponseHeaders after ResponseBody regresses within the response direction.
        let result = tracker.check_and_advance(&Request::ResponseHeaders(HttpHeaders::default()));
        assert!(result.is_err(), "ResponseHeaders after ResponseBody should be rejected");
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("out-of-order"));
        }
    }

    #[test]
    fn phase_order_rejects_response_before_request_headers() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpHeaders;
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        let result = tracker.check_and_advance(&Request::ResponseHeaders(HttpHeaders::default()));
        assert!(result.is_err(), "response before request headers should be rejected");
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("before request headers"));
        }
    }

    #[test]
    fn phase_order_rejects_request_direction_regression() {
        use praxis_proto::envoy::service::ext_proc::v3::{HttpBody, HttpHeaders, HttpTrailers};
        use processing_request::Request;
        let mut tracker = PhaseOrderTracker::default();

        assert!(
            tracker
                .check_and_advance(&Request::RequestHeaders(HttpHeaders::default()))
                .is_ok()
        );
        assert!(
            tracker
                .check_and_advance(&Request::RequestTrailers(HttpTrailers::default()))
                .is_ok()
        );

        let result = tracker.check_and_advance(&Request::RequestBody(HttpBody::default()));
        assert!(result.is_err(), "RequestBody after RequestTrailers should be rejected");
        assert_eq!(result.unwrap_err().code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn eos_tracker_false_eos_is_noop() {
        let mut tracker = EosTracker::default();

        // Calling with received_eos=false should be a no-op
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, false).is_ok());
        assert!(!tracker.request_headers.is_complete(), "phase should stay Active");

        // Can still mark it later
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());
        assert!(tracker.request_headers.is_complete(), "phase should now be Completed");
    }

    #[test]
    fn eos_tracker_body_blocked_after_headers() {
        let mut tracker = EosTracker::default();

        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());

        let result = tracker.check_and_mark(ProtocolPhase::RequestBody, true);
        assert!(
            result.is_err(),
            "RequestBody should be blocked after RequestHeaders EOS"
        );
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("after headers end_of_stream"));
        }

        assert!(tracker.check_and_mark(ProtocolPhase::ResponseHeaders, true).is_ok());

        let result = tracker.check_and_mark(ProtocolPhase::ResponseBody, true);
        assert!(
            result.is_err(),
            "ResponseBody should be blocked after ResponseHeaders EOS"
        );
        if let Err(err) = result {
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(err.message().contains("after headers end_of_stream"));
        }
    }

    #[test]
    fn eos_tracker_multiple_false_then_true() {
        let mut tracker = EosTracker::default();

        // Multiple false calls should all be no-ops
        for _ in 0..5 {
            assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, false).is_ok());
            assert!(!tracker.request_body.is_complete());
        }

        // First true should succeed
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());
        assert!(tracker.request_body.is_complete());

        // Subsequent message (even with false) is a re-delivery.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, false).unwrap(),
            PhaseState::Completed,
            "re-delivery should report Completed even with end_of_stream=false"
        );

        // Subsequent true is also a re-delivery.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, true).unwrap(),
            PhaseState::Completed,
            "re-delivery should report Completed"
        );
    }

    #[test]
    fn duplicate_after_eos_error_includes_phase() {
        let test_cases = [
            (ProtocolPhase::RequestHeaders, "RequestHeaders"),
            (ProtocolPhase::RequestBody, "RequestBody"),
            (ProtocolPhase::ResponseHeaders, "ResponseHeaders"),
            (ProtocolPhase::ResponseBody, "ResponseBody"),
        ];

        for (phase, expected_name) in test_cases {
            let err = duplicate_after_eos(phase);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
            assert!(
                err.message().contains("after end_of_stream"),
                "error for {phase:?} should mention 'after end_of_stream', got: {}",
                err.message()
            );
            assert!(
                err.message().contains(expected_name),
                "error for {phase:?} should contain '{expected_name}', got: {}",
                err.message()
            );
        }
    }

    #[test]
    fn eos_tracker_reports_duplicate_regardless_of_flag() {
        let mut tracker = EosTracker::default();

        // Mark EOS
        assert!(tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_ok());

        // A re-delivery is reported as Completed whatever the end_of_stream flag.
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, false).unwrap(),
            PhaseState::Completed
        );
        assert_eq!(
            tracker.check_and_mark(ProtocolPhase::RequestBody, true).unwrap(),
            PhaseState::Completed
        );
    }

    #[test]
    fn headers_complete_in_none_mode_regardless_of_eos() {
        assert!(headers_complete_message(false, BodyMode::None));
        assert!(headers_complete_message(true, BodyMode::None));
        assert!(headers_complete_message(true, BodyMode::Buffered));

        for mode in [BodyMode::Buffered, BodyMode::Streamed, BodyMode::FullDuplexStreamed] {
            assert!(
                !headers_complete_message(false, mode),
                "{mode:?} must wait for the body when end_of_stream is false"
            );
        }
    }

    #[test]
    fn buffered_body_is_complete_without_eos() {
        assert!(body_complete_message(false, BodyMode::Buffered));
        assert!(body_complete_message(true, BodyMode::Buffered));

        for mode in [BodyMode::Streamed, BodyMode::FullDuplexStreamed] {
            assert!(
                !body_complete_message(false, mode),
                "{mode:?} delivers the body in several messages"
            );
            assert!(body_complete_message(true, mode), "{mode:?} completes on end_of_stream");
        }
    }

    #[test]
    fn immediate_from_action_short_circuits_reject_and_terminal() {
        let reject = immediate_from_action(FilterAction::Reject(praxis_filter::Rejection::status(403)));
        assert_eq!(
            reject.and_then(|i| i.status).map(|s| s.code),
            Some(403),
            "a rejection becomes an immediate response"
        );

        let terminal = immediate_from_action(FilterAction::TerminalResponse(Box::new(
            praxis_filter::TerminalResponse::new(200),
        )));
        assert_eq!(
            terminal.and_then(|i| i.status).map(|s| s.code),
            Some(200),
            "a terminal response must end the request at Envoy rather than fall through upstream"
        );

        for action in [FilterAction::Continue, FilterAction::Release, FilterAction::BodyDone] {
            assert!(immediate_from_action(action).is_none(), "non-terminal actions continue");
        }
    }

    #[test]
    fn body_limit_exceeded_only_past_the_ceiling() {
        assert!(
            body_limit_exceeded(4, 4, Some(8), "request").is_none(),
            "exactly at the ceiling is allowed"
        );
        assert!(
            body_limit_exceeded(4, 4, None, "request").is_none(),
            "no ceiling means no rejection"
        );

        let imm = body_limit_exceeded(4, 5, Some(8), "response").unwrap();
        assert_eq!(
            imm.status.map(|s| s.code),
            Some(413),
            "oversized bodies get a local 413"
        );
        assert!(imm.body.contains("response"), "reply names the direction: {}", imm.body);
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
}
