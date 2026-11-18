// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Shared harness, stub filters, config, and helpers for the adapter tests.

pub(crate) use std::{path::PathBuf, process::Command};

pub(crate) use praxis_extproc::{
    config,
    grpc_limits::{DEFAULT_GRPC_MAX_RECV_MSG_SIZE, GRPC_COMPILED_MAX_RECV_DEFAULT, GRPC_MAX_RECV_MSG_SIZE_ENV},
    server::{self, PraxisExtProc},
};
pub(crate) use praxis_filter::{
    BodyAccess, BodyMode as FilterBodyMode, FilterAction, FilterError, FilterRegistry, HttpFilter, HttpFilterContext,
    SelectedUpstreamBodyOutcome,
};
pub(crate) use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        BodySendMode, HeaderMap, HttpBody, HttpHeaders, HttpTrailers, ProcessingRequest, ProcessingResponse,
        external_processor_server::ExternalProcessorServer, processing_request::Request as ReqVariant,
        processing_response::Response as RespVariant,
    },
};
pub(crate) use tokio::net::TcpListener;
pub(crate) use tokio_stream::wrappers::ReceiverStream;
pub(crate) use tonic::transport::{Channel, Server};

pub(crate) const TIMEOUT_MILLIS: u64 = 500;


/// Extract `end_of_stream` from a streamed request-body response, if present.
pub(crate) fn streamed_eos(msg: &ProcessingResponse) -> Option<bool> {
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;
    match msg.response.as_ref()? {
        RespVariant::RequestBody(b) => match b.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref()? {
            body_mutation::Mutation::StreamedResponse(s) => Some(s.end_of_stream),
            _ => None,
        },
        _ => None,
    }
}


/// Pull the next response from a `FULL_DUPLEX` stream with the shared timeout.
pub(crate) async fn next_full_duplex_msg(stream: &mut tonic::Streaming<ProcessingResponse>) -> ProcessingResponse {
    tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MILLIS), stream.message())
        .await
        .expect("timed out waiting for response")
        .expect("response stream error")
        .expect("stream closed before response")
}


pub(crate) struct WireSelectedBodyFilter;


#[async_trait::async_trait]
impl HttpFilter for WireSelectedBodyFilter {
    fn name(&self) -> &'static str {
        "wire_selected_body"
    }

    async fn on_request(&self, ctx: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        ctx.cluster = Some("selected-provider".into());
        ctx.request_headers_to_set.push((
            "x-selected-early".parse().expect("header name"),
            "yes".parse().expect("header value"),
        ));
        Ok(FilterAction::Continue)
    }

    fn selected_upstream_request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    fn request_body_mode(&self) -> FilterBodyMode {
        FilterBodyMode::StreamBuffer { max_bytes: Some(1024) }
    }

    async fn on_selected_upstream_request_body(
        &self,
        _: &mut HttpFilterContext<'_>,
        body: &mut Option<bytes::Bytes>,
    ) -> Result<SelectedUpstreamBodyOutcome, FilterError> {
        *body = Some(bytes::Bytes::from_static(b"rewritten-on-selected-upstream"));
        Ok(SelectedUpstreamBodyOutcome::Continue)
    }
}


impl WireSelectedBodyFilter {
    #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
    pub(crate) fn from_config(_: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        Ok(Box::new(Self))
    }
}


/// Replacement body size for the encode-limit test: larger than the test's
/// per-message encode cap, smaller than the default so other tests are unaffected.
pub(crate) const INFLATED_BODY_BYTES: usize = 128 * 1024;


/// Replaces the request body with a payload larger than the encode limit, so the
/// server must try to encode an oversized outbound `ProcessingResponse`.
pub(crate) struct InflatingBodyFilter;


#[async_trait::async_trait]
impl HttpFilter for InflatingBodyFilter {
    fn name(&self) -> &'static str {
        "inflating_body"
    }

    async fn on_request(&self, _: &mut HttpFilterContext<'_>) -> Result<FilterAction, FilterError> {
        Ok(FilterAction::Continue)
    }

    fn request_body_access(&self) -> BodyAccess {
        BodyAccess::ReadWrite
    }

    async fn on_request_body(
        &self,
        _: &mut HttpFilterContext<'_>,
        body: &mut Option<bytes::Bytes>,
        _end_of_stream: bool,
    ) -> Result<FilterAction, FilterError> {
        *body = Some(bytes::Bytes::from(vec![b'A'; INFLATED_BODY_BYTES]));
        Ok(FilterAction::Continue)
    }
}


