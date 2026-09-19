// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Per-stream Kuadrant policy executor.
//!
//! The Kuadrant pipeline is `!Send` **and** stateful across request/response
//! phases: the rate-limit *check* runs on the request, the token *report* runs
//! on the response, and both are tasks of one pipeline instance correlated by
//! the request context. That instance therefore cannot live in the `Send`
//! `ext_proc` `StreamState` (held across `.await` in a spawned handler).
//!
//! [`PolicyStream`] runs the pipeline on a dedicated per-stream thread with its
//! own current-thread tokio runtime. There, `!Send` is free (no work-stealing),
//! so the pipeline is held across `.await` and the drive stays fully async —
//! no `block_on`, no `spawn_blocking`. The `ext_proc` handlers talk to the thread
//! over channels carrying only `Send` messages, and get back an allow/reject
//! decision per phase.

use std::{sync::Arc, thread::JoinHandle};

use kuadrant_filter::kuadrant::{Pipeline, PipelineFactory, ReqRespCtx, resolver::AttributeResolver};
use tokio::sync::{mpsc, oneshot};

use crate::kuadrant_host::{GrpcTransport, HttpReply, PhaseOutcome, PraxisResolver, drive_phase};

/// One request/response phase, handed to the executor thread.
enum Phase {
    /// Request headers (also seed `request.*` properties). Enforces auth + the
    /// rate-limit check.
    RequestHeaders(Vec<(String, String)>),
    /// Response headers.
    ResponseHeaders(Vec<(String, String)>),
    /// A response body chunk plus whether it is the final one. In streamed mode
    /// the crate accumulates chunks internally as they arrive; the token report
    /// fires on end-of-stream. In buffered mode this is one chunk with `eos=true`.
    ResponseBody(Vec<u8>, bool),
}

/// A phase event plus the one-shot channel to answer on.
struct PhaseMsg {
    /// The request/response phase to process.
    phase: Phase,
    /// `Ok(None)` = allow; `Ok(Some(reply))` = reject with this immediate reply;
    /// `Err(msg)` = internal error (the caller decides fail-open vs. fail-closed).
    reply: oneshot::Sender<Result<Option<HttpReply>, String>>,
}

/// Handle to a per-stream policy executor thread. Dropping it closes the channel
/// and the thread exits after finishing any in-flight phase.
#[derive(Debug)]
pub struct PolicyStream {
    /// Sends phase events to the executor thread.
    tx: mpsc::UnboundedSender<PhaseMsg>,
    /// Join handle for the executor thread (kept so it lives with this handle).
    _thread: JoinHandle<()>,
}

impl PolicyStream {
    /// Spawn a policy executor for one stream. `factory` is the Kuadrant pipeline
    /// factory, compiled once at startup and shared across streams; `transport`
    /// dials Authorino/Limitador. Generic over the transport (monomorphized, no
    /// vtable) so tests inject a fake.
    ///
    /// # Panics
    /// Panics if the OS cannot spawn the thread or build its tokio runtime —
    /// both are unrecoverable at stream start.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "thread/runtime creation failure at stream start is unrecoverable"
    )]
    pub fn spawn<T: GrpcTransport + 'static>(factory: Arc<PipelineFactory>, transport: Arc<T>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let thread = std::thread::Builder::new()
            .name("kuadrant-policy".to_owned())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build kuadrant-policy current-thread runtime");
                rt.block_on(run_executor(rx, factory, transport));
            })
            .expect("spawn kuadrant-policy thread");
        Self { tx, _thread: thread }
    }

    /// Enforce the request phase (auth + rate-limit check). `Ok(Some)` rejects.
    ///
    /// # Errors
    /// Returns a message if the executor thread is gone or the drive fails.
    pub async fn on_request_headers(&self, headers: Vec<(String, String)>) -> Result<Option<HttpReply>, String> {
        self.phase(Phase::RequestHeaders(headers)).await
    }

    /// Resume the pipeline on the response-headers phase.
    ///
    /// # Errors
    /// Returns a message if the executor thread is gone or the drive fails.
    pub async fn on_response_headers(&self, headers: Vec<(String, String)>) -> Result<Option<HttpReply>, String> {
        self.phase(Phase::ResponseHeaders(headers)).await
    }

    /// Feed a response-body chunk. In streamed mode call once per chunk with the
    /// current chunk bytes and Envoy's `end_of_stream`; the token report fires on
    /// the final (end-of-stream) call. In buffered mode call once with the full
    /// body and `end_of_stream = true`.
    ///
    /// # Errors
    /// Returns a message if the executor thread is gone or the drive fails.
    pub async fn on_response_body(&self, body: Vec<u8>, end_of_stream: bool) -> Result<Option<HttpReply>, String> {
        self.phase(Phase::ResponseBody(body, end_of_stream)).await
    }

    /// Send one phase to the executor thread and await its allow/reject decision.
    async fn phase(&self, phase: Phase) -> Result<Option<HttpReply>, String> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PhaseMsg { phase, reply: reply_tx })
            .map_err(|e| format!("kuadrant policy executor thread gone: {e}"))?;
        reply_rx
            .await
            .map_err(|e| format!("kuadrant policy executor dropped reply: {e}"))?
    }
}

