// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! gRPC message-size framing and body-accumulation limits.

use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

use super::support::*;

#[tokio::test]
async fn streamed_body_filter_enforces_cumulative_limit_before_the_next_hook() {
    let yaml = format!(
        "server:\n  max_body_bytes: 4\n{}",
        GUARDRAILS_CONFIG.replace("allow_unbounded_body: true", "allow_unbounded_body: false")
    );
    let (mut client, _shutdown) = start_server(&yaml).await;
    for overflow in [false, true] {
        let (tx, mut inbound) = open_stream(&mut client).await;
        let mut headers = make_request_headers("POST", "/limited", false);
        headers.protocol_config = Some(ProtocolConfiguration {
            request_body_mode: BodySendMode::Streamed.into(),
            response_body_mode: BodySendMode::Buffered.into(),
            send_body_without_waiting_for_header_response: false,
        });
        tx.send(headers).await.unwrap();
        assert!(matches!(
            inbound.message().await.unwrap().unwrap().response,
            Some(RespVariant::RequestHeaders(_))
        ));
        for bytes in [b"ab".as_slice(), b"cd"] {
            tx.send(ProcessingRequest {
                request: Some(ReqVariant::RequestBody(HttpBody {
                    body: bytes.to_vec(),
                    end_of_stream: false,
                })),
                ..Default::default()
            })
            .await
            .unwrap();
            assert!(
                matches!(
                    inbound.message().await.unwrap().unwrap().response,
                    Some(RespVariant::RequestBody(_))
                ),
                "body filters must accept chunks up to the cumulative boundary"
            );
        }
        tx.send(ProcessingRequest {
            request: Some(ReqVariant::RequestBody(HttpBody {
                body: if overflow { b"DROP TABLE".to_vec() } else { Vec::new() },
                end_of_stream: true,
            })),
            ..Default::default()
        })
        .await
        .unwrap();
        drop(tx);
        let replies = collect_responses(&mut inbound).await;
        assert_eq!(replies.len(), 1);
        if overflow {
            assert!(
                matches!(&replies[0].response, Some(RespVariant::ImmediateResponse(reply))
                if reply.status.as_ref().unwrap().code == 413),
                "the cumulative cap must reject before the guardrail can return its own 403"
            );
        } else {
            assert!(
                matches!(replies[0].response, Some(RespVariant::RequestBody(_))),
                "an empty EOS at the exact limit remains valid"
            );
        }
    }
}


/// Request bodies over `server.max_body_bytes` must be `ImmediateResponse` 413,
/// not a gRPC `RESOURCE_EXHAUSTED` that Envoy turns into an empty HTTP 500.
#[tokio::test]
async fn request_body_over_max_returns_immediate_413() {
    let (mut client, _shutdown) = start_server(SMALL_BODY_LIMIT_CONFIG).await;
    let body = vec![b'x'; 2048];
    let responses = send_full_request(&mut client, "POST", "/too-large", &body).await;
    assert!(
        responses.iter().any(|response| matches!(
            response.response.as_ref(),
            Some(RespVariant::ImmediateResponse(reply))
                if reply.status.as_ref().map(|status| status.code) == Some(413)
        )),
        "over-limit request body must be ImmediateResponse 413, got: {responses:?}"
    );
}


/// After `ImmediateResponse` 413, the server must end its response stream even
/// when the client keeps the request sender open (resource-lifetime guard).
#[tokio::test]
async fn immediate_413_ends_response_stream_while_client_input_stays_open() {
    let (mut client, _shutdown) = start_server(SMALL_BODY_LIMIT_CONFIG).await;
    let (tx, mut inbound) = open_stream(&mut client).await;

    tx.send(make_request_headers("POST", "/too-large-open", false))
        .await
        .expect("send headers");
    let header_resp = inbound.message().await.expect("recv").expect("headers response");
    assert!(
        matches!(&header_resp.response, Some(RespVariant::RequestHeaders(_))),
        "headers phase must complete before the over-limit body, got: {header_resp:?}"
    );

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: vec![b'x'; 2048],
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send over-limit body");

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
    let mut saw_413 = false;
    let mut saw_eof = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(tokio::time::Duration::from_millis(200), inbound.message()).await {
            Ok(Ok(Some(resp))) => {
                if matches!(
                    resp.response.as_ref(),
                    Some(RespVariant::ImmediateResponse(reply))
                        if reply.status.as_ref().map(|status| status.code) == Some(413)
                ) {
                    saw_413 = true;
                }
            },
            Ok(Ok(None)) => {
                saw_eof = true;
                break;
            },
            Ok(Err(status)) => panic!("unexpected stream error before EOF: {status}"),
            Err(_) => {},
        }
    }

    assert!(
        saw_413,
        "must receive ImmediateResponse 413 while client input stays open"
    );
    assert!(
        saw_eof,
        "server must EOF the response stream after ImmediateResponse without waiting for client input EOF"
    );
    // Keep the request sender alive until after the assertions so this case
    // cannot pass merely because the client closed the input stream.
    drop(tx);
}


