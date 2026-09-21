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

use std::{collections::HashMap, mem};

use bytes::Bytes;
use praxis_filter::{FilterAction, FilterPipeline, HttpFilterContext, Response};
use praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse;
use tonic::Status;

use crate::{
    adapter, metrics,
    response::{self, BodyMode},
    server::StreamState,
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
pub(crate) async fn run_response_header_filters_early(
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
        let mut state = StreamState::new();
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
}
