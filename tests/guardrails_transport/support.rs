//! Test utilities using the product registry and a recording `NeMo` HTTP peer.

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use praxis_extproc::{
    config::{ExtProcConfig, build_pipeline},
    response_mode::ResponseModeSelector,
    server::PraxisExtProc,
};
use praxis_filter::{
    BodyAccess, FilterAction, FilterError, FilterFactory, FilterRegistry, HttpFilter, HttpFilterContext, Rejection,
};
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        BodySendMode, HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, ProcessingResponse, ProtocolConfiguration,
        body_mutation, external_processor_client::ExternalProcessorClient,
        external_processor_server::ExternalProcessorServer, processing_request::Request as RequestMessage,
        processing_response::Response as ResponseMessage,
    },
};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc},
    task::JoinHandle,
};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{
    Streaming,
    transport::{Channel, Server},
};

const DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug)]
pub(crate) enum NemoReply {
    Pass,
    BlockInput,
    BlockOutput,
    Unavailable,
    Malformed,
    Unknown,
    Timeout,
    HoldFirst,
}

#[derive(Default)]
pub(crate) struct PendingCall {
    pub(crate) started: Notify,
    pub(crate) finished: Notify,
}

struct AwaitingProvider(Arc<PendingCall>);

impl Drop for AwaitingProvider {
    fn drop(&mut self) {
        self.0.finished.notify_one();
    }
}

pub(crate) struct Harness {
    pub(crate) client: ExternalProcessorClient<Channel>,
    #[cfg(feature = "integration")]
    pub(crate) endpoint: String,
    pub(crate) pending_call: Arc<PendingCall>,
    calls: Arc<Mutex<Vec<Value>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Harness {
    pub(crate) async fn new(reply: NemoReply, translation: bool) -> Self {
        Self::with_limit(reply, translation, 2_097_152).await
    }

    pub(crate) async fn with_limit(reply: NemoReply, translation: bool, limit: usize) -> Self {
        Self::configured(reply, translation, limit, |_| {}).await
    }

    pub(crate) async fn configured(
        reply: NemoReply,
        translation: bool,
        limit: usize,
        configure: impl FnOnce(&mut ExtProcConfig),
    ) -> Self {
        Self::configured_service(reply, translation, limit, configure, true).await
    }

    pub(crate) async fn embedded(reply: NemoReply) -> Self {
        Self::configured_service(reply, false, 2_097_152, |_| {}, false).await
    }

    async fn configured_service(
        reply: NemoReply,
        translation: bool,
        limit: usize,
        configure: impl FnOnce(&mut ExtProcConfig),
        install_selector: bool,
    ) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/checks", listener.local_addr().unwrap());
        let records = Arc::clone(&calls);
        let pending_call = Arc::new(PendingCall::default());
        let pending = Arc::clone(&pending_call);
        let nemo = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((socket, _)) = listener.accept().await {
                let records = Arc::clone(&records);
                let pending = Arc::clone(&pending);
                connections.spawn(async move {
                    let service =
                        service_fn(move |req| nemo_response(req, reply, Arc::clone(&records), Arc::clone(&pending)));
                    drop(
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                            .await,
                    );
                });
            }
        });
        let mut cfg = configuration(&endpoint, translation, limit);
        configure(&mut cfg);
        let pipeline = build_pipeline(&cfg, &praxis_ai_filters::build_ai_registry()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut service = PraxisExtProc::new(pipeline).with_max_body_accumulation(cfg.max_body_accumulation());
        if install_selector {
            service = service.with_response_mode_selector(ResponseModeSelector::from_config(&cfg.filter_chains));
        }
        let service = ExternalProcessorServer::new(service);
        let server = tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let client = ExternalProcessorClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Self {
            client,
            #[cfg(feature = "integration")]
            endpoint,
            pending_call,
            calls,
            tasks: vec![nemo, server],
        }
    }

    pub(crate) fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) async fn session(&self, mode: BodySendMode, input: bool, output: bool) -> Session {
        let mut session = self.open().await;
        let protocol = ProtocolConfiguration {
            request_body_mode: BodySendMode::Buffered.into(),
            response_body_mode: mode.into(),
            ..Default::default()
        };
        let response = session
            .exchange(request_headers(protocol, (input, output), false))
            .await;
        assert!(matches!(response.response, Some(ResponseMessage::RequestHeaders(_))));
        session
    }

    pub(crate) async fn open(&self) -> Session {
        let (sender, receiver) = mpsc::channel(8);
        let responses = self
            .client
            .clone()
            .process(ReceiverStream::new(receiver))
            .await
            .unwrap()
            .into_inner();
        Session { sender, responses }
    }

    pub(crate) async fn complete(
        &self,
        mode: BodySendMode,
        selected: (bool, bool),
        prompt: &str,
        answer: &str,
    ) -> Vec<ProcessingResponse> {
        let mut session = Box::pin(self.session(mode, selected.0, selected.1)).await;
        let mut replies = vec![Box::pin(session.exchange(request(prompt, false))).await];
        session.send(response_headers("application/json")).await;
        if mode != BodySendMode::FullDuplexStreamed {
            replies.push(session.next().await.unwrap());
        }
        let body = serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":answer}}]})).unwrap();
        session.send(response_body(&body, true)).await;
        drop(session.sender);
        while let Some(response) = tokio::time::timeout(DEADLINE, session.responses.message())
            .await
            .unwrap()
            .unwrap()
        {
            replies.push(response);
        }
        replies
    }
}

