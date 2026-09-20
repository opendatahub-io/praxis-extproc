// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! gRPC [`ExternalProcessor`] implementation for Praxis filter pipelines.
//!
//! Receives Envoy ExtProc messages, translates them into Praxis filter
//! pipeline invocations, and returns header/body mutations or immediate
//! responses.
//!
//! [`ExternalProcessor`]: praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessor

use std::{collections::HashMap, mem, pin::Pin, sync::Arc, time::Instant};

use bytes::Bytes;
use kuadrant_filter::{configuration::PluginConfiguration, filter::DescriptorManager, kuadrant::PipelineFactory};
use praxis_filter::{FilterAction, FilterPipeline, HttpFilterContext, Request, Response};
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        ImmediateResponse, ProcessingRequest, ProcessingResponse, ProtocolConfiguration,
        external_processor_server::ExternalProcessor, processing_request,
    },
};
use tokio::sync::mpsc;
use tokio_stream::{StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request as TonicRequest, Response as TonicResponse, Status, Streaming};
use tracing::{debug, error, warn};

use crate::{
    adapter,
    kuadrant_executor::PolicyStream,
    kuadrant_host::HttpReply,
    kuadrant_transport::{TonicTransport, UpstreamTls},
    metrics,
    response::{self, BodyMode},
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Maximum accumulated body size before rejecting.
const MAX_BODY_ACCUMULATION: usize = 10_485_760; // 10 MiB

/// Channel buffer size for the response stream.
const RESPONSE_CHANNEL_SIZE: usize = 16;

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

impl From<crate::config::BodyModeOverride> for BodyMode {
    fn from(mode: crate::config::BodyModeOverride) -> Self {
        match mode {
            crate::config::BodyModeOverride::Streamed => Self::Streamed,
            crate::config::BodyModeOverride::Buffered => Self::Buffered,
        }
    }
}

/// Body modes a deployment pinned, applied over whatever Envoy conveys.
#[derive(Debug, Clone, Copy, Default)]
struct BodyModeOverrides {
    /// Pinned request body mode, or `None` to use Envoy's.
    request: Option<BodyMode>,
    /// Pinned response body mode, or `None` to use Envoy's.
    response: Option<BodyMode>,
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
    /// Shared filter pipeline.
    pipeline: Arc<FilterPipeline>,

    /// Body modes pinned by config, overriding Envoy's `protocol_config`.
    body_modes: BodyModeOverrides,

    /// Optional Kuadrant policy (Authorino auth + Limitador rate limiting) run
    /// per request. `None` leaves the `ext_proc` as native-filters-only.
    kuadrant: Option<Arc<KuadrantPolicy>>,
}

/// Kuadrant pipeline factory + gRPC transport, shared across streams.
pub struct KuadrantPolicy {
    /// Kuadrant pipeline factory, compiled once from the plugin config so no
    /// request pays config compilation.
    factory: Arc<PipelineFactory>,
    /// gRPC transport dialing the Authorino/Limitador upstreams.
    transport: Arc<TonicTransport>,
}

impl std::fmt::Debug for KuadrantPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // PipelineFactory is not Debug; the compiled blueprints are not useful here.
        f.debug_struct("KuadrantPolicy").finish_non_exhaustive()
    }
}

impl KuadrantPolicy {
    /// Build from a parsed plugin configuration, a cluster-name -> endpoint map,
    /// and per-upstream TLS settings (e.g. the Authorino service CA). Compiles the
    /// pipeline factory once here, off the request path.
    ///
    /// # Errors
    /// Returns an error string if the plugin configuration fails to compile.
    pub fn new(
        config: PluginConfiguration,
        upstreams: HashMap<String, String>,
        tls: HashMap<String, UpstreamTls>,
    ) -> Result<Self, String> {
        let descriptors = Arc::new(DescriptorManager::default());
        let factory =
            PipelineFactory::try_from(config, &descriptors).map_err(|e| format!("compile kuadrant policy: {e:?}"))?;
        let transport = TonicTransport::new(upstreams, tls).map_err(|e| format!("build kuadrant transport: {e}"))?;
        Ok(Self {
            factory: Arc::new(factory),
            transport: Arc::new(transport),
        })
    }
}

