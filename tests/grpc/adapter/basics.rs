// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Baseline request/response processing and header handling.

use super::support::*;

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