pub(crate) struct Session {
    pub(crate) sender: mpsc::Sender<ProcessingRequest>,
    pub(crate) responses: Streaming<ProcessingResponse>,
}

impl Session {
    pub(crate) async fn send(&self, message: ProcessingRequest) {
        self.sender.send(message).await.unwrap();
    }

    pub(crate) async fn next(&mut self) -> Option<ProcessingResponse> {
        tokio::time::timeout(DEADLINE, self.responses.message())
            .await
            .unwrap()
            .unwrap()
    }

    pub(crate) async fn exchange(&mut self, message: ProcessingRequest) -> ProcessingResponse {
        self.send(message).await;
        self.next().await.expect("ExtProc response")
    }
}

pub(crate) fn wrap(request: RequestMessage) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(request),
        ..Default::default()
    }
}
pub(crate) fn header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        raw_value: value.as_bytes().to_vec(),
        ..Default::default()
    }
}
pub(crate) fn request(prompt: &str, stream: bool) -> ProcessingRequest {
    wrap(RequestMessage::RequestBody(HttpBody {
        body: serde_json::to_vec(
            &json!({"model":"test", "messages":[{"role":"user","content":prompt}],"stream":stream}),
        )
        .unwrap(),
        end_of_stream: true,
    }))
}
/// Build a chat-completions *request* body of an exact byte length, carrying
/// real inner text (multibyte UTF-8 and JSON escapes) instead of trailing
/// whitespace padding.
pub(crate) fn exact_size_request(target: usize) -> Vec<u8> {
    exact_size_body(
        target,
        |content| json!({"model":"test","messages":[{"role":"user","content":content}],"stream":false}),
    )
}

/// Build a chat-completions *response* body of an exact byte length, same idea.
pub(crate) fn exact_size_response(target: usize) -> Vec<u8> {
    exact_size_body(
        target,
        |content| json!({"choices":[{"message":{"role":"assistant","content":content}}]}),
    )
}

/// Serialize `envelope(content)` to exactly `target` bytes. The seed carries the
/// bytes worth exercising (2- and 3-byte UTF-8 scalars, an escaped quote, an
/// escaped newline); ASCII padding then dials the serialized length to the
/// target, since each padding byte serializes one-to-one.
fn exact_size_body(target: usize, envelope: impl Fn(&str) -> Value) -> Vec<u8> {
    const SEED: &str = "café ☕ \"q\"\n";
    let base = serde_json::to_vec(&envelope(SEED)).unwrap().len();
    assert!(
        target >= base,
        "target {target} is below the minimal envelope size {base}"
    );
    let content = format!("{SEED}{}", "a".repeat(target - base));
    let bytes = serde_json::to_vec(&envelope(&content)).unwrap();
    assert_eq!(bytes.len(), target, "exact-size payload must hit its byte target");
    bytes
}

