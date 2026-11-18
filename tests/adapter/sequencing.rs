// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Protocol sequencing: ordering, duplicate end-of-stream, local reply, drain.

use super::support::*;

#[tokio::test]
async fn empty_processing_messages_do_not_poison_the_next_request() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, mut inbound) = open_stream(&mut client).await;
    tx.send(ProcessingRequest::default()).await.unwrap();
    tx.send(make_request_headers("GET", "/after-empty", true))
        .await
        .unwrap();
    tx.send(ProcessingRequest::default()).await.unwrap();
    drop(tx);
    let responses = collect_responses(&mut inbound).await;
    assert_eq!(
        responses.len(),
        1,
        "empty messages must not produce responses or end the stream"
    );
    assert!(matches!(responses[0].response, Some(RespVariant::RequestHeaders(_))));
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
async fn local_reply_buffered_passes_through_without_filters() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;
    let (tx, mut stream) = open_stream(&mut client).await;

    tx.send(with_response_body_mode(
        make_response_headers(401, false),
        BodySendMode::Buffered,
    ))
    .await
    .unwrap();
    let headers = next_full_duplex_msg(&mut stream).await;
    tx.send(make_response_body(br#"{"error":"unauthorized"}"#, true))
        .await
        .unwrap();
    let body = next_full_duplex_msg(&mut stream).await;

    assert!(
        matches!(&headers.response, Some(RespVariant::ResponseHeaders(h)) if h.response.as_ref().is_some_and(|c| c.header_mutation.is_none())),
        "local reply headers must pass through without the response filter's mutation: {headers:?}"
    );
    assert!(
        matches!(&body.response, Some(RespVariant::ResponseBody(b))
            if b.response.as_ref().is_some_and(|c| c.body_mutation.is_none() && c.header_mutation.is_none())),
        "local reply body must pass through unchanged, without deferred header mutations: {body:?}"
    );
}


#[tokio::test]
async fn local_reply_without_body_passes_through() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;
    let (tx, mut stream) = open_stream(&mut client).await;

    tx.send(make_response_headers(429, true)).await.unwrap();
    let headers = next_full_duplex_msg(&mut stream).await;

    assert!(
        matches!(&headers.response, Some(RespVariant::ResponseHeaders(h)) if h.response.as_ref().is_some_and(|c| c.header_mutation.is_none())),
        "headers-only local reply must pass through without the response filter's mutation: {headers:?}"
    );
}


#[tokio::test]
async fn local_reply_streamed_body_and_trailers_pass_through() {
    let (mut client, _shutdown) = start_server(RESPONSE_HEADER_CONFIG).await;
    let (tx, mut stream) = open_stream(&mut client).await;

    tx.send(with_response_body_mode(
        make_response_headers(403, false),
        BodySendMode::Streamed,
    ))
    .await
    .unwrap();
    let mut responses = vec![next_full_duplex_msg(&mut stream).await];
    for chunk in [b"forbidden-1".as_slice(), b"forbidden-2"] {
        tx.send(make_response_body(chunk, false)).await.unwrap();
        responses.push(next_full_duplex_msg(&mut stream).await);
    }
    tx.send(ProcessingRequest {
        request: Some(ReqVariant::ResponseTrailers(HttpTrailers { trailers: None })),
        ..Default::default()
    })
    .await
    .unwrap();
    responses.push(next_full_duplex_msg(&mut stream).await);

    assert!(
        matches!(
            responses.iter().map(|r| r.response.as_ref()).collect::<Vec<_>>().as_slice(),
            [
                Some(RespVariant::ResponseHeaders(h)),
                Some(RespVariant::ResponseBody(b1)),
                Some(RespVariant::ResponseBody(b2)),
                Some(RespVariant::ResponseTrailers(_)),
            ] if h.response.as_ref().is_some_and(|c| c.header_mutation.is_none())
                && [b1, b2].iter().all(|b| b.response.as_ref().is_some_and(|c| c.body_mutation.is_none()))
        ),
        "every local reply phase must continue unchanged: {responses:?}"
    );
}


#[tokio::test]
async fn request_headers_after_local_reply_rejected() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, mut stream) = open_stream(&mut client).await;

    tx.send(make_response_headers(401, true)).await.unwrap();
    let _headers = next_full_duplex_msg(&mut stream).await;
    tx.send(make_request_headers("GET", "/", true)).await.unwrap();

    let err = tokio::time::timeout(std::time::Duration::from_millis(TIMEOUT_MILLIS), stream.message())
        .await
        .expect("timed out waiting for error")
        .expect_err("request headers after a local reply should be rejected");

    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        err.message().contains("after a local reply"),
        "should name the local reply: {}",
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
async fn drain_deadline_force_cancels_in_flight_stream() {
    let yaml = r#"
filter_chains:
  - name: main
    filters:
      - filter: request_id
"#;
    let cfg: config::ExtProcConfig = serde_yaml::from_str(yaml).expect("parse config");
    let registry = praxis_ai_filters::build_ai_registry();
    let pipeline = config::build_pipeline(&cfg, &registry).expect("build pipeline");

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");

    // Wire a force-shutdown latch we can trigger directly, standing in for the
    // drain-deadline timer.
    let (force_tx, force_rx) = tokio::sync::watch::channel(false);
    let svc = PraxisExtProc::new(pipeline).with_force_shutdown(force_rx);
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
    let mut client = ExtProcClient::new(channel);

    // Keep the stream in-flight: send headers without end_of_stream and hold the
    // sender open so the handler parks awaiting further messages.
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = ReceiverStream::new(rx);
    let response = client.process(stream).await.expect("process call failed");
    let mut inbound = response.into_inner();

    tx.send(make_request_headers("GET", "/", false))
        .await
        .expect("send headers");

    // Consume the header response so the next item is the forced cancellation.
    let first = inbound.message().await.expect("first message ok");
    assert!(first.is_some(), "should receive a header response");

    // Trigger the drain deadline.
    force_tx.send(true).expect("flip latch");

    let err = loop {
        match inbound.message().await {
            Ok(Some(_)) => {},
            Ok(None) => panic!("stream ended cleanly; expected forced cancellation"),
            Err(status) => break status,
        }
    };
    assert_eq!(
        err.code(),
        tonic::Code::Unavailable,
        "in-flight stream should be force-cancelled with Unavailable, got {err:?}"
    );

    drop(shutdown_tx);
}
