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
    clippy::tests_outside_test_module,
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
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        HeaderMap, HttpBody, HttpHeaders, HttpTrailers, ProcessingRequest, ProcessingResponse,
        external_processor_server::ExternalProcessorServer, processing_request::Request as ReqVariant,
        processing_response::Response as RespVariant,
    },
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Server};

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

const TIMEOUT_MILLIS: u64 = 500;

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
    let has_x_test = mutations.iter().any(|h| h.key == "x-test" && h.value == "extproc");

    assert!(has_x_test, "x-test header should be added by headers filter");
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
        request: Some(ReqVariant::RequestTrailers(HttpTrailers { trailers: None })),
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

#[tokio::test]
async fn large_body_request_succeeds() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let body = vec![b'x'; 65_536];
    let responses = send_full_request(&mut client, "POST", "/large", &body).await;

    let has_immediate = responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_))));

    assert!(!has_immediate, "large body should not be rejected");
    assert!(!responses.is_empty(), "should produce responses for large body");
}

#[tokio::test]
async fn response_trailers_passthrough() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.expect("send");
    drop(inbound.message().await);

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

    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send response trailers");

    let trailer_resp = inbound.message().await.expect("receive").expect("response");

    assert!(
        matches!(&trailer_resp.response, Some(RespVariant::ResponseTrailers(_))),
        "should echo back response trailers"
    );
}

#[tokio::test]
async fn body_with_headers_deferred_response() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("POST", "/api", false))
        .await
        .expect("send headers");

    let header_resp = inbound.message().await.expect("receive").expect("response");
    assert!(
        matches!(&header_resp.response, Some(RespVariant::RequestHeaders(_))),
        "should get immediate headers response before body"
    );

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"test payload".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send body");

    let body_resp = inbound.message().await.expect("receive").expect("body response");
    assert!(
        !matches!(&body_resp.response, Some(RespVariant::ImmediateResponse(_))),
        "should not reject clean body"
    );
}

#[tokio::test]
async fn response_body_with_header_mutations() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.expect("send");
    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![make_header(":status", "200"), make_header("content-type", "text/plain")],
            }),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .expect("send response headers");

    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"response data".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send response body");

    let body_resp = inbound.message().await.expect("receive").expect("response body");
    assert!(
        !matches!(&body_resp.response, Some(RespVariant::ImmediateResponse(_))),
        "should not reject response body"
    );
}

#[tokio::test]
async fn multi_chunk_body_accumulation() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("POST", "/chunked", false))
        .await
        .expect("send headers");

    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk1".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .expect("send chunk 1");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk2".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send chunk 2");

    let body_resp = inbound.message().await.expect("receive").expect("body response");

    assert!(
        !matches!(&body_resp.response, Some(RespVariant::ImmediateResponse(_))),
        "accumulated chunks should not be rejected"
    );
}

#[tokio::test]
async fn multi_chunk_response_body() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.expect("send");
    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![make_header(":status", "200")],
            }),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .expect("send response headers");

    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"part1".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .expect("send response body chunk 1");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"part2".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send response body chunk 2");

    let resp = inbound.message().await.expect("receive").expect("response");

    assert!(
        !matches!(&resp.response, Some(RespVariant::ImmediateResponse(_))),
        "multi-chunk response body should succeed"
    );
}

#[tokio::test]
async fn raw_value_header_parsing() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![
                    HeaderValue {
                        key: ":method".to_owned(),
                        value: String::new(),
                        raw_value: b"GET".to_vec(),
                    },
                    HeaderValue {
                        key: ":path".to_owned(),
                        value: String::new(),
                        raw_value: b"/raw-test".to_vec(),
                    },
                ],
            }),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send headers with raw_value");

    let resp = inbound.message().await.expect("receive").expect("response");

    assert!(
        matches!(&resp.response, Some(RespVariant::RequestHeaders(_))),
        "raw_value headers should parse correctly"
    );
}

#[tokio::test]
async fn guardrails_rejects_body_in_buffered_mode() {
    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("POST", "/api", false))
        .await
        .expect("send headers");

    drop(inbound.message().await);

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"DROP TABLE users".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send body");

    let body_resp = inbound.message().await.expect("receive").expect("body response");

    assert!(
        matches!(&body_resp.response, Some(RespVariant::ImmediateResponse(_))),
        "guardrails should reject via ImmediateResponse in buffered mode"
    );
}

#[tokio::test]
async fn unconditional_branch_adds_headers_from_branch_chain() {
    let (mut client, _shutdown) = start_server(UNCONDITIONAL_BRANCH_CONFIG).await;

    let responses = send_headers_only(&mut client, "GET", "/").await;

    let mutations = extract_all_set_headers(&responses);
    let has_main = mutations.iter().any(|h| h.key == "x-main");
    let has_branch = mutations.iter().any(|h| h.key == "x-branch-applied");

    assert!(has_main, "main chain header should be present");
    assert!(has_branch, "branch chain header should be present");
}

