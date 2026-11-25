// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Body-mode delivery: buffered, streamed, and full-duplex.

use std::time::Duration;

use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

use super::support::*;

#[tokio::test]
async fn none_body_mode_runs_pipeline_at_headers() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    // NONE(0) body mode: headers are the complete message even without EOS, so
    // the pipeline runs at the header phase and never waits for a body.
    let mut headers = make_request_headers("POST", "/none", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 0,
        response_body_mode: 0,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    let mutations = extract_all_set_headers(&responses);
    assert!(
        mutations.iter().any(|h| h.key == "x-test" && h.value == "extproc"),
        "NONE mode should run the header pipeline and emit the x-test mutation without a body message"
    );
}


#[tokio::test]
async fn buffered_request_body_closed_by_trailers_acks() {
    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    // BUFFERED request body announced by headers(EOS=false), but no body message
    // arrives; trailers close the body. The held pipeline work must run and the
    // trailers must be acknowledged rather than the stream stalling.
    let mut headers = make_request_headers("POST", "/trailers", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send trailers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestTrailers(_)))),
        "trailers closing a BUFFERED body should be acknowledged with a RequestTrailers response"
    );
    assert!(
        !responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_)))),
        "clean empty body should not be rejected"
    );
}


#[tokio::test]
async fn buffered_response_body_closed_by_trailers_acks() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    let mut headers = make_request_headers("GET", "/trailers", true);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");
    drop(inbound.message().await);

    // Response body announced by headers(EOS=false), closed by trailers alone.
    tx.send(make_response_headers(200, false))
        .await
        .expect("send response headers");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send response trailers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::ResponseTrailers(_)))),
        "trailers closing a BUFFERED response body should be acknowledged with a ResponseTrailers response"
    );
}


#[tokio::test]
async fn fds_request_body_closed_by_trailers_runs_pipeline() {
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    // FULL_DUPLEX_STREAMED request body announced by headers(EOS=false). A body
    // chunk arrives but is NOT marked end_of_stream; trailers close the body.
    // The held headers response and the accumulated body must be flushed and the
    // body pipeline must run, rather than only the trailers ack going out.
    let mut headers = make_request_headers("POST", "/api", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"hello".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .expect("send body");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send trailers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    assert!(
        has_request_headers_response(&responses),
        "trailers closing an FDS body must flush the deferred headers response, got: {responses:?}"
    );

    let streamed_chunks: Vec<_> = responses
        .iter()
        .filter_map(|r| match &r.response {
            Some(RespVariant::RequestBody(b)) => b
                .response
                .as_ref()
                .and_then(|c| c.body_mutation.as_ref())
                .and_then(|m| m.mutation.as_ref()),
            _ => None,
        })
        .filter_map(|m| match m {
            body_mutation::Mutation::StreamedResponse(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let streamed_body: Vec<u8> = streamed_chunks.iter().flat_map(|s| s.body.clone()).collect();
    assert_eq!(
        streamed_body, b"hello",
        "trailers closing an FDS body must flush the accumulated body as a streamed chunk"
    );
    assert!(
        streamed_chunks.iter().all(|s| !s.end_of_stream),
        "trailers close the stream, so the body chunk must carry end_of_stream=false"
    );

    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestTrailers(_)))),
        "the trailers must still be acknowledged, got: {responses:?}"
    );
    assert!(
        !responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_)))),
        "clean content should not be rejected"
    );
}


#[tokio::test]
async fn fds_request_body_closed_by_trailers_rejects_blocked_content() {
    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    let mut headers = make_request_headers("POST", "/api", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"DROP TABLE users".to_vec(),
            end_of_stream: false,
        })),
        ..Default::default()
    })
    .await
    .expect("send body");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send trailers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_)))),
        "guardrails must run on an FDS body closed by trailers and reject, got: {responses:?}"
    );
    assert!(
        !responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestTrailers(_)))),
        "no trailers ack should follow an immediate response, got: {responses:?}"
    );
}


#[tokio::test]
async fn fds_passthrough_request_trailers_flush_deferred_headers() {
    let (mut client, _shutdown) = start_server(HEADERS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    // FDS with a headers-only (no body access) pipeline defers the header
    // mutation, expecting to ride it out on the first body chunk. If trailers
    // arrive before any chunk, the parked mutation must still be flushed.
    let mut headers = make_request_headers("GET", "/", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send trailers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    let mutations = extract_all_set_headers(&responses);
    assert!(
        mutations.iter().any(|h| h.key == "x-test" && h.value == "extproc"),
        "trailers before any FDS chunk must still flush the deferred X-Test mutation, got: {responses:?}"
    );
    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestTrailers(_)))),
        "the trailers must still be acknowledged, got: {responses:?}"
    );
}