pub(crate) fn request_headers(
    protocol: ProtocolConfiguration,
    selected: (bool, bool),
    end_of_stream: bool,
) -> ProcessingRequest {
    let headers = vec![
        header(":method", "POST"),
        header(":path", "/v1/chat/completions"),
        header("content-type", "application/json"),
        header("x-aigateway-guardrail-input", if selected.0 { "true" } else { "false" }),
        header(
            "x-aigateway-guardrail-output",
            if selected.1 { "true" } else { "false" },
        ),
    ];
    let mut message = wrap(RequestMessage::RequestHeaders(HttpHeaders {
        headers: Some(HeaderMap { headers }),
        end_of_stream,
    }));
    message.protocol_config = Some(protocol);
    message
}

pub(crate) fn response_headers(content_type: &str) -> ProcessingRequest {
    wrap(RequestMessage::ResponseHeaders(HttpHeaders {
        headers: Some(HeaderMap {
            headers: vec![header(":status", "200"), header("content-type", content_type)],
        }),
        ..Default::default()
    }))
}
pub(crate) fn response_body(body: &[u8], eos: bool) -> ProcessingRequest {
    wrap(RequestMessage::ResponseBody(HttpBody {
        body: body.to_vec(),
        end_of_stream: eos,
    }))
}
pub(crate) fn immediate_status(responses: &[ProcessingResponse]) -> Option<i32> {
    responses.iter().find_map(|response| match &response.response {
        Some(ResponseMessage::ImmediateResponse(immediate)) => immediate.status.as_ref().map(|status| status.code),
        _ => None,
    })
}
pub(crate) fn immediate_body(response: &ProcessingResponse) -> String {
    match &response.response {
        Some(ResponseMessage::ImmediateResponse(immediate)) => immediate.body.clone(),
        _ => String::new(),
    }
}

pub(crate) fn body_bytes(responses: &[ProcessingResponse]) -> Vec<u8> {
    responses
        .iter()
        .filter_map(|response| match &response.response {
            Some(ResponseMessage::ResponseBody(body)) => {
                body.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref()
            },
            _ => None,
        })
        .flat_map(|mutation| match mutation {
            body_mutation::Mutation::Body(body) => body.clone(),
            body_mutation::Mutation::StreamedResponse(body) => body.body.clone(),
            body_mutation::Mutation::ClearBody(_) => Vec::new(),
        })
        .collect()
}