#[tokio::test]
async fn conditional_terminal_branch_rejects_matching_request() {
    let (mut client, _shutdown) = start_server(CONDITIONAL_TERMINAL_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);

    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![
                    make_header(":method", "GET"),
                    make_header(":path", "/"),
                    make_header("x-danger", "true"),
                ],
            }),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send headers");

    let resp = inbound.message().await.expect("receive").expect("response");

    assert!(
        matches!(&resp.response, Some(RespVariant::ImmediateResponse(_))),
        "terminal branch should produce ImmediateResponse for flagged request"
    );
}

#[tokio::test]
async fn conditional_terminal_branch_allows_clean_request() {
    let (mut client, _shutdown) = start_server(CONDITIONAL_TERMINAL_CONFIG).await;

    let responses = send_headers_only(&mut client, "GET", "/").await;

    let has_immediate = responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_))));

    assert!(
        !has_immediate,
        "clean request should not be rejected by terminal branch"
    );
}

#[tokio::test]
async fn duplicate_eos_in_request_headers_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some(), "first request should get response");

    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for duplicate EOS rejection")
    .expect_err("duplicate EOS was accepted");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "should be InvalidArgument");
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "error should mention out-of-order or invalid transition: {}",
        err.message()
    );
    assert!(
        err.message().contains("Request") || err.message().contains("phase"),
        "error should mention request or phase: {}",
        err.message()
    );
}

#[tokio::test]
async fn duplicate_eos_in_request_body_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("POST", "/", false)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some(), "header phase should get response");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk1".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let resp2 = response_stream.message().await.unwrap();
    assert!(resp2.is_some(), "first body should get response");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk2".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error");

    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "should be InvalidArgument");
    assert!(
        err.message().contains("after end_of_stream"),
        "error should mention message after EOS: {}",
        err.message()
    );
    assert!(
        err.message().contains("RequestBody"),
        "error should mention phase: {}",
        err.message()
    );
}

#[tokio::test]
async fn duplicate_eos_in_response_headers_rejected() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some(), "request should get response");

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
    .unwrap();

    let resp2 = response_stream.message().await.unwrap();
    assert!(resp2.is_some(), "response headers should get response");

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
    .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error");

    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "should be InvalidArgument");
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "error should mention out-of-order or invalid transition: {}",
        err.message()
    );
    assert!(
        err.message().contains("Response") || err.message().contains("phase"),
        "error should mention response or phase: {}",
        err.message()
    );
}

#[tokio::test]
async fn initial_request_body_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    // Send RequestBody as first message (before RequestHeaders)
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"attacker data".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error")
    .expect_err("initial RequestBody should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "should reject body before headers: {}",
        err.message()
    );
}

#[tokio::test]
async fn initial_request_trailers_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    // Send RequestTrailers as first message (before RequestHeaders)
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(HttpTrailers {
            trailers: Some(HeaderMap {
                headers: vec![make_header("x-trailer", "value")],
            }),
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error")
    .expect_err("initial RequestTrailers should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "should reject trailers before headers: {}",
        err.message()
    );
}

#[tokio::test]
async fn response_body_before_response_headers_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    // Send RequestHeaders first (required)
    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some());

    // Send ResponseBody before ResponseHeaders (invalid)
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"early body".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error")
    .expect_err("ResponseBody before ResponseHeaders should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "should reject response body before response headers: {}",
        err.message()
    );
}

#[tokio::test]
async fn duplicate_non_eos_request_headers_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    // First RequestHeaders (non-EOS)
    tx.send(make_request_headers("GET", "/", false)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some());

    // Duplicate RequestHeaders (non-EOS) - should be rejected
    tx.send(make_request_headers("GET", "/attacker", false)).await.unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error")
    .expect_err("duplicate non-EOS RequestHeaders should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "should reject duplicate request headers: {}",
        err.message()
    );
}

#[tokio::test]
async fn duplicate_non_eos_response_headers_rejected() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    // RequestHeaders first
    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some());

    // First ResponseHeaders (non-EOS)
    tx.send(make_response_headers(200, false)).await.unwrap();

    let resp2 = response_stream.message().await.unwrap();
    assert!(resp2.is_some());

    // Duplicate ResponseHeaders (non-EOS) - should be rejected
    tx.send(make_response_headers(500, false)).await.unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error")
    .expect_err("duplicate non-EOS ResponseHeaders should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("out-of-order") || err.message().contains("invalid"),
        "should reject duplicate response headers: {}",
        err.message()
    );
}