/// Pipeline lifecycle across phases, owned by the executor thread.
enum ExecState {
    /// Not built yet (before the request-headers phase).
    Fresh,
    /// Built and paused between phases; resume with `Pipeline::eval`. Boxed —
    /// `Pipeline` is large, so this keeps `ExecState` small and avoids copying it.
    Paused(Box<Pipeline>),
    /// Finished — completed, terminated with a reply, or bypassed (no match).
    /// Later phases short-circuit to allow.
    Done,
}

/// The executor thread's loop: one persisted pipeline, driven phase by phase.
#[expect(
    clippy::future_not_send,
    reason = "runs on the per-stream current-thread runtime; the Kuadrant pipeline is !Send by design"
)]
async fn run_executor<T: GrpcTransport>(
    mut rx: mpsc::UnboundedReceiver<PhaseMsg>,
    factory: Arc<PipelineFactory>,
    transport: Arc<T>,
) {
    // One resolver for the whole stream; phases install their data on it and the
    // persisted pipeline reads it back through the `AttributeResolver` seam.
    let resolver = Arc::new(PraxisResolver::new(Vec::new(), Vec::new(), None, None));
    let mut state = ExecState::Fresh;

    while let Some(msg) = rx.recv().await {
        let result = handle_phase(&mut state, &resolver, &factory, transport.as_ref(), msg.phase).await;
        // Receiver may be gone if the stream was cancelled; that's fine.
        drop(msg.reply.send(result));
    }
}