impl InflatingBodyFilter {
    #[expect(clippy::unnecessary_wraps, reason = "FilterFactory signature requires Result")]
    pub(crate) fn from_config(_: &serde_yaml::Value) -> Result<Box<dyn HttpFilter>, FilterError> {
        Ok(Box::new(Self))
    }
}


pub(crate) const INFLATING_BODY_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: inflating_body
insecure_options:
  allow_unbounded_body: true
"#;


pub(crate) const HEADERS_ONLY_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: request_id
insecure_options:
  allow_unbounded_body: true
"#;


pub(crate) const SMALL_BODY_LIMIT_CONFIG: &str = r#"
server:
  max_body_bytes: 1024
filter_chains:
  - name: test
    filters:
      - filter: request_id
"#;


/// Default `max_body_bytes` (32 MiB) with no `allow_unbounded_body`, for framing
/// / accumulation boundary tests against the raised gRPC receive ceiling.
pub(crate) const DEFAULT_BODY_LIMIT_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: request_id
"#;


pub(crate) const HEADERS_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: request_id
      - filter: headers
        request_add:
          - name: X-Test
            value: extproc
insecure_options:
  allow_unbounded_body: true
"#;


pub(crate) const GUARDRAILS_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: guardrails
        rules:
          - target: body
            contains: "DROP TABLE"
insecure_options:
  allow_unbounded_body: true
"#;


pub(crate) const UNCONDITIONAL_BRANCH_CONFIG: &str = r#"
filter_chains:
  - name: branch_chain
    filters:
      - filter: headers
        request_add:
          - name: X-Branch-Applied
            value: "true"
  - name: test
    filters:
      - filter: headers
        request_add:
          - name: X-Main
            value: "true"
        branch_chains:
          - name: always_run
            rejoin: next
            chains:
              - branch_chain
insecure_options:
  allow_unbounded_body: true
"#;


pub(crate) const CONDITIONAL_TERMINAL_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: guardrails
        action: flag
        rules:
          - target: header
            name: "x-danger"
            contains: "true"
        branch_chains:
          - name: block_dangerous
            on_result:
              filter: guardrails
              result: blocked
            rejoin: terminal
            chains:
              - name: reject
                filters:
                  - filter: static_response
                    status: 403
                    body: "blocked by branch"
insecure_options:
  allow_unbounded_body: true
"#;


pub(crate) const RESPONSE_HEADER_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: headers
        response_set:
          - name: X-Resp
            value: "true"
insecure_options:
  allow_unbounded_body: true
"#;


/// A filter whose response transform is split across the response headers and
/// body phases: `anthropic_messages_to_chat_completions` records its transform
/// decision in `filter_metadata` during `on_response` and reads it back in
/// `on_response_body`. Exercises the BUFFERED metadata-carry path.
pub(crate) const ANTHROPIC_TRANSFORM_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: anthropic_messages_to_chat_completions
insecure_options:
  allow_unbounded_body: true
"#;


/// Shared translation chain with Envoy-owned routing and path-conditional filters.
pub(crate) const ENVOY_ANTHROPIC_CONFIG: &str = r#"
filter_chains:
  - name: shared
    filters:
      - filter: anthropic_messages_request
        on_invalid: reject
        conditions:
          - when:
              path: "/v1/messages"
      - filter: anthropic_messages_to_chat_completions
        max_body_bytes: 1048576
        conditions:
          - when:
              path: "/v1/messages"
      - filter: path_rewrite
        replace:
          pattern: "^/v1/messages$"
          replacement: "/v1/chat/completions"
        conditions:
          - when:
              path: "/v1/messages"
"#;


pub(crate) const SELECTED_BODY_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: wire_selected_body
"#;


pub(crate) type ExtProcClient =
    praxis_proto::envoy::service::ext_proc::v3::external_processor_client::ExternalProcessorClient<Channel>;


pub(crate) fn command_output(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}


pub(crate) fn validate_command(config: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_praxis-extproc"));
    command.args(["-t", "-c"]).arg(config);
    command
}


