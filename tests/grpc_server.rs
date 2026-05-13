// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! In-process gRPC integration tests for the ExtProc server.
//!
//! Starts a tonic server on a random port, sends `ProcessingRequest`
//! messages via a tonic client, and verifies responses.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "tests"
)]
#![allow(missing_docs, reason = "test module")]

use praxis_extproc::{config, server::PraxisExtProc};
use praxis_filter::FilterRegistry;
use praxis_proto::envoy::service::common::v3::HeaderValue;
use praxis_proto::envoy::service::ext_proc::v3::{
    HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, ProcessingResponse,
    external_processor_server::ExternalProcessorServer,
    processing_request::Request as ReqVariant,
    processing_response::Response as RespVariant,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[tokio::test]
async fn headers_only_request_returns_continue() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let responses = send_headers_only(&mut client, "GET", "/").await;

    assert!(!responses.is_empty(), "should receive at least one response");
    assert!(
        has_request_headers_response(&responses),
        "should contain a request headers response"
    );
}

#[tokio::test]
async fn headers_filter_adds_response_header() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;

    let responses = send_full_request(&mut client, "GET", "/", &[]).await;

    let mutations = extract_all_set_headers(&responses);
    let has_x_test = mutations.iter().any(|h| h.key == "X-Test" && h.value == "extproc");

    assert!(has_x_test, "X-Test header should be added by headers filter");
}

#[tokio::test]
async fn request_with_body_processes_successfully() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let body = b"hello world";
    let responses = send_full_request(&mut client, "POST", "/submit", body).await;

    assert!(!responses.is_empty(), "should receive responses for body request");
}

#[tokio::test]
async fn guardrails_filter_rejects_blocked_content() {
    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let body = b"DROP TABLE users";
    let responses = send_full_request(&mut client, "POST", "/api", body).await;

    let has_immediate = responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_))));

    assert!(has_immediate, "guardrails should reject with ImmediateResponse");
}

#[tokio::test]
async fn guardrails_filter_allows_clean_content() {
    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let body = b"SELECT name FROM users";
    let responses = send_full_request(&mut client, "POST", "/api", body).await;

    let has_immediate = responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_))));

    assert!(!has_immediate, "clean content should not be rejected");
}

#[tokio::test]
async fn response_phase_processes_headers() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let responses = send_full_roundtrip(&mut client, "GET", "/").await;

    let has_response_headers = responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::ResponseHeaders(_))));

    assert!(has_response_headers, "should include response headers processing");
}

#[tokio::test]
async fn multiple_streams_are_independent() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    for i in 0..5 {
        let responses = send_headers_only(&mut client, "GET", &format!("/req-{i}")).await;

        assert!(!responses.is_empty(), "stream {i} should produce responses");
    }
}

#[tokio::test]
async fn empty_body_request_succeeds() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let responses = send_full_request(&mut client, "POST", "/empty", &[]).await;

    assert!(!responses.is_empty(), "empty body request should succeed");
}

#[tokio::test]
async fn trailers_passthrough() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("GET", "/", true))
        .await
        .expect("send headers");

    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(
            praxis_proto::envoy::service::ext_proc::v3::HttpTrailers { trailers: None },
        )),
        ..Default::default()
    })
    .await
    .expect("send trailers");

    let trailer_resp = inbound.message().await.expect("receive").expect("response");

    assert!(
        matches!(&trailer_resp.response, Some(RespVariant::RequestTrailers(_))),
        "should echo back request trailers response"
    );
}

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

const HEADERS_ONLY_CONFIG: &str = r#"
filter_chains:
  - name: test
    filters:
      - filter: request_id
insecure_options:
  allow_unbounded_body: true
"#;

const HEADERS_CONFIG: &str = r#"
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

const GUARDRAILS_CONFIG: &str = r#"
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

const RESPONSE_HEADER_CONFIG: &str = r#"
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

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

type ExtProcClient =
    praxis_proto::envoy::service::ext_proc::v3::external_processor_client::ExternalProcessorClient<Channel>;

async fn start_server(config_yaml: &str) -> (ExtProcClient, tokio::sync::oneshot::Sender<()>) {
    let cfg: config::ExtProcConfig = serde_yaml::from_str(config_yaml).expect("parse config");
    let registry = FilterRegistry::with_builtins();
    let pipeline = config::build_pipeline(&cfg, &registry).expect("build pipeline");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let svc = PraxisExtProc::new(pipeline);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        Server::builder()
            .add_service(ExternalProcessorServer::new(svc))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async { drop(shutdown_rx.await) },
            )
            .await
            .expect("server failed");
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("uri")
        .connect()
        .await
        .expect("connect");
    let client = ExtProcClient::new(channel);

    (client, shutdown_tx)
}

async fn send_headers_only(client: &mut ExtProcClient, method: &str, path: &str) -> Vec<ProcessingResponse> {
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

async fn send_full_request(
    client: &mut ExtProcClient,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<ProcessingResponse> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
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
    collect_responses(&mut inbound).await
}

async fn send_full_roundtrip(client: &mut ExtProcClient, method: &str, path: &str) -> Vec<ProcessingResponse> {
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

fn make_request_headers(method: &str, path: &str, end_of_stream: bool) -> ProcessingRequest {
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

fn make_header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_owned(),
        raw_value: Vec::new(),
    }
}

async fn collect_responses(
    inbound: &mut tonic::Streaming<ProcessingResponse>,
) -> Vec<ProcessingResponse> {
    let mut responses = Vec::new();
    let timeout = tokio::time::Duration::from_secs(2);

    while let Ok(Ok(Some(resp))) = tokio::time::timeout(timeout, inbound.message()).await {
        responses.push(resp);
    }

    responses
}

fn has_request_headers_response(responses: &[ProcessingResponse]) -> bool {
    responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::RequestHeaders(_))))
}

fn extract_all_set_headers(responses: &[ProcessingResponse]) -> Vec<HeaderValue> {
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
