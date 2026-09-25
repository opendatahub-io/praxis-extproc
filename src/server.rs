// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! gRPC [`ExternalProcessor`] implementation for Praxis filter pipelines.
//!
//! Receives Envoy ExtProc messages, translates them into Praxis filter
//! pipeline invocations, and returns header/body mutations or immediate
//! responses.
//!
//! The message-level work is split across sibling modules: `protocol`
//! enforces sequencing and parses protocol configuration, `handlers`
//! owns each protocol phase, and `pipeline` executes filters and builds
//! responses. This module holds the gRPC service, the per-stream loop, the
//! shared `StreamState`, and message dispatch.
//!
//! [`ExternalProcessor`]: praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessor

use std::{collections::HashMap, pin::Pin, sync::Arc, time::Instant};

use praxis_filter::{FilterPipeline, HttpFilterContext, Request, Response};
use praxis_proto::envoy::service::ext_proc::v3::{
    ProcessingRequest, ProcessingResponse, ProtocolConfiguration, external_processor_server::ExternalProcessor,
    processing_request,
};
use tokio::sync::{mpsc, watch};
use tokio_stream::{StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request as TonicRequest, Response as TonicResponse, Status, Streaming};
use tracing::{debug, error, warn};

use crate::{
    handlers::{
        handle_request_body, handle_request_headers, handle_response_body, handle_response_headers,
        local_reply_passthrough,
    },
    metrics,
    protocol::{EosTracker, PhaseOrderTracker, ProtocolConfig, request_type_label, validate_body_message},
    response,
};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Channel buffer size for the response stream.
const RESPONSE_CHANNEL_SIZE: usize = 16;

// -----------------------------------------------------------------------------
// PraxisExtProc
// -----------------------------------------------------------------------------

/// Output stream type for the `Process` RPC.
type ProcessStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<ProcessingResponse, Status>> + Send>>;

/// Praxis ExtProc gRPC service.
///
/// Holds a shared [`FilterPipeline`] and executes it for each
/// incoming gRPC stream.
///
/// [`FilterPipeline`]: praxis_filter::FilterPipeline
pub struct PraxisExtProc {
    /// Shared filter pipeline.
    pipeline: Arc<FilterPipeline>,
    /// Latch flipped to `true` when the shutdown drain deadline expires,
    /// signalling in-flight streams to cancel forcefully.
    force_shutdown: watch::Receiver<bool>,
    /// Effective body-accumulation ceiling in bytes; `None` means unbounded.
    max_body_accumulation: Option<usize>,
}

impl PraxisExtProc {
    /// Create a new ExtProc service backed by the given pipeline.
    ///
    /// No drain deadline is wired: the force-shutdown latch never fires. Use
    /// [`with_force_shutdown`](Self::with_force_shutdown) to arm one. The
    /// body-accumulation ceiling defaults to the built-in 10 MiB
    /// (`config::DEFAULT_MAX_BODY_BYTES`); use
    /// [`with_max_body_accumulation`](Self::with_max_body_accumulation) to set
    /// the configured effective limit.
    pub fn new(pipeline: Arc<FilterPipeline>) -> Self {
        let (_tx, rx) = watch::channel(false); // `_tx` dropped => latch never fires
        Self {
            pipeline,
            force_shutdown: rx,
            max_body_accumulation: Some(crate::config::DEFAULT_MAX_BODY_BYTES),
        }
    }

    /// Wire a force-shutdown latch driven by the drain deadline.
    #[must_use]
    pub fn with_force_shutdown(mut self, rx: watch::Receiver<bool>) -> Self {
        self.force_shutdown = rx;
        self
    }

    /// Set the effective body-accumulation ceiling; `None` disables bounding.
    #[must_use]
    pub fn with_max_body_accumulation(mut self, limit: Option<usize>) -> Self {
        self.max_body_accumulation = limit;
        self
    }
}