/// Flatten Envoy `HeaderValue`s into owned `(key, value)` pairs for the Kuadrant
/// resolver, preferring `raw_value` (binary-safe) over the UTF-8 `value`.
fn kuadrant_header_pairs(headers: &[HeaderValue]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|hv| {
            let value = if hv.raw_value.is_empty() {
                hv.value.clone()
            } else {
                String::from_utf8_lossy(&hv.raw_value).into_owned()
            };
            (hv.key.clone(), value)
        })
        .collect()
}

/// Translate a pipeline [`HttpReply`] into an `ext_proc` [`ImmediateResponse`].
fn to_immediate(reply: HttpReply) -> ImmediateResponse {
    use praxis_proto::envoy::service::common::v3::HttpStatus;
    ImmediateResponse {
        status: Some(HttpStatus {
            code: i32::try_from(reply.status).unwrap_or(500),
        }),
        body: reply
            .body
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default(),
        ..Default::default()
    }
}

/// Run the Kuadrant request-phase check (Authorino auth + Limitador rate-limit).
/// `Some` short-circuits with an immediate denial; `None` continues to native
/// filters, and the same pipeline resumes on the response phases.
///
/// Deploy the `ext_proc` filter with `failure_mode_allow: false`. Transport
/// failures are already mapped to each service's own failureMode, but an
/// executor-thread error surfaces as a stream error, and an in-process filter
/// cannot honor per-service fail-closed once its own executor fails. Envoy's
/// `failure_mode_allow` is then the only backstop, so it must deny.
async fn kuadrant_request_check(
    stream: &PolicyStream,
    envoy_headers: &[HeaderValue],
) -> Result<Option<Vec<ProcessingResponse>>, Status> {
    let decision = stream
        .on_request_headers(kuadrant_header_pairs(envoy_headers))
        .await
        .map_err(Status::internal)?;
    Ok(decision.map(|reply| vec![response::immediate(to_immediate(reply))]))
}

/// Resume the Kuadrant pipeline on the response-headers phase. `Some`
/// short-circuits with an immediate denial; `None` continues.
async fn kuadrant_response_check(
    stream: &PolicyStream,
    envoy_headers: &[HeaderValue],
) -> Result<Option<Vec<ProcessingResponse>>, Status> {
    let decision = stream
        .on_response_headers(kuadrant_header_pairs(envoy_headers))
        .await
        .map_err(Status::internal)?;
    Ok(decision.map(|reply| vec![response::immediate(to_immediate(reply))]))
}

impl PraxisExtProc {
    /// Create a new ExtProc service backed by the given pipeline.
    pub fn new(pipeline: Arc<FilterPipeline>) -> Self {
        Self {
            pipeline,
            body_modes: BodyModeOverrides::default(),
            kuadrant: None,
        }
    }

    /// Pin the request/response body modes from config, so the `ext_proc` uses
    /// them even when Envoy does not send a `protocol_config`.
    #[must_use]
    pub fn with_body_modes(
        mut self,
        request: Option<crate::config::BodyModeOverride>,
        response: Option<crate::config::BodyModeOverride>,
    ) -> Self {
        self.body_modes = BodyModeOverrides {
            request: request.map(Into::into),
            response: response.map(Into::into),
        };
        self
    }