#[tokio::test]
async fn duplicate_eos_in_response_body_rejected() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);

    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let resp1 = response_stream.message().await.unwrap();
    assert!(resp1.is_some(), "request should get response");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![make_header(":status", "200")],
            }),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    // Receive response headers response
    let resp2 = response_stream.message().await.unwrap();
    assert!(resp2.is_some(), "response headers should get response");

    // Send first response body with EOS
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"body1".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let resp3 = response_stream.message().await.unwrap();
    assert!(resp3.is_some(), "first body should get response");

    // Send duplicate response body with EOS (invalid)
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"body2".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for error");

    let err = result.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "should be InvalidArgument");
    assert!(
        err.message().contains("after end_of_stream"),
        "error should mention message after EOS: {}",
        err.message()
    );
    assert!(
        err.message().contains("ResponseBody"),
        "error should mention phase: {}",
        err.message()
    );
}

/// Regression for #58: in `FULL_DUPLEX_STREAMED`, Envoy re-delivers the final
/// body chunk after `end_of_stream` (observed with >1MB bodies on Envoy 1.35+).
/// The re-delivery must be a no-op that keeps the stream usable, not an error.
#[tokio::test]
async fn duplicate_eos_in_fds_request_body_ignored() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/upload", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4, // FULL_DUPLEX_STREAMED
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let body_data = vec![0_u8; 1024];
    let final_chunk = ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: body_data.clone(),
            end_of_stream: true,
        })),
        ..Default::default()
    };
    tx.send(final_chunk.clone()).await.unwrap();

    // Header response, then the streamed body response for the first EOS chunk.
    let hdr_msg = response_stream.message().await.unwrap();
    assert!(
        matches!(hdr_msg.and_then(|m| m.response), Some(RespVariant::RequestHeaders(_))),
        "first response should be HeadersResponse"
    );
    let body_msg = response_stream.message().await.unwrap();
    assert!(
        matches!(body_msg.and_then(|m| m.response), Some(RespVariant::RequestBody(_))),
        "second response should be the streamed body response"
    );

    // Re-deliver the final chunk, then close the request stream.
    tx.send(final_chunk).await.unwrap();
    drop(tx);

    // The duplicate is a no-op: the stream stays healthy and closes cleanly
    // (Ok) with no ImmediateResponse — never an error tearing it down.
    loop {
        let next = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MILLIS),
            response_stream.message(),
        )
        .await
        .expect("timed out waiting for stream to drain")
        .expect("duplicate FDS end-of-stream chunk must not error the stream");

        match next {
            None => break,
            Some(msg) => assert!(
                !matches!(&msg.response, Some(RespVariant::ImmediateResponse(_))),
                "duplicate chunk must not produce an ImmediateResponse, got: {msg:?}"
            ),
        }
    }
}

#[tokio::test]
async fn repro_ap_post_eos_body() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("POST", "/", false)).await.unwrap();
    assert!(
        response_stream.message().await.unwrap().is_some(),
        "headers phase should get a response"
    );

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk1".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();
    assert!(
        response_stream.message().await.unwrap().is_some(),
        "first body EOS should run the pipeline and respond"
    );

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"AFTER_EOS_INJECTED".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await;

    match outcome {
        Ok(Err(err)) => {
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "ap-post-eos-body: expected InvalidArgument, got {}: {}",
                err.code(),
                err.message()
            );
        },
        Ok(Ok(msg)) => panic!(
            "ap-post-eos-body: expected InvalidArgument after post-EOS body(end_of_stream=false); got success message: {msg:?}"
        ),
        Err(_) => panic!(
            "ap-post-eos-body: timed out waiting for rejection; server accepted body(end_of_stream=false) after EOS and sent no error (bytes still appended to request_body)"
        ),
    }
}


#[tokio::test]
async fn repro_ap_post_eos_headers() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("GET", "/first", true)).await.unwrap();
    assert!(
        response_stream.message().await.unwrap().is_some(),
        "first headers+EOS should run the pipeline and respond"
    );

    tx.send(make_request_headers("POST", "/injected-after-eos", false))
        .await
        .unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await;

    match outcome {
        Ok(Err(err)) => {
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "ap-post-eos-headers: expected InvalidArgument, got {}: {}",
                err.code(),
                err.message()
            );
        },
        Ok(Ok(msg)) => panic!(
            "ap-post-eos-headers: expected InvalidArgument after post-EOS headers(end_of_stream=false); got success message instead (state.request was overwritten). response={msg:?}"
        ),
        Err(_) => panic!("ap-post-eos-headers: timed out waiting for stream result"),
    }
}