/// Resolve only when the latch flips to `true`.
///
/// A closed channel (no drain deadline wired) stays pending forever, so the
/// caller's other `select!` branch always wins in that case.
async fn wait_force(mut rx: watch::Receiver<bool>) {
    while rx.changed().await.is_ok() {
        if *rx.borrow() {
            return;
        }
    }
    std::future::pending::<()>().await;
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
        let force = self.force_shutdown.clone();
        let max_body = self.max_body_accumulation;
        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel(RESPONSE_CHANNEL_SIZE);

        tokio::spawn(async move {
            tokio::select! {
                r = Box::pin(handle_stream(&pipeline, &mut inbound, &tx, max_body)) => {
                    if let Err(e) = r {
                        error!(error = %e, "stream processing failed");
                        drop(tx.send(Err(e)).await);
                    }
                }
                () = wait_force(force) => {
                    // Best-effort notification: never block on a full channel. The
                    // deadline race in the binary drops the serving future, which
                    // force-closes the connection regardless of delivery.
                    warn!("drain deadline exceeded; forcefully cancelling stream");
                    drop(tx.try_send(Err(Status::unavailable("server shutting down"))));
                }
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
    max_body: Option<usize>,
) -> Result<(), Status> {
    let start = Instant::now();
    let mut stream_state = StreamState::new();
    stream_state.max_body_accumulation = max_body;

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

        // Boxed: the dispatch future carries a full filter context, which would
        // otherwise put this loop's frame past the large-stack-frames threshold.
        let responses = Box::pin(dispatch_request(pipeline, req, stream_state)).await?;
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

// -----------------------------------------------------------------------------
// Protocol Configuration
// -----------------------------------------------------------------------------

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
    debug!(
        request_mode = ?stream_state.protocol_config.request_body_mode,
        response_mode = ?stream_state.protocol_config.response_body_mode,
        "ExtProc protocol configuration received from Envoy"
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// Dispatch
// -----------------------------------------------------------------------------

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
    validate_body_message(&req, &state.protocol_config)?;
    state.phase_order.check_and_advance(&req)?;
    if state.phase_order.local_reply() {
        return local_reply_passthrough(&req, state);
    }

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
// StreamState
// -----------------------------------------------------------------------------

/// Tracks header response delivery and filter execution across phases.
#[derive(Debug, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent per-direction flags, not a state machine"
)]
pub(crate) struct HeaderDeliveryState {
    /// Whether response-phase filters already ran at header time.
    pub(crate) response_filters_executed: bool,

    /// Whether the deferred request `HeadersResponse` has been sent.
    pub(crate) request_headers_sent: bool,

    /// Whether the deferred response `HeadersResponse` has been sent.
    pub(crate) response_headers_sent: bool,
}

impl HeaderDeliveryState {
    /// Mark direction as sent; returns `true` on first call per direction.
    pub(crate) fn take_first_chunk(&mut self, is_request: bool) -> bool {
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

/// Cross-phase filter-context state parked between ExtProc phases.
///
/// A fresh [`HttpFilterContext`] is built per phase, so these fields cross the
/// boundary as two linear moves: [`HydratedContext::hydrate`] pours the parked state
/// into a fresh context at phase start, and [`HydratedContext::dehydrate`] captures it
/// back out at phase end. The stream state parks this in an `Option` slot moved out
/// to hydrate each phase's context and moved back on capture: a phase that forgets to
/// restore its state leaves the slot `None`, which the next `hydrate` reports as an
/// error instead of silently carrying an empty context. `#[must_use]` flags parked
/// state that is dropped instead of hydrated.
#[must_use = "parked cross-phase state must be hydrated into a context"]
#[derive(Debug, Default)]
pub(super) struct CarriedContext {
    /// Branch re-entrance counters.
    pub(super) branch_iterations: HashMap<Arc<str>, u32>,

    /// Filter indices executed in earlier phases.
    pub(super) executed_filter_indices: Vec<bool>,

    /// Flat string metadata.
    pub(super) filter_metadata: HashMap<String, String>,

    /// Typed per-filter state.
    pub(super) filter_state: HashMap<usize, Box<dyn std::any::Any + Send + Sync>>,
}

/// A context holding hydrated cross-phase state.
///
/// [`HydratedContext::hydrate`] is the only constructor and the `ctx` field is
/// private, so there is no bare context to capture out of by mistake. Deref
/// exposes it as a plain [`HttpFilterContext`] so filter code is unchanged, and
/// consuming `self` in [`HydratedContext::dehydrate`] makes a second capture a
/// compile error.
#[must_use = "a hydrated context must be dehydrated back into CarriedContext"]
pub(super) struct HydratedContext<'a> {
    /// The context holding hydrated cross-phase state.
    ctx: HttpFilterContext<'a>,
}

impl<'a> HydratedContext<'a> {
    /// Pour parked state into a fresh context at phase start. A `None` argument
    /// means a prior phase never restored its state, so it surfaces as an error
    /// rather than silently hydrating an empty context. Owning the context by value
    /// means the bare context is consumed, so it cannot be hydrated a second time.
    pub(super) fn hydrate(carried: Option<CarriedContext>, mut ctx: HttpFilterContext<'a>) -> Result<Self, Status> {
        let carried =
            carried.ok_or_else(|| Status::internal("cross-phase context missing: a prior phase did not restore it"))?;
        ctx.branch_iterations = carried.branch_iterations;
        ctx.executed_filter_indices = carried.executed_filter_indices;
        ctx.filter_metadata = carried.filter_metadata;
        ctx.filter_state = carried.filter_state;
        Ok(Self { ctx })
    }

    /// Capture cross-phase state back into `slot` at phase end, consuming the
    /// hydrated context so it cannot be captured twice. The slot must be empty:
    /// [`hydrate`] drained it at phase start, so a `Some` slot means a prior
    /// write-back was never drained. That surfaces as an error rather than
    /// silently overwriting parked state.
    ///
    /// Takes the [`StreamState::carried_context`] slot rather than `&mut
    /// StreamState`: the hydrated context still borrows `state.request` /
    /// `state.response`, so only a disjoint-field borrow of the slot is
    /// available at the call site.
    ///
    /// [`hydrate`]: HydratedContext::hydrate
    pub(super) fn dehydrate(self, slot: &mut Option<CarriedContext>) -> Result<(), Status> {
        if slot.is_some() {
            return Err(Status::internal(
                "cross-phase context already present: this phase did not drain it before capture",
            ));
        }
        *slot = Some(CarriedContext {
            branch_iterations: self.ctx.branch_iterations,
            executed_filter_indices: self.ctx.executed_filter_indices,
            filter_metadata: self.ctx.filter_metadata,
            filter_state: self.ctx.filter_state,
        });
        Ok(())
    }
}

impl<'a> std::ops::Deref for HydratedContext<'a> {
    type Target = HttpFilterContext<'a>;

    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl std::ops::DerefMut for HydratedContext<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}

/// Per-stream state accumulated across ExtProc phases.
#[derive(Debug)]
pub(crate) struct StreamState {
    /// Filter-context state carried across phase boundaries.
    ///
    /// Moved out to hydrate each phase's context and refilled on capture; a `None`
    /// between phases means a phase failed to restore its state.
    pub(crate) carried_context: Option<CarriedContext>,

    /// Converted request from the headers phase.
    pub(crate) request: Option<Request>,

    /// Accumulated request body bytes.
    pub(crate) request_body: Vec<u8>,

    /// Converted response from the response headers phase.
    pub(crate) response: Option<Response>,

    /// Accumulated response body bytes.
    pub(crate) response_body: Vec<u8>,

    /// Header delivery tracking across phases.
    pub(crate) header_state: HeaderDeliveryState,

    /// End-of-stream tracking for protocol safety.
    pub(crate) eos_tracker: EosTracker,

    /// Protocol configuration parsed from Envoy's first message.
    pub(crate) protocol_config: ProtocolConfig,

    /// Deferred request header mutation for FDS passthrough mode.
    pub(crate) deferred_request_header_mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,

    /// Deferred response header mutation for BUFFERED or FDS passthrough mode.
    pub(crate) deferred_response_header_mutation: Option<praxis_proto::envoy::service::ext_proc::v3::HeaderMutation>,

    /// Per-direction phase ordering guard.
    pub(crate) phase_order: PhaseOrderTracker,

    /// Effective body-accumulation ceiling in bytes; `None` means unbounded.
    pub(crate) max_body_accumulation: Option<usize>,
}

impl Default for StreamState {
    /// The carried-context slot is seeded present, not `None`: `None` is reserved
    /// for a slot a phase drained without restoring, so both constructors must start
    /// it as `Some` to keep that signal meaningful.
    fn default() -> Self {
        Self {
            carried_context: Some(CarriedContext::default()),
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
            max_body_accumulation: None,
        }
    }
}

impl StreamState {
    /// Create a new empty stream state with default protocol configuration.
    ///
    /// The body-accumulation ceiling defaults to the built-in 10 MiB
    /// ([`crate::config::DEFAULT_MAX_BODY_BYTES`]); [`handle_stream`] overrides
    /// it with the configured effective limit.
    pub(crate) fn new() -> Self {
        Self {
            max_body_accumulation: Some(crate::config::DEFAULT_MAX_BODY_BYTES),
            ..Default::default()
        }
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use praxis_filter::FilterAction;

    use super::*;
    use crate::{response::BodyMode, test_support::invalid_arg_count};

    #[test]
    fn apply_protocol_config_after_first_message_records_metric() {
        let mut state = StreamState::new();
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
        let mut state = StreamState::new();
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

    /// Counts body-filter execution so malformed body messages can prove they
    /// are rejected before the pipeline is entered.
    static BODY_FILTER_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    struct BodyExecutionProbe;

    #[async_trait::async_trait]
    impl praxis_filter::HttpFilter for BodyExecutionProbe {
        fn name(&self) -> &'static str {
            "body_execution_probe"
        }

        async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            BODY_FILTER_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(FilterAction::Continue)
        }

        async fn on_response(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, praxis_filter::FilterError> {
            BODY_FILTER_RUNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(FilterAction::Continue)
        }
    }

    impl BodyExecutionProbe {
        /// Registry factory for malformed-stream tests.
        #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
        fn from_config(
            _: &serde_yaml::Value,
        ) -> Result<Box<dyn praxis_filter::HttpFilter>, praxis_filter::FilterError> {
            Ok(Box::new(Self))
        }
    }

    fn body_probe_pipeline() -> Arc<FilterPipeline> {
        let cfg: crate::config::ExtProcConfig = serde_yaml::from_str(
            "filter_chains:\n  - name: main\n    filters:\n      - filter: body_execution_probe\n",
        )
        .unwrap();
        let mut registry = praxis_filter::FilterRegistry::with_builtins();
        registry
            .register(
                "body_execution_probe",
                praxis_filter::http_builtin(BodyExecutionProbe::from_config),
            )
            .unwrap();
        crate::config::build_pipeline(&cfg, &registry).unwrap()
    }

    #[tokio::test]
    async fn request_body_in_none_mode_is_rejected_before_tracking_or_filters() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;
        use processing_request::Request;

        BODY_FILTER_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        let pipeline = body_probe_pipeline();
        let mut state = StreamState::new();
        state.protocol_config.request_body_mode = BodyMode::None;

        let result = dispatch_request(
            &pipeline,
            Request::RequestBody(HttpBody {
                body: b"unexpected".to_vec(),
                end_of_stream: true,
            }),
            &mut state,
        )
        .await;

        assert!(result.is_err(), "RequestBody in NONE mode must be rejected");
        if let Err(error) = result {
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(error.message().contains("RequestBody"));
        }
        assert!(!state.eos_tracker.request_body.is_complete(), "EOS must not be tracked");
        assert!(state.request_body.is_empty(), "body must not be accumulated");
        assert_eq!(BODY_FILTER_RUNS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn local_reply_passes_through_buffered_response_without_filters() {
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response;

        BODY_FILTER_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        let messages = vec![
            local_reply_headers_message(false),
            local_reply_body_message(br#"{"error":"unauthorized"}"#, true),
        ];
        let mut results = dispatch_local_reply(BodyMode::Buffered, messages)
            .await
            .into_iter()
            .map(Result::unwrap);
        let (headers, body) = (results.next().unwrap(), results.next().unwrap());

        assert!(
            matches!(headers.as_slice(), [ProcessingResponse { response: Some(Response::ResponseHeaders(h)), .. }]
                if h.response.as_ref().is_some_and(|c| c.header_mutation.is_none())),
            "local reply headers must continue unchanged: {headers:?}"
        );
        assert!(
            matches!(body.as_slice(), [ProcessingResponse { response: Some(Response::ResponseBody(b)), .. }]
                if b.response.as_ref().is_some_and(|c| c.body_mutation.is_none())),
            "local reply body must continue unchanged: {body:?}"
        );
        assert_eq!(
            BODY_FILTER_RUNS.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no filter may run on a local reply"
        );
    }

    #[tokio::test]
    async fn local_reply_in_full_duplex_sends_headers_with_first_chunk() {
        use praxis_proto::envoy::service::ext_proc::v3::processing_response::Response;

        let messages = vec![
            local_reply_headers_message(false),
            local_reply_body_message(b"rate limited", true),
        ];
        let mut results = dispatch_local_reply(BodyMode::FullDuplexStreamed, messages)
            .await
            .into_iter()
            .map(Result::unwrap);
        let (headers, body) = (results.next().unwrap(), results.next().unwrap());

        assert!(
            headers.is_empty(),
            "FDS defers the headers response to the first chunk: {headers:?}"
        );
        assert!(
            matches!(
                body.as_slice(),
                [
                    ProcessingResponse {
                        response: Some(Response::ResponseHeaders(_)),
                        ..
                    },
                    ProcessingResponse {
                        response: Some(Response::ResponseBody(_)),
                        ..
                    },
                ]
            ),
            "first chunk must carry the headers response, then the echoed chunk: {body:?}"
        );
    }

    #[tokio::test]
    async fn local_reply_ignores_redelivered_full_duplex_eos_chunk() {
        let messages = vec![
            local_reply_headers_message(false),
            local_reply_body_message(b"rate limited", true),
            local_reply_body_message(b"rate limited", true),
        ];
        let results = dispatch_local_reply(BodyMode::FullDuplexStreamed, messages).await;

        assert!(
            matches!(results.as_slice(), [Ok(_), Ok(_), Ok(redelivered)] if redelivered.is_empty()),
            "a re-delivered FDS EOS chunk must not be echoed again: {results:?}"
        );
    }

    #[tokio::test]
    async fn local_reply_keeps_eos_hardening() {
        let duplicate_eos = dispatch_local_reply(
            BodyMode::Buffered,
            vec![
                local_reply_headers_message(false),
                local_reply_body_message(b"denied", true),
                local_reply_body_message(b"denied", true),
            ],
        )
        .await;
        let body_after_headers_eos = dispatch_local_reply(
            BodyMode::Buffered,
            vec![
                local_reply_headers_message(true),
                local_reply_body_message(b"denied", true),
            ],
        )
        .await;

        assert!(
            matches!(duplicate_eos.last(), Some(Err(e)) if e.code() == tonic::Code::InvalidArgument),
            "a duplicate BUFFERED EOS chunk must be rejected: {duplicate_eos:?}"
        );
        assert!(
            matches!(body_after_headers_eos.last(), Some(Err(e)) if e.code() == tonic::Code::InvalidArgument),
            "a body after headers EOS must be rejected: {body_after_headers_eos:?}"
        );
    }

    #[tokio::test]
    async fn response_body_in_none_mode_is_rejected_before_tracking_or_filters() {
        use praxis_proto::envoy::service::ext_proc::v3::HttpBody;
        use processing_request::Request;

        BODY_FILTER_RUNS.store(0, std::sync::atomic::Ordering::SeqCst);
        let pipeline = body_probe_pipeline();
        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = BodyMode::None;

        let result = dispatch_request(
            &pipeline,
            Request::ResponseBody(HttpBody {
                body: b"unexpected".to_vec(),
                end_of_stream: true,
            }),
            &mut state,
        )
        .await;

        assert!(result.is_err(), "ResponseBody in NONE mode must be rejected");
        if let Err(error) = result {
            assert_eq!(error.code(), tonic::Code::InvalidArgument);
            assert!(error.message().contains("ResponseBody"));
        }
        assert!(
            !state.eos_tracker.response_body.is_complete(),
            "EOS must not be tracked"
        );
        assert!(state.response_body.is_empty(), "body must not be accumulated");
        assert_eq!(BODY_FILTER_RUNS.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    fn local_reply_headers_message(end_of_stream: bool) -> processing_request::Request {
        processing_request::Request::ResponseHeaders(praxis_proto::envoy::service::ext_proc::v3::HttpHeaders {
            end_of_stream,
            ..Default::default()
        })
    }

    fn local_reply_body_message(body: &[u8], end_of_stream: bool) -> processing_request::Request {
        processing_request::Request::ResponseBody(praxis_proto::envoy::service::ext_proc::v3::HttpBody {
            body: body.to_vec(),
            end_of_stream,
        })
    }

    /// Dispatch a stream that opens with response messages, one message at a time, in `mode`.
    async fn dispatch_local_reply(
        mode: BodyMode,
        messages: Vec<processing_request::Request>,
    ) -> Vec<Result<Vec<ProcessingResponse>, Status>> {
        let pipeline = body_probe_pipeline();
        let mut state = StreamState::new();
        state.protocol_config.response_body_mode = mode;
        let mut results = Vec::new();
        for message in messages {
            results.push(dispatch_request(&pipeline, message, &mut state).await);
        }
        results
    }
}