async fn nemo_response(
    request: Request<Incoming>,
    reply: NemoReply,
    records: Arc<Mutex<Vec<Value>>>,
    pending: Arc<PendingCall>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    assert_eq!(request.uri().path(), "/v1/checks");
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let input = value["guardrails"]["rail_types"][0] == "input";
    let call_number = {
        let mut calls = records.lock().unwrap();
        calls.push(value);
        calls.len()
    };
    if matches!(reply, NemoReply::HoldFirst) && call_number == 1 {
        let _cleanup = AwaitingProvider(Arc::clone(&pending));
        pending.started.notify_one();
        std::future::pending::<()>().await;
    }
    let blocked = matches!(
        (reply, input),
        (NemoReply::BlockInput, true) | (NemoReply::BlockOutput, false)
    );
    let body = match reply {
        NemoReply::Malformed => "not JSON".to_owned(),
        NemoReply::Unknown => json!({"status":"unknown", "content":""}).to_string(),
        _ => json!({"status":if blocked {"blocked"} else {"passed"}, "content":"", "rail":"test-rail"}).to_string(),
    };
    if matches!(reply, NemoReply::Timeout) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(Response::builder()
        .status(if matches!(reply, NemoReply::Unavailable) {
            503
        } else {
            200
        })
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

/// First SSE chunk emitted by the late-rejection harness.
pub(crate) const FIRST: &[u8] = b"data: first\n\n";

/// An in-process ext-proc server whose single filter streams response chunks
/// and rejects once a byte budget is exceeded, used to prove that an
/// `ImmediateResponse` after some output was released still terminates cleanly.
pub(crate) struct RejectingServer {
    pub(crate) address: SocketAddr,
    calls: Arc<Mutex<Vec<Vec<u8>>>>,
    task: JoinHandle<()>,
}

impl Drop for RejectingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RejectingServer {
    pub(crate) async fn new(accepted_bytes: usize) -> Self {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&calls);
        let mut registry = FilterRegistry::with_builtins();
        registry
            .register(
                "late_rejection",
                FilterFactory::Http(Arc::new(move |_| {
                    Ok(Box::new(LateRejection {
                        accepted_bytes,
                        calls: Arc::clone(&record),
                    }))
                })),
            )
            .unwrap();
        let cfg: ExtProcConfig =
            serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: late_rejection\n")
                .unwrap();
        let service = ExternalProcessorServer::new(PraxisExtProc::new(build_pipeline(&cfg, &registry).unwrap()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            Server::builder()
                .add_service(service)
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        Self { address, calls, task }
    }

    pub(crate) fn calls(&self) -> Vec<Vec<u8>> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) async fn session(&self, mode: BodySendMode) -> Session {
        let mut client = Box::pin(ExternalProcessorClient::connect(format!("http://{}", self.address)))
            .await
            .unwrap();
        let (sender, receiver) = mpsc::channel(8);
        let responses = client
            .process(ReceiverStream::new(receiver))
            .await
            .unwrap()
            .into_inner();
        let mut session = Session { sender, responses };
        let protocol = ProtocolConfiguration {
            request_body_mode: BodySendMode::None.into(),
            response_body_mode: mode.into(),
            ..Default::default()
        };
        let headers = session.exchange(request_headers(protocol, (false, false), true)).await;
        assert!(matches!(headers.response, Some(ResponseMessage::RequestHeaders(_))));
        session.send(response_headers("text/event-stream")).await;
        if mode != BodySendMode::FullDuplexStreamed {
            assert!(matches!(
                session.next().await.unwrap().response,
                Some(ResponseMessage::ResponseHeaders(_))
            ));
        }
        session
    }
}

struct LateRejection {
    accepted_bytes: usize,
    calls: Arc<Mutex<Vec<Vec<u8>>>>,
}

#[async_trait::async_trait]
impl HttpFilter for LateRejection {
    fn name(&self) -> &'static str {
        "late_rejection"
    }

    async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    fn response_body_access(&self) -> BodyAccess {
        BodyAccess::ReadOnly
    }

    fn response_body_mode(&self) -> praxis_filter::BodyMode {
        praxis_filter::BodyMode::Stream
    }

    fn on_response_body(
        &self,
        _: &mut HttpFilterContext<'_>,
        body: &mut Option<Bytes>,
        _: bool,
    ) -> Result<FilterAction, FilterError> {
        let bytes = body.as_deref().unwrap_or_default();
        if bytes.is_empty() {
            return Ok(FilterAction::Continue);
        }
        let mut calls = self.calls.lock().unwrap();
        let total = calls.iter().map(Vec::len).sum::<usize>() + bytes.len();
        calls.push(bytes.to_vec());
        drop(calls);
        Ok(if total > self.accepted_bytes {
            FilterAction::Reject(Rejection::status(403))
        } else {
            FilterAction::Continue
        })
    }
}

pub(crate) fn configuration(endpoint: &str, translation: bool, limit: usize) -> ExtProcConfig {
    let mut filters = Vec::new();
    filters.push(json!({"filter":"request_id"}));
    for (name, input, output) in [("input", true, false), ("output", false, true)] {
        filters.push(json!({"filter":"ai_guardrails", "conditions":[{"when":{"headers":{format!("x-aigateway-guardrail-{name}"):"true"}}}],
            "phase":{"request":input,"response":output}, "provider":{"type":"nemo", "endpoint":endpoint, "timeout_ms":500, "guardrails":{"config_ids":[name]}}}));
    }
    if translation {
        filters.push(json!({"filter":"anthropic_messages_to_chat_completions", "max_body_bytes":1_048_576, "conditions":[{"when":{"path":"/v1/messages"}}]}));
        filters.push(json!({"filter":"anthropic_messages_to_chat_completions_stream", "conditions":[{"when":{"path":"/v1/messages"}}], "response_conditions":[{"when":{"headers":{"content-type":"text/event-stream"}}}]}));
    }
    filters.push(
        json!({"filter":"headers", "request_remove":["x-aigateway-guardrail-input","x-aigateway-guardrail-output"]}),
    );
    serde_yaml::from_str(&serde_json::to_string(&json!({"server":{"max_body_bytes":limit}, "insecure_options":{"allow_private_upstreams":true}, "filter_chains":[{"name":"ipp","filters":filters}]})).unwrap()).unwrap()
}