#[tokio::test]
async fn wrong_wire_mode_unsupported_buffered_partial_rejected() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/submit", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 3, // BUFFERED_PARTIAL — not implemented
        response_body_mode: 3,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await;

    match outcome {
        Ok(Err(err)) => {
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "expected InvalidArgument for BUFFERED_PARTIAL mode, got {}: {}",
                err.code(),
                err.message()
            );
            assert!(
                err.message().contains("BUFFERED_PARTIAL") || err.message().contains("not yet implemented"),
                "error message should mention BUFFERED_PARTIAL or not implemented, got: {}",
                err.message()
            );
        },
        Ok(Ok(Some(msg))) => {
            panic!("expected InvalidArgument for unsupported mode, got success response: {msg:?}");
        },
        Ok(Ok(None)) => panic!("stream closed without error"),
        Err(_) => panic!("timed out waiting for rejection"),
    }
}

#[tokio::test]
async fn unsupported_response_body_mode_rejected() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/submit", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,
        response_body_mode: 3, // BUFFERED_PARTIAL — not implemented
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await;

    match outcome {
        Ok(Err(err)) => {
            assert_eq!(
                err.code(),
                tonic::Code::InvalidArgument,
                "should reject with InvalidArgument"
            );
            assert!(
                err.message().contains("response_body_mode"),
                "error should identify response_body_mode field, got: {}",
                err.message()
            );
        },
        Ok(Ok(Some(msg))) => panic!("expected rejection for unsupported response_body_mode, got: {msg:?}"),
        Ok(Ok(None)) => panic!("stream closed without error"),
        Err(_) => panic!("timed out waiting for rejection"),
    }
}

#[tokio::test]
async fn empty_full_duplex_emits_streamed_eos() {
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/submit", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 4,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    // FDS: no response at header time

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: Vec::new(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    // First response: HeadersResponse (deferred from header phase)
    let hdr_msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for header response")
    .expect("header response stream error")
    .expect("stream closed before header response");
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::RequestHeaders(_))),
        "first response should be HeadersResponse, got: {hdr_msg:?}"
    );

    // Second response: StreamedBodyResponse with empty body + EOS
    let body_msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for body response")
    .expect("body response stream error")
    .expect("stream closed before body response");

    if let Some(RespVariant::RequestBody(b)) = &body_msg.response
        && let Some(body_mutation::Mutation::StreamedResponse(s)) = b
            .response
            .as_ref()
            .and_then(|c| c.body_mutation.as_ref())
            .and_then(|m| m.mutation.as_ref())
    {
        assert!(
            s.body.is_empty(),
            "empty FULL_DUPLEX streamed chunk should have empty body"
        );
        assert!(
            s.end_of_stream,
            "empty FULL_DUPLEX streamed chunk must set end_of_stream"
        );
    } else {
        panic!("expected StreamedBodyResponse for empty FULL_DUPLEX body, got: {body_msg:?}");
    }
}

#[tokio::test]
async fn full_duplex_single_chunk_request_body() {
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/upload", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    // FDS: no response at header time

    let body_data = vec![0_u8; 1024];
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: body_data.clone(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    // First response: HeadersResponse (deferred from header phase)
    let hdr_msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for header response")
    .expect("header response stream error")
    .expect("stream closed before header response");
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::RequestHeaders(_))),
        "first response should be HeadersResponse, got: {hdr_msg:?}"
    );

    // Second response: StreamedBodyResponse with body data + EOS
    let body_msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for body response")
    .expect("body response stream error")
    .expect("stream closed before body response");

    if let Some(RespVariant::RequestBody(b)) = &body_msg.response
        && let Some(body_mutation::Mutation::StreamedResponse(s)) = b
            .response
            .as_ref()
            .and_then(|c| c.body_mutation.as_ref())
            .and_then(|m| m.mutation.as_ref())
    {
        assert_eq!(s.body.len(), body_data.len(), "streamed chunk should contain full body");
        assert!(s.end_of_stream, "single chunk should set end_of_stream");
    } else {
        panic!("expected StreamedResponse for FULL_DUPLEX request body, got: {body_msg:?}");
    }
}

