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
use tokio::sync::mpsc;
use tokio_stream::{StreamExt as _, wrappers::ReceiverStream};
use tonic::{Request as TonicRequest, Response as TonicResponse, Status, Streaming};
use tracing::{debug, error, warn};

use crate::{
    handlers::{handle_request_body, handle_request_headers, handle_response_body, handle_response_headers},
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

/// Per-stream state accumulated across ExtProc phases.
#[derive(Debug, Default)]
pub(crate) struct StreamState {
    /// Re-entrance counters from request-phase branch chains.
    pub(crate) branch_iterations: HashMap<Arc<str>, u32>,

    /// Executed filter indices from request phase.
    pub(crate) executed_filter_indices: Vec<bool>,

    /// Metadata carried from request to response phase.
    pub(crate) filter_metadata: HashMap<String, String>,

    /// Typed per-filter state carried from request to response phase.
    pub(crate) filter_state: HashMap<usize, Box<dyn std::any::Any + Send + Sync>>,

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
}

impl StreamState {
    /// Create a new empty stream state with default protocol configuration.
    pub(crate) fn new() -> Self {
        Self {
            protocol_config: ProtocolConfig::default(),
            ..Default::default()
        }
    }

    /// Restore filter execution state into a response context.
    pub(crate) fn restore_request_ctx(&self, ctx: &mut HttpFilterContext<'_>) {
        ctx.executed_filter_indices.clone_from(&self.executed_filter_indices);
        ctx.branch_iterations.clone_from(&self.branch_iterations);
        ctx.filter_metadata.clone_from(&self.filter_metadata);
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
}
