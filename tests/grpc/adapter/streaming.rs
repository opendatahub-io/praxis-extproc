// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Byte integrity and interleaving across streamed response modes.
//!
//! EOS assertions follow `StreamedBodyResponse.end_of_stream` in Envoy's
//! `envoy/service/ext_proc/v3/external_processor.proto` contract.

use super::support::*;

#[tokio::test]
async fn sse_fragments_reassemble_with_exactly_one_terminal_done_marker() {
    let sse = "data: {\"text\":\"café ☕\"}\n\ndata: [DONE]\n\n".as_bytes();
    for mode in [BodySendMode::Streamed, BodySendMode::FullDuplexStreamed] {
        let (mut client, _shutdown) = start_server_with_registry(PROBE_CONFIG, probe_registry()).await;
        let (tx, mut stream) = open_stream(&mut client).await;
        let mut headers = make_request_headers("GET", "/sse", true);
        headers.protocol_config = Some(protocol(BodySendMode::None, mode));
        tx.send(headers).await.unwrap();
        assert!(matches!(
            next(&mut stream).await.response,
            Some(RespVariant::RequestHeaders(_))
        ));
        tx.send(response_headers("text/event-stream")).await.unwrap();
        if mode == BodySendMode::Streamed {
            assert!(matches!(
                next(&mut stream).await.response,
                Some(RespVariant::ResponseHeaders(_))
            ));
        }
        let chunks: Vec<_> = sse.chunks(3).collect();
        let mut replies = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            tx.send(make_response_body(chunk, index + 1 == chunks.len()))
                .await
                .unwrap();
            let mut reply = next(&mut stream).await;
            if matches!(reply.response, Some(RespVariant::ResponseHeaders(_))) {
                replies.push(reply);
                reply = next(&mut stream).await;
            }
            assert!(matches!(reply.response, Some(RespVariant::ResponseBody(_))));
            replies.push(reply);
        }
        drop(tx);
        assert!(finish(&mut stream).await.is_empty(), "no duplicate terminal output");
        let bytes = body_bytes(&replies);
        assert_eq!(bytes, sse, "{mode:?}: every fragment must survive");
        assert_eq!(String::from_utf8(bytes).unwrap().matches("[DONE]").count(), 1);
        if mode == BodySendMode::FullDuplexStreamed {
            let endings: Vec<_> = replies.iter().filter_map(response_eos).collect();
            assert_eq!(endings.iter().filter(|&&eos| eos).count(), 1);
            assert_eq!(endings.last(), Some(&true));
        }
    }
}


#[tokio::test]
async fn full_duplex_request_and_response_body_chunks_interleave_before_either_eos() {
    let (mut client, _shutdown) = start_server(HEADERS_ONLY_CONFIG).await;
    let (tx, mut stream) = open_stream(&mut client).await;
    let mut headers = make_request_headers("POST", "/duplex", false);
    headers.protocol_config = Some(protocol(
        BodySendMode::FullDuplexStreamed,
        BodySendMode::FullDuplexStreamed,
    ));
    tx.send(headers).await.unwrap();
    tx.send(request_body(b"request-1", false)).await.unwrap();
    assert!(matches!(
        next(&mut stream).await.response,
        Some(RespVariant::RequestHeaders(_))
    ));
    let first_request = next(&mut stream).await;
    assert_eq!(request_bytes(std::slice::from_ref(&first_request)), b"request-1");
    assert_eq!(streamed_eos(&first_request), Some(false));
    tx.send(make_response_headers(200, false)).await.unwrap();
    tx.send(make_response_body(b"response-1", false)).await.unwrap();
    assert!(matches!(
        next(&mut stream).await.response,
        Some(RespVariant::ResponseHeaders(_))
    ));
    let first_response = next(&mut stream).await;
    assert_eq!(body_bytes(std::slice::from_ref(&first_response)), b"response-1");
    assert_eq!(response_eos(&first_response), Some(false));
    tx.send(request_body(b"request-2", true)).await.unwrap();
    let last_request = next(&mut stream).await;
    assert_eq!(request_bytes(std::slice::from_ref(&last_request)), b"request-2");
    assert_eq!(streamed_eos(&last_request), Some(true));
    tx.send(make_response_body(b"response-2", true)).await.unwrap();
    let last_response = next(&mut stream).await;
    assert_eq!(body_bytes(std::slice::from_ref(&last_response)), b"response-2");
    assert_eq!(response_eos(&last_response), Some(true));
    drop(tx);
    assert!(finish(&mut stream).await.is_empty());
}