#[tokio::test]
async fn full_duplex_multi_chunk_request_body() {
    const MAX_CHUNKS: usize = 8;
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/upload", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    // FDS: no response at header time

    let body_data: Vec<u8> = (0_u32..100_000).map(|i| (i % 251) as u8).collect();
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: body_data.clone(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    // First response: HeadersResponse (deferred from header phase)
    let hdr_msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for header response")
    .expect("header response stream error")
    .expect("stream closed before header response");
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::RequestHeaders(_))),
        "first response should be HeadersResponse, got: {hdr_msg:?}"
    );

    // Remaining responses: streamed body chunks
    let (chunks, received_body) = collect_streamed_chunks(
        &mut response_stream,
        |msg| {
            msg.response.as_ref().and_then(|r| match r {
                RespVariant::RequestBody(b) => {
                    b.response
                        .as_ref()
                        .and_then(|c| c.body_mutation.as_ref())
                        .and_then(|m| match &m.mutation {
                            Some(body_mutation::Mutation::StreamedResponse(s)) => Some(s),
                            _ => None,
                        })
                },
                _ => None,
            })
        },
        MAX_CHUNKS,
    )
    .await;

    assert!(chunks.len() > 1, "100KB body should produce multiple chunks");
    assert_eq!(received_body, body_data, "streamed bytes should match input");
    assert!(
        chunks.last().unwrap().end_of_stream,
        "final chunk must set end_of_stream"
    );
}

#[tokio::test]
async fn full_duplex_headers_prepended_only_to_first_chunk() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/upload", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    // FDS: no response at header time. Send three discrete body messages;
    // small bodies keep a 1:1 message-to-chunk mapping (no server-side split).
    let eos_flags = [false, false, true];
    for (i, eos) in eos_flags.iter().enumerate() {
        tx.send(ProcessingRequest {
            request: Some(ReqVariant::RequestBody(HttpBody {
                body: vec![u8::try_from(i).expect("should expect chunks"); 16],
                end_of_stream: *eos,
            })),
            ..Default::default()
        })
        .await
        .unwrap();
    }

    // First chunk: deferred RequestHeaders must precede the body response.
    let hdr_msg = next_full_duplex_msg(&mut response_stream).await;
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::RequestHeaders(_))),
        "first response should be deferred HeadersResponse, got: {hdr_msg:?}"
    );
    let first_body = next_full_duplex_msg(&mut response_stream).await;
    assert_eq!(
        streamed_eos(&first_body),
        Some(false),
        "first chunk must propagate source EOS=false, got: {first_body:?}"
    );

    // Subsequent chunks: body responses only, never preceded by RequestHeaders,
    // and each must propagate the source chunk's own end_of_stream flag.
    for (chunk, &eos) in eos_flags.iter().enumerate().skip(1) {
        let msg = next_full_duplex_msg(&mut response_stream).await;
        assert_eq!(
            streamed_eos(&msg),
            Some(eos),
            "chunk {chunk} should propagate source EOS={eos}, got: {msg:?}"
        );
    }
}

/// Extract `end_of_stream` from a streamed request-body response, if present.
fn streamed_eos(msg: &ProcessingResponse) -> Option<bool> {
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
async fn next_full_duplex_msg(stream: &mut tonic::Streaming<ProcessingResponse>) -> ProcessingResponse {
    tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MILLIS), stream.message())
        .await
        .expect("timed out waiting for response")
        .expect("response stream error")
        .expect("stream closed before response")
}

#[tokio::test]
async fn full_duplex_response_body() {
    const MAX_CHUNKS: usize = 8;
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("GET", "/", true);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,
        response_body_mode: 4,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let _header_resp = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for header response")
    .expect("header response stream error")
    .expect("stream closed before header response");

    tx.send(make_response_headers(200, false)).await.unwrap();

    // FDS response: no response at response header time

    let body_data: Vec<u8> = (0_u32..100_000).map(|i| (i % 251) as u8).collect();
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: body_data.clone(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    // First response after body EOS: ResponseHeaders (deferred)
    let hdr_msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for response header response")
    .expect("response header stream error")
    .expect("stream closed before response header response");
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::ResponseHeaders(_))),
        "first response should be ResponseHeaders, got: {hdr_msg:?}"
    );

    // Remaining responses: streamed body chunks
    let (chunks, received_body) = collect_streamed_chunks(
        &mut response_stream,
        |msg| {
            msg.response.as_ref().and_then(|r| match r {
                RespVariant::ResponseBody(b) => {
                    b.response
                        .as_ref()
                        .and_then(|c| c.body_mutation.as_ref())
                        .and_then(|m| match &m.mutation {
                            Some(body_mutation::Mutation::StreamedResponse(s)) => Some(s),
                            _ => None,
                        })
                },
                _ => None,
            })
        },
        MAX_CHUNKS,
    )
    .await;

    assert!(chunks.len() > 1, "100KB response body should produce multiple chunks");
    assert_eq!(received_body, body_data, "streamed bytes should match input");
    assert!(
        chunks.last().unwrap().end_of_stream,
        "final chunk must set end_of_stream"
    );
}