pub(crate) fn write_invalid_tls_validate_config() -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::tempdir().expect("create temporary directory");
    let missing = temp_dir.path().join("missing-tls");
    let yaml = format!(
        "server:\n  grpc_address: \"127.0.0.1:50051\"\n  health_address: \"127.0.0.1:50052\"\n  \
         metrics_address: \"127.0.0.1:9090\"\n  tls:\n    mode: provided\n    cert_path: \"{}.crt\"\n    \
         key_path: \"{}.key\"\nfilter_chains:\n  - name: test\n    filters:\n      - filter: request_id\n\
         insecure_options:\n  allow_unbounded_body: true\n",
        missing.display(),
        missing.display()
    );
    let path = temp_dir.path().join("config.yaml");
    std::fs::write(&path, yaml).expect("write invalid TLS config");
    (temp_dir, path)
}


pub(crate) async fn start_server(config_yaml: &str) -> (ExtProcClient, tokio::sync::oneshot::Sender<()>) {
    start_server_with_registry(config_yaml, praxis_ai_filters::build_ai_registry()).await
}


pub(crate) async fn start_server_with_registry(
    config_yaml: &str,
    registry: FilterRegistry,
) -> (ExtProcClient, tokio::sync::oneshot::Sender<()>) {
    // Pin the shared harness to the crate default so ambient
    // `GRPC_MAX_RECV_MSG_SIZE` cannot reshape unrelated tests.
    start_server_with_registry_and_max_recv(config_yaml, registry, DEFAULT_GRPC_MAX_RECV_MSG_SIZE).await
}


pub(crate) async fn start_server_with_registry_and_max_recv(
    config_yaml: &str,
    registry: FilterRegistry,
    max_recv: usize,
) -> (ExtProcClient, tokio::sync::oneshot::Sender<()>) {
    let cfg: config::ExtProcConfig = serde_yaml::from_str(config_yaml).expect("parse config");
    let pipeline = config::build_pipeline(&cfg, &registry).expect("build pipeline");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let svc = server::external_processor_server(
        PraxisExtProc::new(pipeline).with_max_body_accumulation(cfg.max_body_accumulation()),
        max_recv,
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                drop(shutdown_rx.await);
            })
            .await
            .expect("server failed");
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (extproc_client_for_addr(addr).await, shutdown_tx)
}


/// In-process server that leaves tonic's compiled 4 MiB max-recv untouched.
pub(crate) async fn start_server_with_tonic_compiled_max_recv(
    config_yaml: &str,
) -> (ExtProcClient, tokio::sync::oneshot::Sender<()>) {
    let cfg: config::ExtProcConfig = serde_yaml::from_str(config_yaml).expect("parse config");
    let pipeline = config::build_pipeline(&cfg, &praxis_ai_filters::build_ai_registry()).expect("build pipeline");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let svc = ExternalProcessorServer::new(
        PraxisExtProc::new(pipeline).with_max_body_accumulation(cfg.max_body_accumulation()),
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                drop(shutdown_rx.await);
            })
            .await
            .expect("server failed");
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (extproc_client_for_addr(addr).await, shutdown_tx)
}


pub(crate) async fn extproc_client_for_addr(addr: std::net::SocketAddr) -> ExtProcClient {
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("uri")
        .connect()
        .await
        .expect("connect");
    // Client send/recv limits stay at the raised default so tests can deliver
    // bodies that only the *server* max_recv under test should reject.
    ExtProcClient::new(channel)
        .max_decoding_message_size(DEFAULT_GRPC_MAX_RECV_MSG_SIZE)
        .max_encoding_message_size(DEFAULT_GRPC_MAX_RECV_MSG_SIZE)
}


