// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Independent gRPC receive and response encoding limits.

use prost::Message as _;

use super::support::*;

#[tokio::test]
async fn grpc_receive_boundary_counts_serialized_protobuf_not_only_body_bytes() {
    let message = request_body(&vec![b'x'; 1024], true);
    let encoded = message.encoded_len();
    assert!(encoded > 1024, "protobuf envelope contributes to the receive limit");
    for max_recv in [encoded - 1, encoded, encoded + 1] {
        let (mut client, _shutdown) = start_server_with_registry_and_max_recv(
            DEFAULT_BODY_LIMIT_CONFIG,
            FilterRegistry::with_builtins(),
            max_recv,
        )
        .await;
        let (tx, mut stream) = open_stream(&mut client).await;
        tx.send(make_request_headers("POST", "/framing", false)).await.unwrap();
        next(&mut stream).await;
        tx.send(message.clone()).await.unwrap();
        drop(tx);
        if max_recv < encoded {
            let error = stream.message().await.unwrap_err();
            assert_eq!(error.code(), tonic::Code::Internal, "{error}");
            assert!(error.message().contains("decoded message length too large"), "{error}");
        } else {
            let replies = finish(&mut stream).await;
            assert_eq!(immediate_status(&replies), None);
            assert!(
                replies
                    .iter()
                    .any(|reply| matches!(reply.response, Some(RespVariant::RequestBody(_))))
            );
        }
    }
}

#[tokio::test]
async fn server_encode_limit_rejects_an_oversized_outbound_response() {
    let yaml = format!(
        "filter_chains:\n  - name: main\n    filters:\n      - filter: static_response\n        status: 200\n        body: '{}'\n",
        "x".repeat(2048)
    );
    let cfg: config::ExtProcConfig = serde_yaml::from_str(&yaml).unwrap();
    let pipeline = config::build_pipeline(&cfg, &praxis_ai_filters::build_ai_registry()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = encoding_limited_server(listener, pipeline).await;
    let (tx, mut stream) = open_stream(&mut client).await;
    tx.send(make_request_headers("GET", "/oversized-response", true))
        .await
        .unwrap();
    drop(tx);
    let error = tokio::time::timeout(DEADLINE, stream.message())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::OutOfRange);
    assert!(error.message().contains("encoded message length too large"), "{error}");
}