#[tokio::test]
async fn streamed_single_chunk_request_body() {
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/upload", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 1, // STREAMED
        response_body_mode: 1,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let _header_resp = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for header response")
    .expect("header response stream error")
    .expect("stream closed before header response");

    let body_data = b"streamed body content";
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: body_data.to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out")
    .expect("stream error")
    .expect("stream closed");

    if let Some(RespVariant::RequestBody(b)) = &msg.response
        && let Some(body_mutation::Mutation::Body(bytes)) = b
            .response
            .as_ref()
            .and_then(|c| c.body_mutation.as_ref())
            .and_then(|m| m.mutation.as_ref())
    {
        assert_eq!(bytes, body_data, "STREAMED chunk should echo body data");
    } else {
        panic!("expected BodyMutation::Body for STREAMED request body, got: {msg:?}");
    }
}

#[tokio::test]
async fn streamed_multi_chunk_request_body() {
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/upload", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 1, // STREAMED
        response_body_mode: 1,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let _header_resp = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for header response")
    .expect("header response stream error")
    .expect("stream closed before header response");

    let chunks: &[&[u8]] = &[b"chunk-1-", b"chunk-2-", b"chunk-3"];
    let mut received_body = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;

        tx.send(ProcessingRequest {
            request: Some(ReqVariant::RequestBody(HttpBody {
                body: chunk.to_vec(),
                end_of_stream: is_last,
            })),
            ..Default::default()
        })
        .await
        .unwrap();

        // Half-duplex: must get a response before sending next chunk
        let msg = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MILLIS),
            response_stream.message(),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for response to chunk {i}"))
        .unwrap_or_else(|e| panic!("stream error on chunk {i}: {e}"))
        .unwrap_or_else(|| panic!("stream closed before response to chunk {i}"));

        if let Some(RespVariant::RequestBody(b)) = &msg.response {
            if let Some(body_mutation::Mutation::Body(bytes)) = b
                .response
                .as_ref()
                .and_then(|c| c.body_mutation.as_ref())
                .and_then(|m| m.mutation.as_ref())
            {
                received_body.extend_from_slice(bytes);
            }
        } else {
            panic!("expected RequestBody response for chunk {i}, got: {msg:?}");
        }
    }

    let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    assert_eq!(received_body, expected, "reassembled body should match input chunks");
}