    /// Enable Kuadrant policy enforcement for every stream.
    #[must_use]
    pub fn with_kuadrant(mut self, policy: KuadrantPolicy) -> Self {
        self.kuadrant = Some(Arc::new(policy));
        self
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
        let body_modes = self.body_modes;
        let kuadrant = self.kuadrant.clone();
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(RESPONSE_CHANNEL_SIZE);

        tokio::spawn(async move {
            if let Err(e) = handle_stream(&pipeline, body_modes, kuadrant, &mut inbound, &tx).await {
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
    body_modes: BodyModeOverrides,
    kuadrant: Option<Arc<KuadrantPolicy>>,
    inbound: &mut Streaming<ProcessingRequest>,
    tx: &mpsc::Sender<Result<ProcessingResponse, Status>>,
) -> Result<(), Status> {
    let start = Instant::now();
    let mut stream_state = StreamState::new(body_modes);
    // Spawn a per-stream policy executor: the Kuadrant pipeline is `!Send` and
    // spans request->response, so it lives on its own thread, not in this state.
    stream_state.kuadrant =
        kuadrant.map(|policy| PolicyStream::spawn(Arc::clone(&policy.factory), Arc::clone(&policy.transport)));

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

/// Apply a first-message `protocol_config`, rejecting late deliveries.
///
/// # Errors
///
/// Returns [`Status::invalid_argument`] if `protocol_config` arrives after the
/// first message, or if it requests an unsupported body mode.
fn apply_protocol_config(
    stream_state: &mut StreamState,
    proto_cfg: Option<ProtocolConfiguration>,
    first_message_processed: bool,
) -> Result<(), Status> {
    let Some(proto_cfg) = proto_cfg else {
        return Ok(());
    };
    if first_message_processed {
        metrics::record_invalid_argument("protocol_config", "after_first_message");
        return Err(Status::invalid_argument(
            "protocol_config may only be sent on the first stream message",
        ));
    }
    config_from_first_message(stream_state, proto_cfg)
}

/// Parses `protocol_config` from first message.
///
/// # Errors
///
/// Returns [`Status::invalid_argument`] if unsupported body modes are requested.
fn config_from_first_message(stream_state: &mut StreamState, proto_cfg: ProtocolConfiguration) -> Result<(), Status> {
    stream_state.protocol_config = ProtocolConfig::try_from(proto_cfg).map_err(|m| {
        metrics::record_invalid_argument("protocol_config", "unsupported_mode");
        Status::invalid_argument(m)
    })?;
    stream_state.apply_body_mode_overrides();
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
                    metrics::record_invalid_argument("message_order", "response_before_request_headers");
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
            metrics::record_invalid_argument("message_order", "invalid_phase_transition");
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
            metrics::record_invalid_argument("message_order", "body_after_headers_eos");
            return Err(Status::invalid_argument(format!(
                "received {phase:?} message after headers end_of_stream was already marked"
            )));
        }

        if received_eos {
            match phase {
                ProtocolPhase::RequestHeaders => self.request_headers = PhaseState::Completed,
                ProtocolPhase::RequestBody => self.request_body = PhaseState::Completed,
                ProtocolPhase::ResponseHeaders => self.response_headers = PhaseState::Completed,
                ProtocolPhase::ResponseBody => self.response_body = PhaseState::Completed,
            }
        }

        Ok(PhaseState::Active)
    }
}

// -----------------------------------------------------------------------------
// Phase Handlers
// -----------------------------------------------------------------------------

/// Error for a message re-delivered after its phase already completed.
fn duplicate_after_eos(phase: ProtocolPhase) -> Status {
    metrics::record_invalid_argument("duplicate_eos", "redelivery");
    Status::invalid_argument(format!(
        "received {phase:?} message after end_of_stream was already marked"
    ))
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

    if let Some(stream) = state.kuadrant.as_ref()
        && let Some(reply) = kuadrant_request_check(stream, &envoy_headers).await?
    {
        return Ok(reply);
    }

    if headers.end_of_stream {
        return run_request_pipeline(RequestPhase::Headers, pipeline, state).await;
    }

    match state.protocol_config.request_body_mode {
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
    check_body_limit(state.request_body.len(), body.body.len())?;
    state.request_body.extend_from_slice(&body.body);

    if !body.end_of_stream {
        return Ok(Vec::new());
    }

    run_request_pipeline(RequestPhase::Body, pipeline, state).await
}

/// Handle response headers: run response filters and respond with mutations.
///
/// For `BUFFERED`, runs filters early and defers mutations to body phase
/// (Envoy honours `CommonResponse.header_mutation` on body responses).
/// For `STREAMED`, runs filters early and sends mutations immediately
/// (Envoy ignores header mutations on body responses for non-`BUFFERED`).
/// For `FDS` with body filters, returns no response — full pipeline at body EOS.
/// For `FDS` passthrough, runs filters early, defers mutations to first chunk.
#[expect(
    clippy::large_stack_frames,
    reason = "StreamState carries HttpFilterContext fields grown in praxis 0.5.4"
)]
async fn handle_response_headers(
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

    if let Some(stream) = state.kuadrant.as_ref()
        && let Some(reply) = kuadrant_response_check(stream, &envoy_headers).await?
    {
        return Ok(reply);
    }

    if headers.end_of_stream {
        return run_response_pipeline(ResponsePhase::Headers, pipeline, state).await;
    }

    match state.protocol_config.response_body_mode {
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

    match route_response_body(mode, needs_body, state.kuadrant.is_some()) {
        ResponseBodyRoute::Passthrough => Ok(passthrough_chunk(&body, state, mode, false)),
        ResponseBodyRoute::Streamed => process_streamed_body_chunk(pipeline, body, state, false).await,
        ResponseBodyRoute::Accumulate => accumulate_response_body(pipeline, body, state).await,
    }
}

/// Where a response-body chunk is routed.
#[derive(Debug, PartialEq, Eq)]
enum ResponseBodyRoute {
    /// Emit the chunk without running body filters or the Kuadrant executor.
    Passthrough,
    /// Per-chunk path: run streamed body filters and feed the Kuadrant executor
    /// (which reports token usage at end-of-stream) without buffering the body.
    Streamed,
    /// Buffer to end-of-stream, then run the pipeline / Kuadrant report.
    Accumulate,
}

/// Decide how to route a response-body chunk. Native routing is unchanged; when
/// Kuadrant is enabled it must see every chunk so the token report fires, so a
/// route that would skip it (passthrough) is upgraded to a body-aware one. Left
/// as passthrough, the Limitador debit would be a silent no-op on the common
/// streamed-response shape with no native body filter.
fn route_response_body(mode: BodyMode, needs_body: bool, kuadrant: bool) -> ResponseBodyRoute {
    let native = match (mode, needs_body) {
        (BodyMode::Streamed | BodyMode::FullDuplexStreamed, false) => ResponseBodyRoute::Passthrough,
        (BodyMode::Streamed, true) => ResponseBodyRoute::Streamed,
        _ => ResponseBodyRoute::Accumulate,
    };
    match (kuadrant, &native, mode) {
        // Kuadrant on but native chose passthrough: stream the chunks so the token
        // report fires at eos. Passthrough only arises for the streamed modes, so
        // this covers every passthrough case.
        (true, ResponseBodyRoute::Passthrough, BodyMode::Streamed | BodyMode::FullDuplexStreamed) => {
            ResponseBodyRoute::Streamed
        },
        _ => native,
    }
}

/// Accumulate response body chunks, run full pipeline on EOS.
async fn accumulate_response_body(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
) -> Result<Vec<ProcessingResponse>, Status> {
    check_body_limit(state.response_body.len(), body.body.len())?;
    state.response_body.extend_from_slice(&body.body);

    if !body.end_of_stream {
        return Ok(Vec::new());
    }

    // Resume the Kuadrant pipeline on the response-body phase: the token-usage
    // task parses the completion and reports consumption to Limitador (the
    // rate-limit debit). Buffered mode is one chunk with end_of_stream = true.
    if let Some(stream) = state.kuadrant.as_ref() {
        let response_body = state.response_body.clone();
        match stream.on_response_body(response_body, true).await {
            Ok(Some(reply)) => return Ok(vec![response::immediate(to_immediate(reply))]),
            Ok(None) => {},
            // Fail open on the response phase: the token report is a post-hoc debit
            // and the response is already complete, so an executor error must not
            // reset it. Request-phase auth stays fail-closed.
            Err(e) => warn!(error = %e, "kuadrant response report failed; allowing response"),
        }
    }

    run_response_pipeline(ResponsePhase::Body, pipeline, state).await
}

// -----------------------------------------------------------------------------
// Pipeline Execution
// -----------------------------------------------------------------------------

/// Request filter execution phase.
#[derive(Debug, Clone, Copy)]
enum RequestPhase {
    /// Headers phase (headers EOS=true).
    Headers,
    /// Body phase (body EOS=true).
    Body,
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
        metrics::record_invalid_argument("missing_headers", "request");
        return Err(Status::invalid_argument("request headers not received"));
    };
    let mut ctx = adapter::build_filter_context(pipeline, request);

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = check_reject(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    let original_len = state.request_body.len();
    let body_reject = run_body_filters(pipeline, &mut ctx, &mut state.request_body, true).await?;
    if let Some(imm) = body_reject {
        return Ok(vec![response::immediate(imm)]);
    }

    let mutation = adapter::collect_request_header_mutations(&ctx);

    state.executed_filter_indices = mem::take(&mut ctx.executed_filter_indices);
    state.branch_iterations = mem::take(&mut ctx.branch_iterations);
    state.filter_metadata = mem::take(&mut ctx.filter_metadata);
    state.filter_state = mem::take(&mut ctx.filter_state);

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
    /// Body phase (response body EOS=true).
    Body,
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
        metrics::record_invalid_argument("missing_headers", "request");
        return Err(Status::invalid_argument("request headers not received"));
    };

    let mut resp = state.response.take().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "response");
        Status::invalid_argument("response headers not received")
    })?;

    let mut ctx = adapter::build_filter_context(pipeline, request);
    state.restore_request_ctx(&mut ctx);
    ctx.filter_state = mem::take(&mut state.filter_state);
    let original_headers = capture_original_headers(&resp);
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

    // Persist filter state so a later response phase sees it. Without this, the
    // response-header phase's metadata (e.g. token_count's SSE/JSON mode) is
    // lost before the body phase, and body filters that read it do nothing.
    state.filter_metadata.clone_from(&ctx.filter_metadata);
    state.executed_filter_indices.clone_from(&ctx.executed_filter_indices);
    state.branch_iterations.clone_from(&ctx.branch_iterations);

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
) -> Result<Option<ImmediateResponse>, Status> {
    let should_execute = match phase {
        ResponsePhase::Headers => true,
        ResponsePhase::Body => !filters_executed,
    };

    if should_execute {
        let action = execute_response(pipeline, ctx).await?;
        if let Some(imm) = check_reject(action) {
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
async fn process_streamed_body_chunk(
    pipeline: &FilterPipeline,
    body: praxis_proto::envoy::service::ext_proc::v3::HttpBody,
    state: &mut StreamState,
    is_request: bool,
) -> Result<Vec<ProcessingResponse>, Status> {
    // Kuadrant token report over a STREAMED response: feed each chunk to the
    // per-stream executor. The crate accumulates chunks and the report/debit
    // fires on `end_of_stream`. Content chunks return quickly (no gRPC), so this
    // does not stall the stream. Done before the `&mut state.response` borrow.
    let kuadrant_reply = match state.kuadrant.as_ref() {
        Some(kstream) if !is_request => match kstream.on_response_body(body.body.clone(), body.end_of_stream).await {
            Ok(reply) => reply,
            // Fail open on the response phase: the token report is a post-hoc debit
            // and chunks are already streaming, so an executor error must not reset
            // an in-flight response. Request-phase auth stays fail-closed.
            Err(e) => {
                warn!(error = %e, "kuadrant response report failed; allowing response");
                None
            },
        },
        _ => None,
    };
    if let Some(reply) = kuadrant_reply {
        return Ok(vec![response::immediate(to_immediate(reply))]);
    }

    let request = state.request.as_ref().ok_or_else(|| {
        metrics::record_invalid_argument("missing_headers", "request");
        Status::invalid_argument("request headers not received")
    })?;
    let mut ctx = adapter::build_filter_context(pipeline, request);
    state.restore_request_ctx(&mut ctx);
    ctx.filter_state = mem::take(&mut state.filter_state);
    if !is_request {
        let resp = state.response.as_mut().ok_or_else(|| {
            metrics::record_invalid_argument("missing_headers", "response");
            Status::invalid_argument("response headers not received")
        })?;
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
    state.executed_filter_indices = mem::take(&mut ctx.executed_filter_indices);
    state.branch_iterations = mem::take(&mut ctx.branch_iterations);
    state.filter_metadata = mem::take(&mut ctx.filter_metadata);
    state.filter_state = mem::take(&mut ctx.filter_state);
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
    let mut ctx = adapter::build_filter_context(pipeline, request);

    let action = execute_request(pipeline, &mut ctx).await?;
    if let Some(imm) = check_reject(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    state.executed_filter_indices = mem::take(&mut ctx.executed_filter_indices);
    state.branch_iterations = mem::take(&mut ctx.branch_iterations);
    state.filter_metadata = mem::take(&mut ctx.filter_metadata);
    state.filter_state = mem::take(&mut ctx.filter_state);
    let mutation = adapter::collect_request_header_mutations(&ctx);

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

    let mut ctx = adapter::build_filter_context(pipeline, request);
    state.restore_request_ctx(&mut ctx);
    ctx.filter_state = mem::take(&mut state.filter_state);

    let Some(resp) = state.response.as_mut() else {
        return Ok(delivery.deliver_response(None, state));
    };

    let original_headers = capture_original_headers(resp);
    ctx.response_header = Some(resp);

    let action = execute_response(pipeline, &mut ctx).await?;
    if let Some(imm) = check_reject(action) {
        return Ok(vec![response::immediate(imm)]);
    }

    state.header_state.response_filters_executed = true;
    // Move filter_state back so the response-body phase's fresh ctx still sees it.
    state.filter_state = mem::take(&mut ctx.filter_state);
    let mutation = adapter::collect_response_header_mutations_diff(&ctx, &original_headers);

    Ok(delivery.deliver_response(mutation, state))
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
fn check_reject(action: FilterAction) -> Option<ImmediateResponse> {
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
    eos: bool,
) -> Result<Option<ImmediateResponse>, Status> {
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
) -> Result<Option<ImmediateResponse>, Status> {
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
#[derive(Debug, Default)]
struct StreamState {
    /// Re-entrance counters from request-phase branch chains.
    branch_iterations: HashMap<Arc<str>, u32>,

    /// Executed filter indices from request phase.
    executed_filter_indices: Vec<bool>,

    /// Metadata carried from request to response phase.
    filter_metadata: HashMap<String, String>,

    /// Typed per-filter state carried from request to response phase.
    filter_state: HashMap<usize, Box<dyn std::any::Any + Send + Sync>>,

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

    /// Body modes pinned by config, re-applied over Envoy's `protocol_config`.
    body_mode_overrides: BodyModeOverrides,

    /// Deferred request header mutation for FDS passthrough mode.
    deferred_request_header_mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,

    /// Deferred response header mutation for BUFFERED or FDS passthrough mode.
    deferred_response_header_mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,

    /// Per-direction phase ordering guard.
    phase_order: PhaseOrderTracker,

    /// Per-stream Kuadrant policy executor, if enabled. Owns the `!Send`,
    /// cross-phase pipeline on its own thread.
    kuadrant: Option<PolicyStream>,
}

impl StreamState {
    /// Create a new empty stream state, applying any pinned body modes.
    fn new(body_mode_overrides: BodyModeOverrides) -> Self {
        let mut state = Self {
            protocol_config: ProtocolConfig::default(),
            body_mode_overrides,
            ..Default::default()
        };
        state.apply_body_mode_overrides();
        state
    }

    /// Apply the config-pinned body modes over `protocol_config`, so they win
    /// whether or not Envoy sent a `protocol_config`.
    fn apply_body_mode_overrides(&mut self) {
        if let Some(mode) = self.body_mode_overrides.request {
            self.protocol_config.request_body_mode = mode;
        }
        if let Some(mode) = self.body_mode_overrides.response {
            self.protocol_config.response_body_mode = mode;
        }
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
        metrics::record_body_size_rejection();
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
    fn kuadrant_routes_streamed_response_body_off_passthrough() {
        // Regression: with Kuadrant enabled and no native body filter, a streamed
        // response body must reach the executor (so the token report fires),
        // never passthrough (which would make the Limitador debit a silent
        // no-op). Native routing is unchanged when Kuadrant is off.
        use ResponseBodyRoute::{Accumulate, Passthrough, Streamed};
        // (mode, needs_native_body, kuadrant) -> route.
        assert_eq!(route_response_body(BodyMode::Streamed, false, false), Passthrough);
        assert_eq!(route_response_body(BodyMode::Streamed, false, true), Streamed);
        assert_eq!(route_response_body(BodyMode::FullDuplexStreamed, false, true), Streamed);
        assert_eq!(route_response_body(BodyMode::Buffered, false, true), Accumulate);
        // Native routing is unchanged when Kuadrant is off.
        assert_eq!(route_response_body(BodyMode::Streamed, true, false), Streamed);
        assert_eq!(
            route_response_body(BodyMode::FullDuplexStreamed, true, false),
            Accumulate
        );
    }

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

    /// Value of the counter `name` carrying every `labels` pair (0 if absent).
    fn snapshot_counter(
        snapshotter: &metrics_util::debugging::Snapshotter,
        name: &str,
        labels: &[(&str, &str)],
    ) -> u64 {
        use metrics_util::debugging::DebugValue;

        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(composite, _unit, _desc, value)| {
                let key = composite.key();
                let matches = key.name() == name
                    && labels
                        .iter()
                        .all(|(k, v)| key.labels().any(|l| l.key() == *k && l.value() == *v));
                match value {
                    DebugValue::Counter(count) if matches => Some(count),
                    _ => None,
                }
            })
            .unwrap_or(0)
    }

    /// Run sync `f` under a thread-local recorder and read back counter `name`/`labels`.
    fn counter_value(name: &str, labels: &[(&str, &str)], f: impl FnOnce()) -> u64 {
        use metrics_util::debugging::DebuggingRecorder;

        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        {
            // `::metrics` disambiguates the external crate from this crate's `metrics` module.
            let _guard = ::metrics::set_default_local_recorder(&recorder);
            f();
        }
        snapshot_counter(&snapshotter, name, labels)
    }

    /// Count `invalid_argument_total` increments matching `reason`/`detail` while `f` runs.
    fn invalid_arg_count(reason: &str, detail: &str, f: impl FnOnce()) -> u64 {
        counter_value(
            "praxis_extproc_invalid_argument_total",
            &[("reason", reason), ("detail", detail)],
            f,
        )
    }

    #[test]
    fn apply_protocol_config_after_first_message_records_metric() {
        let mut state = StreamState::new(BodyModeOverrides::default());
        let count = invalid_arg_count("protocol_config", "after_first_message", || {
            let result = apply_protocol_config(&mut state, Some(ProtocolConfiguration::default()), true);
            assert!(
                matches!(&result, Err(status) if status.code() == tonic::Code::InvalidArgument),
                "late protocol_config must be rejected with invalid_argument"
            );
        });
        assert_eq!(count, 1, "late delivery must increment after_first_message");
    }

    #[test]
    fn config_from_first_message_unsupported_mode_records_metric() {
        let mut state = StreamState::new(BodyModeOverrides::default());
        // 3 == BUFFERED_PARTIAL, an unsupported body mode.
        let bad = ProtocolConfiguration {
            request_body_mode: 3,
            ..ProtocolConfiguration::default()
        };
        let count = invalid_arg_count("protocol_config", "unsupported_mode", || {
            assert!(
                config_from_first_message(&mut state, bad).is_err(),
                "unsupported mode must be rejected"
            );
        });
        assert_eq!(count, 1, "unsupported mode must increment unsupported_mode");
    }

    #[test]
    fn phase_order_response_before_request_headers_records_metric() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpHeaders;
        use processing_request::Request;

        let mut tracker = PhaseOrderTracker::default();
        let count = invalid_arg_count("message_order", "response_before_request_headers", || {
            assert!(
                tracker
                    .check_and_advance(&Request::ResponseHeaders(HttpHeaders::default()))
                    .is_err(),
                "response before request headers must be rejected"
            );
        });
        assert_eq!(count, 1, "response-before-request-headers must increment the counter");
    }

    #[test]
    fn phase_order_invalid_transition_records_metric() {
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

        let count = invalid_arg_count("message_order", "invalid_phase_transition", || {
            assert!(
                tracker
                    .check_and_advance(&Request::RequestBody(HttpBody::default()))
                    .is_err(),
                "RequestBody after RequestTrailers must be rejected"
            );
        });
        assert_eq!(count, 1, "invalid phase transition must increment the counter");
    }

    #[test]
    fn eos_body_after_headers_records_metric() {
        let mut tracker = EosTracker::default();
        assert!(tracker.check_and_mark(ProtocolPhase::RequestHeaders, true).is_ok());

        let count = invalid_arg_count("message_order", "body_after_headers_eos", || {
            assert!(
                tracker.check_and_mark(ProtocolPhase::RequestBody, true).is_err(),
                "body after headers EOS must be rejected"
            );
        });
        assert_eq!(count, 1, "body-after-headers-eos must increment the counter");
    }

    #[test]
    fn duplicate_after_eos_records_metric() {
        let count = invalid_arg_count("duplicate_eos", "redelivery", || {
            let err = duplicate_after_eos(ProtocolPhase::RequestBody);
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        });
        assert_eq!(count, 1, "duplicate-eos redelivery must increment the counter");
    }

    #[test]
    fn check_body_limit_rejection_records_metric() {
        let count = counter_value("praxis_extproc_body_size_rejections_total", &[], || {
            assert!(
                check_body_limit(MAX_BODY_ACCUMULATION, 1).is_err(),
                "exceeding the body limit must be rejected"
            );
        });
        assert_eq!(count, 1, "body-size rejection must increment the counter");
    }

    #[tokio::test]
    async fn run_request_pipeline_missing_headers_records_metric() {
        use praxis_filter::FilterRegistry;

        let pipeline = FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).unwrap();
        let mut state = StreamState::new(BodyModeOverrides::default());

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
        let mut state = StreamState::new(BodyModeOverrides::default());
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

    #[tokio::test]
    async fn response_phase_report_error_fails_open_not_reset() {
        use praxis_filter::FilterRegistry;

        use crate::kuadrant_executor::PolicyStream;

        let pipeline = FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).unwrap();
        let mut state = StreamState::new(BodyModeOverrides::default());
        // Request headers present so the fall-through reaches the response branch.
        state.request = Some(adapter::envoy_headers_to_request(&[]));
        // A dead Kuadrant executor makes on_response_body error at end-of-stream.
        state.kuadrant = Some(PolicyStream::dead());

        let body = praxis_proto::envoy::service::ext_proc::v3::HttpBody {
            body: b"{}".to_vec(),
            end_of_stream: true,
        };
        let result = accumulate_response_body(&pipeline, body, &mut state).await;
        // Fail open: the report error must not reset the response with an internal
        // status. It falls through to the pipeline, which here errs only because no
        // response headers were set (invalid_argument). The pre-fix `?` returned
        // internal.
        assert!(
            result.is_err(),
            "pipeline still runs after fail-open (no response headers set)"
        );
        assert_eq!(
            result.unwrap_err().code(),
            tonic::Code::InvalidArgument,
            "response-phase report error must fail open, not reset the stream with an internal status"
        );
    }

    /// Typed marker a probe filter stashes on request and reads on response.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    struct Probe(u64);
    /// Sentinel value carried through `filter_state`.
    const PROBE_VALUE: u64 = 0x00C0_FFEE;
    /// Value the probe observed in `on_response` (0 if state was lost).
    static PROBE_OBSERVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
            Ok(FilterAction::Continue)
        }

        async fn on_response(
            &self,
            ctx: &mut HttpFilterContext<'_>,
        ) -> Result<FilterAction, praxis_filter::FilterError> {
            let observed = ctx.get_filter_state::<Probe>().map_or(0, |p| p.0);
            PROBE_OBSERVED.store(observed, std::sync::atomic::Ordering::SeqCst);
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

    #[tokio::test]
    async fn filter_state_survives_request_to_response_phase() {
        use std::sync::atomic::Ordering;

        use praxis_filter::FilterRegistry;

        PROBE_OBSERVED.store(0, Ordering::SeqCst);
        let cfg: crate::config::ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: state_probe\n")
                .unwrap();
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register("state_probe", praxis_filter::http_builtin(ProbeFilter::from_config))
            .unwrap();
        let pipeline = crate::config::build_pipeline(&cfg, &registry).unwrap();
        let mut state = StreamState::new(BodyModeOverrides::default());
        state.request = Some(adapter::envoy_headers_to_request(&[]));

        // Request phase stores state; it must be moved out into StreamState.
        run_request_pipeline(RequestPhase::Headers, &pipeline, &mut state)
            .await
            .unwrap();
        assert!(
            state.filter_state.contains_key(&0),
            "request-phase filter_state must persist into StreamState"
        );

        // Response phase builds a fresh ctx; state must be moved back in.
        state.response = Some(adapter::envoy_headers_to_response(&[]));
        run_response_pipeline(ResponsePhase::Headers, &pipeline, &mut state)
            .await
            .unwrap();
        assert_eq!(
            PROBE_OBSERVED.load(Ordering::SeqCst),
            PROBE_VALUE,
            "on_response must see state stored in on_request; 0 means it was dropped at the phase boundary"
        );
    }
}