#[tokio::test]
async fn fds_request_trailers_without_body_chunk_sends_no_body() {
    let (mut client, _shutdown) = start_server(GUARDRAILS_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    // FDS with a body-reading pipeline, headers(EOS=false), then trailers close the
    // body with no body chunk ever arriving. The pipeline must still run (headers
    // response, ack), but no empty body message should be emitted.
    let mut headers = make_request_headers("POST", "/api", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 4,
        response_body_mode: 2,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .expect("send trailers");
    drop(tx);

    let responses = collect_responses(&mut inbound).await;

    assert!(
        has_request_headers_response(&responses),
        "a trailer-only FDS request must still flush the headers response, got: {responses:?}"
    );
    assert!(
        !responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestBody(_)))),
        "no body chunk arrived, so no body message should be emitted, got: {responses:?}"
    );
    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestTrailers(_)))),
        "the trailers must still be acknowledged, got: {responses:?}"
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

    tx.send(make_response_headers_with(
        200,
        false,
        [make_header("content-type", "text/plain")],
    ))
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


/// Under BUFFERED with a body filter, the response pipeline defers to body EOS
/// and runs `on_response` and `on_response_body` together on one shared context.
/// A filter that stashes a decision in `filter_metadata` during `on_response`
/// and reads it in `on_response_body` (the anthropic messages->chat-completions
/// transform) only works if both hooks see the same context. Regression test for
/// the body-EOS deferral: if `on_response` ran early on a throwaway header-time
/// context, the decision would be dropped and the upstream (OpenAI-shaped) body
/// would pass through untransformed.
#[tokio::test]
async fn buffered_response_transform_metadata_survives_body_phase() {
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

    let (mut client, _shutdown) = start_server(ANTHROPIC_TRANSFORM_CONFIG).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    // First message carries the BUFFERED protocol config for both directions.
    let mut headers = make_request_headers("POST", "/v1/messages", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,  // BUFFERED
        response_body_mode: 2, // BUFFERED
        ..Default::default()
    });
    tx.send(headers).await.expect("send request headers");
    drop(inbound.message().await); // request headers continue

    // Anthropic-shaped request body: on_request_body records the transform intent.
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: br#"{"model":"claude-opus-4-8","max_tokens":1024,"messages":[{"role":"user","content":"Hello"}]}"#
                .to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send request body");
    drop(inbound.message().await); // request body (transformed to chat-completions)

    // 200 response headers: on_response runs early under BUFFERED and stashes the
    // transform decision in filter_metadata.
    tx.send(make_response_headers(200, false))
        .await
        .expect("send response headers");
    drop(inbound.message().await); // response headers continue

    // Upstream returns an OpenAI chat.completion; on_response_body must convert it
    // back to Anthropic shape using the metadata set during on_response.
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: br#"{"id":"chatcmpl-1","model":"gpt-4","choices":[{"message":{"role":"assistant","content":"Hello!"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send response body");

    let body_resp = inbound.message().await.expect("receive").expect("response body");
    let emitted = match &body_resp.response {
        Some(RespVariant::ResponseBody(b)) => b
            .response
            .as_ref()
            .and_then(|c| c.body_mutation.as_ref())
            .and_then(|bm| bm.mutation.as_ref())
            .and_then(|m| match m {
                body_mutation::Mutation::Body(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .expect("response body should carry a replacement body mutation"),
        other => panic!("expected ResponseBody, got {other:?}"),
    };

    let parsed: serde_json::Value = serde_json::from_slice(&emitted).expect("emitted body is JSON");
    assert_eq!(
        parsed["type"], "message",
        "response must be transformed to Anthropic shape, got {parsed}"
    );
    assert_eq!(parsed["role"], "assistant", "transformed message role");
    assert_eq!(parsed["content"][0]["text"], "Hello!", "transformed content text");
    assert!(
        parsed.get("choices").is_none(),
        "OpenAI 'choices' must not survive the transform: {parsed}"
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

    tx.send(make_response_headers(200, false))
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
async fn empty_full_duplex_emits_streamed_eos() {
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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
    let hdr_msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
        .await
        .expect("timed out waiting for header response")
        .expect("header response stream error")
        .expect("stream closed before header response");
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::RequestHeaders(_))),
        "first response should be HeadersResponse, got: {hdr_msg:?}"
    );

    // Second response: StreamedBodyResponse with empty body + EOS
    let body_msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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
    let hdr_msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
        .await
        .expect("timed out waiting for header response")
        .expect("header response stream error")
        .expect("stream closed before header response");
    assert!(
        matches!(hdr_msg.response, Some(RespVariant::RequestHeaders(_))),
        "first response should be HeadersResponse, got: {hdr_msg:?}"
    );

    // Second response: StreamedBodyResponse with body data + EOS
    let body_msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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
    let hdr_msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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


#[tokio::test]
async fn full_duplex_response_body() {
    const MAX_CHUNKS: usize = 8;
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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

    let _header_resp = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    let hdr_msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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

    let _header_resp = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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

    let msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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

    let _header_resp = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
        let msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    headers.protocol_config = Some(ProtocolConfiguration {
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

    let msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
    use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

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

        let msg = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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

    let msg1 = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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

    let msg2 = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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

    let msg1 = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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

    let msg2 = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message())
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