#[tokio::test]
async fn streamed_empty_body() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/empty", false);
    headers.protocol_config = Some(praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration {
        request_body_mode: 1, // STREAMED
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let _header_resp = response_stream.message().await.unwrap();

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: Vec::new(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let msg = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out")
    .expect("stream error")
    .expect("stream closed");

    assert!(
        matches!(&msg.response, Some(RespVariant::RequestBody(_))),
        "empty STREAMED body should still produce a RequestBody response"
    );
}

#[tokio::test]
async fn streamed_response_body() {
    use praxis_proto::envoy::service::ext_proc::v3::{ProtocolConfiguration, body_mutation};

    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("GET", "/", true);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,
        response_body_mode: 1, // STREAMED
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    let _req_header_resp = response_stream.message().await.unwrap();

    tx.send(make_response_headers(200, false)).await.unwrap();
    let _resp_header_resp = response_stream.message().await.unwrap();

    let chunks: &[&[u8]] = &[b"resp-1-", b"resp-2"];
    let mut received_body = Vec::new();

    for (i, chunk) in chunks.iter().enumerate() {
        let is_last = i == chunks.len() - 1;

        tx.send(ProcessingRequest {
            request: Some(ReqVariant::ResponseBody(HttpBody {
                body: chunk.to_vec(),
                end_of_stream: is_last,
            })),
            ..Default::default()
        })
        .await
        .unwrap();

        let msg = tokio::time::timeout(
            std::time::Duration::from_millis(TIMEOUT_MILLIS),
            response_stream.message(),
        )
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for response body chunk {i}"))
        .unwrap_or_else(|e| panic!("stream error on response body chunk {i}: {e}"))
        .unwrap_or_else(|| panic!("stream closed before response body chunk {i}"));

        if let Some(RespVariant::ResponseBody(b)) = &msg.response {
            if let Some(body_mutation::Mutation::Body(bytes)) = b
                .response
                .as_ref()
                .and_then(|c| c.body_mutation.as_ref())
                .and_then(|m| m.mutation.as_ref())
            {
                received_body.extend_from_slice(bytes);
            }
        } else {
            panic!("expected ResponseBody for chunk {i}, got: {msg:?}");
        }
    }

    let expected: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    assert_eq!(received_body, expected, "reassembled response body should match");
}

#[tokio::test]
async fn streamed_guardrails_rejects_on_matching_chunk() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/api", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 1, // STREAMED
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();
    let _header_resp = response_stream.message().await.unwrap();

    // First chunk: clean content — should get a normal body response
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"SELECT name FROM users".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let msg1 = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out on chunk 1")
    .expect("stream error on chunk 1")
    .expect("stream closed on chunk 1");

    assert!(
        matches!(&msg1.response, Some(RespVariant::RequestBody(_))),
        "clean chunk should produce RequestBody, got: {msg1:?}"
    );

    // Second chunk: blocked content — should get ImmediateResponse
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"DROP TABLE users".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let msg2 = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out on chunk 2")
    .expect("stream error on chunk 2")
    .expect("stream closed on chunk 2");

    assert!(
        matches!(&msg2.response, Some(RespVariant::ImmediateResponse(_))),
        "blocked chunk should produce ImmediateResponse, got: {msg2:?}"
    );
}

#[tokio::test]
async fn streamed_deferred_mutation_on_first_chunk_only() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/submit", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 1, // STREAMED
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.unwrap();

    // HeadersResponse should carry X-Test mutation (sent at header time)
    let header_resp = response_stream.message().await.unwrap().unwrap();
    let header_has_mutation = matches!(
        &header_resp.response,
        Some(RespVariant::RequestHeaders(h))
            if h.response.as_ref()
                .and_then(|c| c.header_mutation.as_ref())
                .is_some_and(|m| m.set_headers.iter()
                    .filter_map(|hv| hv.header.as_ref())
                    .any(|hv| hv.key.eq_ignore_ascii_case("x-test")))
    );
    assert!(
        header_has_mutation,
        "HeadersResponse should carry X-Test header mutation, got: {header_resp:?}"
    );

    // First chunk — should NOT have header mutations (already sent at header time)
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk-1".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let msg1 = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out on chunk 1")
    .expect("stream error on chunk 1")
    .expect("stream closed on chunk 1");

    let first_has_mutation = matches!(
        &msg1.response,
        Some(RespVariant::RequestBody(b))
            if b.response.as_ref()
                .and_then(|c| c.header_mutation.as_ref())
                .is_some()
    );
    assert!(
        !first_has_mutation,
        "first chunk should NOT have header mutations, got: {msg1:?}"
    );

    // Second chunk — should NOT have header mutations
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"chunk-2".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let msg2 = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out on chunk 2")
    .expect("stream error on chunk 2")
    .expect("stream closed on chunk 2");

    let second_has_mutation = matches!(
        &msg2.response,
        Some(RespVariant::RequestBody(b))
            if b.response.as_ref()
                .and_then(|c| c.header_mutation.as_ref())
                .is_some()
    );
    assert!(
        !second_has_mutation,
        "second chunk should NOT have header mutations, got: {msg2:?}"
    );
}

#[tokio::test]
async fn response_headers_deferred_by_default() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let mut response_stream = client.process(stream).await.unwrap().into_inner();

    tx.send(make_request_headers("GET", "/", true)).await.unwrap();
    let _req_headers_resp = response_stream.message().await.unwrap();

    tx.send(make_response_headers(200, false)).await.unwrap();

    let resp_headers_resp = response_stream.message().await.unwrap().unwrap();

    if let Some(RespVariant::ResponseHeaders(h)) = &resp_headers_resp.response {
        let has_x_resp = h
            .response
            .as_ref()
            .and_then(|c| c.header_mutation.as_ref())
            .is_some_and(|m| {
                m.set_headers
                    .iter()
                    .filter_map(|h| h.header.as_ref())
                    .any(|hv| hv.key == "x-resp")
            });
        assert!(!has_x_resp, "X-Resp should be deferred until body phase");
    } else {
        panic!("expected ResponseHeaders, got: {resp_headers_resp:?}");
    }

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: b"test body".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let resp_body_resp = response_stream.message().await.unwrap().unwrap();

    if let Some(RespVariant::ResponseBody(b)) = &resp_body_resp.response {
        let has_x_resp = b
            .response
            .as_ref()
            .and_then(|c| c.header_mutation.as_ref())
            .is_some_and(|m| {
                m.set_headers
                    .iter()
                    .filter_map(|h| h.header.as_ref())
                    .any(|hv| hv.key == "x-resp" && hv.value == "true")
            });

        if !has_x_resp {
            let all_headers: Vec<_> = b
                .response
                .as_ref()
                .and_then(|c| c.header_mutation.as_ref())
                .map(|m| {
                    m.set_headers
                        .iter()
                        .filter_map(|h| h.header.as_ref())
                        .map(|hv| format!("{}={}", hv.key, hv.value))
                        .collect()
                })
                .unwrap_or_default();
            panic!("X-Resp should appear in body response. Found headers: {all_headers:?}");
        }
    } else {
        panic!("expected ResponseBody, got: {resp_body_resp:?}");
    }
}