/// Connect to a spawned `praxis-extproc` gRPC listener, retrying until ready.
pub(crate) async fn connect_extproc_client(port: u16) -> ExtProcClient {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(channel) = Channel::from_shared(format!("http://127.0.0.1:{port}"))
            .expect("uri")
            .connect()
            .await
        {
            return ExtProcClient::new(channel)
                .max_decoding_message_size(DEFAULT_GRPC_MAX_RECV_MSG_SIZE)
                .max_encoding_message_size(DEFAULT_GRPC_MAX_RECV_MSG_SIZE);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "praxis-extproc never accepted connections on 127.0.0.1:{port}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}


/// Minimal serving config on free ports for the binary max-recv regression.
///
/// `metrics_auth` is off: this process has no kubeconfig, and the default
/// `enabled: true` exits before the gRPC listener starts.
pub(crate) fn write_grpc_max_recv_config(grpc: u16, health: u16, metrics: u16) -> PathBuf {
    let yaml = format!(
        "server:\n  grpc_address: \"127.0.0.1:{grpc}\"\n  health_address: \"127.0.0.1:{health}\"\n  \
         metrics_address: \"127.0.0.1:{metrics}\"\n  metrics_auth:\n    enabled: false\n  tls:\n    mode: none\n\
         filter_chains:\n  - name: test\n    filters:\n      - filter: request_id\n\
         insecure_options:\n  allow_unbounded_body: true\n"
    );
    let path = std::env::temp_dir().join(format!(
        "praxis-extproc-grpc-max-recv-{}-{grpc}.yaml",
        std::process::id()
    ));
    std::fs::write(&path, yaml).expect("write server config");
    path
}


pub(crate) fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("find a free port")
}


/// Reap the test process even if an assertion or graceful shutdown fails.
pub(crate) struct BinaryChild(pub(crate) std::process::Child);


impl Drop for BinaryChild {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}


#[cfg(unix)]
pub(crate) async fn stop_binary(child: &mut BinaryChild) {
    let signal = Command::new("kill")
        .args(["-TERM", &child.0.id().to_string()])
        .status()
        .expect("signal the test server");
    assert!(signal.success(), "SIGTERM must reach the server");
    let status = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().expect("poll server exit") {
                break status;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("server must drain within ten seconds after its client closes");
    assert!(status.success(), "graceful shutdown must succeed: {status}");
}


#[cfg(not(unix))]
pub(crate) async fn stop_binary(child: &mut BinaryChild) {
    child.0.kill().expect("kill test server");
    child.0.wait().expect("reap test server");
}


pub(crate) async fn send_headers_only(client: &mut ExtProcClient, method: &str, path: &str) -> Vec<ProcessingResponse> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers(method, path, true))
        .await
        .expect("send headers");

    drop(tx);
    collect_responses(&mut inbound).await
}


pub(crate) async fn send_full_request(
    client: &mut ExtProcClient,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<ProcessingResponse> {
    send_full_request_result(client, method, path, body)
        .await
        .expect("process call failed")
}


pub(crate) async fn send_full_request_result(
    client: &mut ExtProcClient,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Vec<ProcessingResponse>, tonic::Status> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await?;
    let mut inbound = response.into_inner();

    let has_body = !body.is_empty();

    tx.send(make_request_headers(method, path, !has_body))
        .await
        .expect("send headers");

    if has_body {
        tx.send(ProcessingRequest {
            request: Some(ReqVariant::RequestBody(HttpBody {
                body: body.to_vec(),
                end_of_stream: true,
            })),
            ..Default::default()
        })
        .await
        .expect("send body");
    }

    drop(tx);
    collect_responses_result(&mut inbound).await
}


pub(crate) async fn send_full_roundtrip(
    client: &mut ExtProcClient,
    method: &str,
    path: &str,
) -> Vec<ProcessingResponse> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers(method, path, true))
        .await
        .expect("send request headers");

    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![make_header(":status", "200")],
            }),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send response headers");

    drop(tx);
    collect_responses(&mut inbound).await
}


pub(crate) fn make_request_headers(method: &str, path: &str, end_of_stream: bool) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(ReqVariant::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![
                    make_header(":method", method),
                    make_header(":path", path),
                    make_header(":authority", "localhost"),
                    make_header(":scheme", "http"),
                ],
            }),
            end_of_stream,
        })),
        ..Default::default()
    }
}


pub(crate) fn make_response_headers(status: u32, end_of_stream: bool) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(ReqVariant::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![make_header(":status", &status.to_string())],
            }),
            end_of_stream,
        })),
        ..Default::default()
    }
}


pub(crate) fn make_response_body(body: &[u8], end_of_stream: bool) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: body.to_vec(),
            end_of_stream,
        })),
        ..Default::default()
    }
}