/// Install a phase's data and drive the persisted pipeline forward over it.
#[expect(
    clippy::future_not_send,
    reason = "runs on the per-stream current-thread runtime; the Kuadrant pipeline is !Send by design"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one linear phase pipeline: install data, obtain/build, signal body, drive"
)]
async fn handle_phase<T: GrpcTransport>(
    state: &mut ExecState,
    resolver: &Arc<PraxisResolver>,
    factory: &PipelineFactory,
    transport: &T,
    phase: Phase,
) -> Result<Option<HttpReply>, String> {
    // Install this phase's data on the resolver; note the response-body length so
    // we can tell the pipeline ctx the body is now available (below).
    let response_body_signal = match phase {
        Phase::RequestHeaders(headers) => {
            resolver.update_request_headers(headers);
            None
        },
        Phase::ResponseHeaders(headers) => {
            resolver.update_response_headers(headers);
            None
        },
        Phase::ResponseBody(body, end_of_stream) => {
            // Per-chunk, mirroring Envoy's drained buffer: the token-usage parser
            // accumulates frames itself, so we install just this chunk.
            let len = resolver.set_response_body_chunk(body);
            Some((len, end_of_stream))
        },
    };

    // Obtain the pipeline: build on the first phase, resume a paused one, or
    // short-circuit if it already finished.
    let mut pipeline = match std::mem::replace(state, ExecState::Done) {
        ExecState::Done => return Ok(None),
        ExecState::Paused(pipeline) => pipeline,
        ExecState::Fresh => {
            // The factory is compiled once at startup (KuadrantPolicy::new) and
            // shared; per stream we build only the per-request pipeline + ctx.
            let resolver_concrete = Arc::clone(resolver);
            let resolver_dyn: Arc<dyn AttributeResolver> = resolver_concrete;
            let ctx = ReqRespCtx::new(resolver_dyn);
            match factory.build(ctx).map_err(|e| format!("build: {e:?}"))? {
                Some(pipeline) => Box::new(pipeline),
                None => return Ok(None), // no blueprint matched this host -> allow
            }
        },
    };

    // Signal body availability on the ctx — this is what gates response-phase
    // tasks (e.g. the token report waits until the response body is end-of-stream),
    // mirroring the shim's `ctx.response_body.set_buffer_size` on `on_http_response_body`.
    if let Some((len, end_of_stream)) = response_body_signal {
        pipeline.ctx.response_body.set_buffer_size(len, end_of_stream);
    }

    match drive_phase(pipeline.eval(), resolver.as_ref(), transport)
        .await
        .map_err(|e| format!("drive: {e:?}"))?
    {
        PhaseOutcome::Done(reply) => {
            *state = ExecState::Done;
            Ok(reply)
        },
        PhaseOutcome::Paused(pipeline) => {
            *state = ExecState::Paused(pipeline);
            Ok(None) // allow this phase; enforcement resumes on the next one
        },
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    reason = "tests"
)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use kuadrant_filter::{configuration::PluginConfiguration, filter::DescriptorManager};

    use super::*;
    use crate::kuadrant_host::GrpcDispatch;

    /// Compile a plugin-config JSON into the shared pipeline factory, mirroring
    /// `KuadrantPolicy::new` so the tests drive the same startup path.
    fn compile(config_json: &str) -> Arc<PipelineFactory> {
        let config: PluginConfiguration = serde_yaml::from_str(config_json).expect("parse RL config");
        let descriptors = Arc::new(DescriptorManager::default());
        Arc::new(PipelineFactory::try_from(config, &descriptors).expect("compile RL config"))
    }

    /// Records each dispatch's `(service, method)` and returns a canned
    /// Limitador `RateLimitResponse { code: OK }` (`[8, 1]`, field 1 varint = 1).
    #[derive(Default)]
    struct MockTransport {
        calls: Mutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl GrpcTransport for MockTransport {
        async fn call(&self, pending: &GrpcDispatch) -> Result<(u32, Vec<u8>), String> {
            self.calls
                .lock()
                .expect("calls mutex")
                .push((pending.service.clone(), pending.method.clone()));
            Ok((0, vec![8, 1]))
        }
    }

    // Token rate limiting in the wasm-shim `dynamic`-service schema (the shape the
    // shim's own `examples/ratelimit_check_report` ships): a request-phase check
    // dispatches `CheckRateLimit`, a `store` action reads the completion's
    // `usage.total_tokens` off the response body, and a report dispatches
    // `Report` debiting those tokens. The store gates the report to the
    // response-body phase, exactly the cross-phase span the executor persists.
    const RL_CONFIG: &str = r#"{
        "services": {
            "ratelimit-check-service":  { "type": "dynamic", "endpoint": "limitador-cluster", "failureMode": "deny", "grpcService": "kuadrant.service.ratelimit.v1.RateLimitService", "grpcMethod": "CheckRateLimit" },
            "ratelimit-report-service": { "type": "dynamic", "endpoint": "limitador-cluster", "failureMode": "deny", "grpcService": "kuadrant.service.ratelimit.v1.RateLimitService", "grpcMethod": "Report" }
        },
        "actionSets": [{
            "name": "some-name",
            "routeRuleConditions": {
                "hostnames": ["example.com", "*.example.com"],
                "predicates": ["request.path == '/v1/chat/completions'"]
            },
            "actions": [
                {
                    "type": "grpc", "var": "ratelimit_response", "service": "ratelimit-check-service",
                    "predicate": "request.path == '/v1/chat/completions'", "terminal": false, "label": "ratelimit",
                    "messageBuilder": "envoy.service.ratelimit.v3.RateLimitRequest { domain: \"domain-a\", hits_addend: 1u, descriptors: [ envoy.extensions.common.ratelimit.v3.RateLimitDescriptor { entries: [ envoy.extensions.common.ratelimit.v3.RateLimitDescriptor.Entry { key: \"a\", value: string(1) } ] } ] }",
                    "onReply": [
                        { "type": "deny", "predicate": "ratelimit_response.overall_code == 2", "terminal": true, "denyWith": "DenyResponse{status: 429u, body: \"Too Many Requests\\n\"}" }
                    ]
                },
                {
                    "type": "store", "predicate": "true", "terminal": false,
                    "path": "kuadrant.internal.response.body",
                    "value": "{\"total_tokens\": responseBodyJSON(\"/usage/total_tokens\")}"
                },
                {
                    "type": "grpc", "execution": "sequential", "var": "report_response", "service": "ratelimit-report-service",
                    "predicate": "request.path == '/v1/chat/completions'", "terminal": false, "isGuard": false, "label": "ratelimit_report",
                    "messageBuilder": "envoy.service.ratelimit.v3.RateLimitRequest { domain: \"domain-a\", hits_addend: uint(kuadrant.internal.response.body.total_tokens), descriptors: [ envoy.extensions.common.ratelimit.v3.RateLimitDescriptor { entries: [ envoy.extensions.common.ratelimit.v3.RateLimitDescriptor.Entry { key: \"a\", value: string(1) } ] } ] }"
                }
            ]
        }]
    }"#;

    /// One `PolicyStream` must dispatch the rate-limit check on the request phase
    /// and the token report on the response-body phase, one pipeline spanning
    /// both, and allow throughout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn check_on_request_then_report_on_response_body() {
        let transport = Arc::new(MockTransport::default());
        let stream = PolicyStream::spawn(compile(RL_CONFIG), Arc::clone(&transport));

        // Request phase: host + path match the action set, so the RL check fires.
        let request_headers = vec![
            (":authority".to_owned(), "example.com".to_owned()),
            (":path".to_owned(), "/v1/chat/completions".to_owned()),
            (":method".to_owned(), "POST".to_owned()),
        ];
        assert!(
            stream
                .on_request_headers(request_headers)
                .await
                .expect("request phase")
                .is_none(),
            "OK check -> allow"
        );

        // Response headers: nothing to report yet; pipeline stays paused.
        assert!(
            stream
                .on_response_headers(Vec::new())
                .await
                .expect("response headers")
                .is_none()
        );

        // Response body: the completion's token usage -> RL report/debit.
        let body = br#"{"usage":{"prompt_tokens":0,"completion_tokens":11,"total_tokens":11}}"#.to_vec();
        assert!(
            stream
                .on_response_body(body, true)
                .await
                .expect("response body")
                .is_none(),
            "report -> allow"
        );

        let calls = transport.calls.lock().expect("calls mutex").clone();
        assert_eq!(calls.len(), 2, "exactly one check + one report: {calls:?}");
        assert_eq!(calls[0].1, "CheckRateLimit", "request phase dispatches the check");
        assert_eq!(calls[1].1, "Report", "response-body phase dispatches the report");
    }

    /// A streamed SSE response arrives in chunks with `end_of_stream = false`,
    /// the usage lands mid-stream, and Envoy sends a final empty
    /// `end_of_stream = true`. The report must fire exactly once, on
    /// end-of-stream, never on a content chunk, so streaming latency is
    /// unaffected.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn report_fires_once_on_streamed_end_of_stream() {
        let transport = Arc::new(MockTransport::default());
        let stream = PolicyStream::spawn(compile(RL_CONFIG), Arc::clone(&transport));

        let request_headers = vec![
            (":authority".to_owned(), "example.com".to_owned()),
            (":path".to_owned(), "/v1/chat/completions".to_owned()),
            (":method".to_owned(), "POST".to_owned()),
        ];
        assert!(
            stream
                .on_request_headers(request_headers)
                .await
                .expect("request")
                .is_none()
        );
        let response_headers = vec![("content-type".to_owned(), "text/event-stream".to_owned())];
        assert!(
            stream
                .on_response_headers(response_headers)
                .await
                .expect("resp headers")
                .is_none()
        );

        // SSE content chunks, none end-of-stream: accumulated, never reported.
        let chunks: [&[u8]; 4] = [
            b"data: {\"id\":\"1\",\"content\":\"Hello\"}\n\n",
            b"data: {\"id\":\"2\",\"content\":\"World\"}\n\n",
            b"data: {\"usage\":{\"total_tokens\":11}}\n\n",
            b"data: [DONE]\n\n",
        ];
        for chunk in chunks {
            assert!(
                stream
                    .on_response_body(chunk.to_vec(), false)
                    .await
                    .expect("chunk")
                    .is_none(),
                "content chunk -> allow (streams to client), no report"
            );
            assert_eq!(
                transport.calls.lock().expect("calls mutex").len(),
                1,
                "no report before eos"
            );
        }

        // Envoy's terminal empty end-of-stream frame: the report fires now.
        assert!(stream.on_response_body(Vec::new(), true).await.expect("eos").is_none());
        let calls = transport.calls.lock().expect("calls mutex").clone();
        assert_eq!(calls.len(), 2, "check + exactly one report at eos: {calls:?}");
        assert_eq!(calls[1].1, "Report", "the single report fires on end-of-stream");
    }
}