/// Framing headroom: a BUFFERED body exactly at `max_body_bytes` must reach the
/// handler (not fail at tonic decode), and one byte over must be `ImmediateResponse` 413.
#[tokio::test]
async fn buffered_body_at_and_over_max_body_bytes() {
    use praxis_extproc::config::DEFAULT_MAX_BODY_BYTES;

    let (mut client, _shutdown) = start_server(DEFAULT_BODY_LIMIT_CONFIG).await;

    let at_limit = vec![b'x'; DEFAULT_MAX_BODY_BYTES];
    let accepted = send_full_request(&mut client, "POST", "/at-limit", &at_limit).await;
    assert!(
        accepted
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestBody(_)))),
        "exact max_body_bytes BUFFERED body must reach the handler, got: {accepted:?}"
    );
    assert!(
        !accepted
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_)))),
        "exact max_body_bytes body must not be rejected"
    );

    let over_limit = vec![b'x'; DEFAULT_MAX_BODY_BYTES + 1];
    let rejected = send_full_request(&mut client, "POST", "/over-limit", &over_limit).await;
    assert!(
        rejected.iter().any(|response| matches!(
            response.response.as_ref(),
            Some(RespVariant::ImmediateResponse(reply))
                if reply.status.as_ref().map(|status| status.code) == Some(413)
        )),
        "over max_body_bytes must be ImmediateResponse 413, got: {rejected:?}"
    );
}


/// RHOAIENG-102139: the shipped binary's default recv ceiling must accept a body
/// just over tonic's compiled 4 MiB `MaxRecv`. Spawns `praxis-extproc` so the
/// assertion exercises `serve_grpc` wiring, not the in-process harness.
#[tokio::test(flavor = "multi_thread")]
async fn grpc_max_recv_binary_accepts_body_above_four_mib() {
    let payload = vec![b'x'; GRPC_COMPILED_MAX_RECV_DEFAULT + 64 * 1024];
    let (grpc, health, metrics) = (free_port(), free_port(), free_port());
    let config_path = write_grpc_max_recv_config(grpc, health, metrics);

    let mut child = BinaryChild(
        Command::new(env!("CARGO_BIN_EXE_praxis-extproc"))
            .arg("-c")
            .arg(&config_path)
            .env_remove(GRPC_MAX_RECV_MSG_SIZE_ENV)
            .spawn()
            .expect("spawn praxis-extproc"),
    );

    let mut client = connect_extproc_client(grpc).await;
    let responses = send_full_request(&mut client, "POST", "/large-over-4mib", &payload).await;

    drop(client);
    stop_binary(&mut child).await;
    drop(std::fs::remove_file(&config_path));

    assert!(
        responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::RequestBody(_)))),
        "default max recv must process the request body, got: {responses:?}"
    );
    assert!(
        !responses
            .iter()
            .any(|r| matches!(&r.response, Some(RespVariant::ImmediateResponse(_)))),
        "body just over 4 MiB must not be rejected as immediate response"
    );
}


/// RHOAIENG-102139 control: tonic's compiled 4 MiB default (no
/// `.max_decoding_message_size`) rejects a body just over that limit.
#[tokio::test]
async fn grpc_compiled_max_recv_rejects_body_above_four_mib() {
    let payload = vec![b'x'; GRPC_COMPILED_MAX_RECV_DEFAULT + 64 * 1024];
    let (mut client, _shutdown) = start_server_with_tonic_compiled_max_recv(HEADERS_ONLY_CONFIG).await;
    let err = send_full_request_result(&mut client, "POST", "/large-over-4mib", &payload)
        .await
        .expect_err("compiled 4 MiB limit must reject the oversized gRPC message");
    let msg = err.to_string();
    assert!(
        msg.contains("length too large") || msg.contains("RESOURCE_EXHAUSTED") || msg.contains("message size"),
        "expected gRPC size rejection, got: {msg}"
    );
}
