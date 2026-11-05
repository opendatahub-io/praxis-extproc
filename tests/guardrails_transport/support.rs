//! Test utilities using the product registry and a recording `NeMo` HTTP peer.

use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use praxis_extproc::{
    config::{ExtProcConfig, build_pipeline},
    server::PraxisExtProc,
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
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};
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
}

pub(crate) struct Harness {
    pub(crate) client: ExternalProcessorClient<Channel>,
    #[cfg(feature = "integration")]
    pub(crate) endpoint: String,
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
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/checks", listener.local_addr().unwrap());
        let records = Arc::clone(&calls);
        let nemo = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((socket, _)) = listener.accept().await {
                let records = Arc::clone(&records);
                connections.spawn(async move {
                    let service = service_fn(move |req| nemo_response(req, reply, Arc::clone(&records)));
                    drop(
                        hyper::server::conn::http1::Builder::new()
                            .serve_connection(hyper_util::rt::TokioIo::new(socket), service)
                            .await,
                    );
                });
            }
        });
        let cfg = configuration(&endpoint, translation, limit);
        let pipeline = build_pipeline(&cfg, &praxis_ai_filters::build_ai_registry()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = ExternalProcessorServer::new(
            PraxisExtProc::new(pipeline).with_max_body_accumulation(cfg.max_body_accumulation()),
        );
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
            calls,
            tasks: vec![nemo, server],
        }
    }

    pub(crate) fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    pub(crate) async fn session(&self, mode: BodySendMode, input: bool, output: bool) -> Session {
        let mut session = self.open().await;
        let headers = vec![
            header(":method", "POST"),
            header(":path", "/v1/chat/completions"),
            header("content-type", "application/json"),
            header("x-aigateway-guardrail-input", if input { "true" } else { "false" }),
            header("x-aigateway-guardrail-output", if output { "true" } else { "false" }),
        ];
        let mut message = wrap(RequestMessage::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap { headers }),
            ..Default::default()
        }));
        message.protocol_config = Some(ProtocolConfiguration {
            request_body_mode: BodySendMode::Buffered.into(),
            response_body_mode: mode.into(),
            ..Default::default()
        });
        let response = session.exchange(message).await;
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
) -> Result<Response<Full<Bytes>>, Infallible> {
    assert_eq!(request.uri().path(), "/v1/checks");
    let body = request.into_body().collect().await.unwrap().to_bytes();
    let value: Value = serde_json::from_slice(&body).unwrap();
    let input = value["guardrails"]["rail_types"][0] == "input";
    records.lock().unwrap().push(value);
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