/// Attach a first-message `protocol_config` selecting `response_body_mode`.
pub(crate) fn with_response_body_mode(
    mut msg: ProcessingRequest,
    response_body_mode: BodySendMode,
) -> ProcessingRequest {
    msg.protocol_config = Some(praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration {
        request_body_mode: BodySendMode::None as i32,
        response_body_mode: response_body_mode as i32,
        send_body_without_waiting_for_header_response: false,
    });
    msg
}


pub(crate) async fn open_stream(
    client: &mut ExtProcClient,
) -> (
    tokio::sync::mpsc::Sender<ProcessingRequest>,
    tonic::Streaming<ProcessingResponse>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = client
        .process(ReceiverStream::new(rx))
        .await
        .expect("process call failed")
        .into_inner();
    (tx, stream)
}


pub(crate) fn make_header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_owned(),
        raw_value: Vec::new(),
    }
}


pub(crate) async fn collect_responses(inbound: &mut tonic::Streaming<ProcessingResponse>) -> Vec<ProcessingResponse> {
    collect_responses_result(inbound).await.expect("collect responses")
}


pub(crate) async fn collect_responses_result(
    inbound: &mut tonic::Streaming<ProcessingResponse>,
) -> Result<Vec<ProcessingResponse>, tonic::Status> {
    let mut responses = Vec::new();
    let timeout = tokio::time::Duration::from_secs(2);

    loop {
        match tokio::time::timeout(timeout, inbound.message()).await {
            Ok(Ok(Some(resp))) => responses.push(resp),
            // Idle timeout (`Err`) means the stream went quiet.
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(status)) => return Err(status),
        }
    }

    Ok(responses)
}


/// Collect streamed body chunks from a response stream.
///
/// Returns `(chunks, reassembled_body)` where chunks is the vector of
/// `StreamedBodyResponse` messages and `reassembled_body` is all chunk
/// bodies concatenated.
///
/// Panics if stream errors, times out, or exceeds `max_chunks` without EOS.
pub(crate) async fn collect_streamed_chunks<F>(
    response_stream: &mut tonic::Streaming<ProcessingResponse>,
    extract_fn: F,
    max_chunks: usize,
) -> (
    Vec<praxis_proto::envoy::service::ext_proc::v3::StreamedBodyResponse>,
    Vec<u8>,
)
where
    F: Fn(&ProcessingResponse) -> Option<&praxis_proto::envoy::service::ext_proc::v3::StreamedBodyResponse>,
{
    let mut chunks = Vec::new();
    let mut received_body = Vec::new();

    loop {
        assert!(
            chunks.len() < max_chunks,
            "received {max_chunks} chunks without end_of_stream"
        );

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MILLIS),
            response_stream.message(),
        )
        .await;

        match outcome {
            Ok(Ok(Some(msg))) => {
                if let Some(s) = extract_fn(&msg) {
                    received_body.extend_from_slice(&s.body);
                    let is_eos = s.end_of_stream;
                    chunks.push(s.clone());
                    if is_eos {
                        break;
                    }
                } else {
                    panic!("expected StreamedResponse, got: {msg:?}");
                }
            },
            Ok(Ok(None)) => panic!("stream closed before EOS"),
            Ok(Err(err)) => panic!("stream error: {err}"),
            Err(_) => panic!("timed out waiting for chunk"),
        }
    }

    (chunks, received_body)
}


pub(crate) fn has_request_headers_response(responses: &[ProcessingResponse]) -> bool {
    responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::RequestHeaders(_))))
}


pub(crate) fn extract_all_set_headers(responses: &[ProcessingResponse]) -> Vec<HeaderValue> {
    let mut headers = Vec::new();
    for r in responses {
        let mutation = match &r.response {
            Some(RespVariant::RequestHeaders(h)) => h.response.as_ref().and_then(|c| c.header_mutation.as_ref()),
            Some(RespVariant::RequestBody(b)) => b.response.as_ref().and_then(|c| c.header_mutation.as_ref()),
            _ => None,
        };
        if let Some(m) = mutation {
            for hvo in &m.set_headers {
                if let Some(hv) = &hvo.header {
                    headers.push(hv.clone());
                }
            }
        }
    }
    headers
}