#[tokio::test]
async fn none_request_mode_runs_header_filters_at_headers() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut response_stream = client.process(ReceiverStream::new(rx)).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/submit", false);
    headers.protocol_config = Some(protocol_config(0, 0));
    tx.send(headers).await.unwrap();

    let msg = next_full_duplex_msg(&mut response_stream).await;
    assert!(
        matches!(&msg.response, Some(RespVariant::RequestHeaders(_))),
        "NONE mode must answer headers with a HeadersResponse, got: {msg:?}"
    );
    let mutations = extract_all_set_headers(std::slice::from_ref(&msg));
    assert!(
        mutations.iter().any(|h| h.key == "x-test" && h.value == "extproc"),
        "header filters must run at header time when no body will ever arrive, got: {msg:?}"
    );
}

#[tokio::test]
async fn none_response_mode_sends_header_mutations_immediately() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut response_stream = client.process(ReceiverStream::new(rx)).await.unwrap().into_inner();

    let mut headers = make_request_headers("GET", "/", true);
    headers.protocol_config = Some(protocol_config(0, 0));
    tx.send(headers).await.unwrap();
    let _req_headers_resp = next_full_duplex_msg(&mut response_stream).await;

    tx.send(make_response_headers(200, false)).await.unwrap();
    let msg = next_full_duplex_msg(&mut response_stream).await;

    let Some(RespVariant::ResponseHeaders(h)) = &msg.response else {
        panic!("expected ResponseHeaders, got: {msg:?}");
    };
    let has_x_resp = h
        .response
        .as_ref()
        .and_then(|c| c.header_mutation.as_ref())
        .is_some_and(|m| {
            m.set_headers
                .iter()
                .filter_map(|h| h.header.as_ref())
                .any(|hv| hv.key == "x-resp" && hv.value == "true")
        });
    assert!(
        has_x_resp,
        "response header mutations must not be deferred to a body that never arrives, got: {msg:?}"
    );
}

#[tokio::test]
async fn none_mode_rejects_unexpected_body_message() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let mut response_stream = client.process(ReceiverStream::new(rx)).await.unwrap().into_inner();

    let mut headers = make_request_headers("POST", "/submit", false);
    headers.protocol_config = Some(protocol_config(0, 0));
    tx.send(headers).await.unwrap();
    let _headers_resp = next_full_duplex_msg(&mut response_stream).await;

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"unexpected".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .unwrap();

    let err = tokio::time::timeout(
        std::time::Duration::from_millis(TIMEOUT_MILLIS),
        response_stream.message(),
    )
    .await
    .expect("timed out waiting for rejection")
    .expect_err("a body message in NONE mode must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument, "should be InvalidArgument");
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

const UNCONDITIONAL_BRANCH_CONFIG: &str = r#"
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

const CONDITIONAL_TERMINAL_CONFIG: &str = r#"
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
    let registry = praxis_ai_filters::build_ai_registry();
    let pipeline = config::build_pipeline(&cfg, &registry).expect("build pipeline");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    let svc = PraxisExtProc::new(pipeline);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        Server::builder()
            .add_service(ExternalProcessorServer::new(svc))
            .serve_with_incoming_shutdown(tokio_stream::wrappers::TcpListenerStream::new(listener), async {
                drop(shutdown_rx.await);
            })
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

fn make_response_headers(status: u32, end_of_stream: bool) -> ProcessingRequest {
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

fn make_header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_owned(),
        raw_value: Vec::new(),
    }
}

/// Build the `protocol_config` Envoy attaches to the first stream message.
///
/// Body modes use the `BodySendMode` wire values: 0 `NONE`, 1 `STREAMED`,
/// 2 `BUFFERED`, 4 `FULL_DUPLEX_STREAMED`.
fn protocol_config(
    request_body_mode: i32,
    response_body_mode: i32,
) -> praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration {
    praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration {
        request_body_mode,
        response_body_mode,
        send_body_without_waiting_for_header_response: false,
    }
}

async fn collect_responses(inbound: &mut tonic::Streaming<ProcessingResponse>) -> Vec<ProcessingResponse> {
    let mut responses = Vec::new();
    let timeout = tokio::time::Duration::from_secs(2);

    while let Ok(Ok(Some(resp))) = tokio::time::timeout(timeout, inbound.message()).await {
        responses.push(resp);
    }

    responses
}

/// Collect streamed body chunks from a response stream.
///
/// Returns `(chunks, reassembled_body)` where chunks is the vector of
/// `StreamedBodyResponse` messages and `reassembled_body` is all chunk
/// bodies concatenated.
///
/// Panics if stream errors, times out, or exceeds `max_chunks` without EOS.
async fn collect_streamed_chunks<F>(
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
