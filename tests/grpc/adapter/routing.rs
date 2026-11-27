// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Envoy-routed translation, selected-body, and branch chains.

use praxis_proto::envoy::service::ext_proc::v3::body_mutation;

use super::support::*;

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

    tx.send(make_request_headers_with(
        "GET",
        "/",
        true,
        [make_header("x-danger", "true")],
    ))
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


/// Envoy-routed requests need both body translation and the late path mutation.
#[tokio::test]
async fn envoy_routed_anthropic_request_translates_body_and_path() {
    let (mut client, _shutdown) = start_server(ENVOY_ANTHROPIC_CONFIG).await;
    let input = br#"{"model":"internal-model","max_tokens":73,"system":"Be concise","messages":[{"role":"user","content":"hello"}]}"#;
    let responses = send_full_request(&mut client, "POST", "/v1/messages", input).await;
    let translated = responses
        .iter()
        .find_map(|response| {
            let RespVariant::RequestBody(body) = response.response.as_ref()? else {
                return None;
            };
            match body.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref()? {
                body_mutation::Mutation::Body(bytes) => Some(bytes),
                _ => None,
            }
        })
        .expect("Envoy-routed request must emit a translated body");
    let body: serde_json::Value = serde_json::from_slice(translated).unwrap();
    assert_eq!(body["model"], "internal-model");
    assert_eq!(body["max_completion_tokens"], 73);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "Be concise");
    assert_eq!(body["messages"][1]["role"], "user");
    assert!(body.get("system").is_none());
    assert!(
        extract_all_set_headers(&responses)
            .iter()
            .any(|header| { header.key == ":path" && header.value == "/v1/chat/completions" }),
        "translated body and rewritten path must be delivered together"
    );
}


/// Request-phase conditions must keep `OpenAI` bodies out of the Anthropic hook.
#[tokio::test]
async fn envoy_routed_openai_request_preserves_body() {
    let (mut client, _shutdown) = start_server(ENVOY_ANTHROPIC_CONFIG).await;
    let input = br#"{"model":"internal-model","messages":[{"role":"user","content":"hello"}]}"#;
    let responses = send_full_request(&mut client, "POST", "/v1/chat/completions", input).await;
    assert!(
        !responses
            .iter()
            .any(|response| matches!(response.response, Some(RespVariant::ImmediateResponse(_))))
    );
    assert!(
        !extract_all_set_headers(&responses)
            .iter()
            .any(|header| header.key == ":path")
    );
    for response in &responses {
        if let Some(RespVariant::RequestBody(body)) = &response.response
            && let Some(body_mutation::Mutation::Body(bytes)) = body
                .response
                .as_ref()
                .and_then(|common| common.body_mutation.as_ref())
                .and_then(|mutation| mutation.mutation.as_ref())
        {
            assert_eq!(bytes, input, "non-matching OpenAI body must remain unchanged");
        }
    }
}


/// Envoy-owned routing must not bypass the selected-body input limit.
#[tokio::test]
async fn envoy_routed_translation_rejects_oversized_input() {
    let (mut client, _shutdown) = start_server(ENVOY_ANTHROPIC_CONFIG).await;
    let mut input =
        br#"{"model":"internal-model","max_tokens":73,"messages":[{"role":"user","content":"hello"}]}"#.to_vec();
    input.resize(1_048_577, b' ');
    let responses = send_full_request(&mut client, "POST", "/v1/messages", &input).await;
    assert!(
        responses.iter().any(|response| matches!(response.response.as_ref(),
        Some(RespVariant::ImmediateResponse(reply)) if reply.status.as_ref().map(|status| status.code) == Some(413))),
        "input over the filter limit must be rejected before forwarding"
    );
}


/// A selected-body filter must reject STREAMED mode before Envoy forwards bytes.
#[tokio::test]
async fn streamed_selected_body_rejects_before_header_ack() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let mut registry = praxis_ai_filters::build_ai_registry();
    registry
        .register(
            "wire_selected_body",
            praxis_filter::http_builtin(WireSelectedBodyFilter::from_config),
        )
        .expect("register selected-body test filter");
    let (mut client, _shutdown) = start_server_with_registry(SELECTED_BODY_CONFIG, registry).await;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut inbound = client
        .process(ReceiverStream::new(rx))
        .await
        .expect("open stream")
        .into_inner();
    let mut headers = make_request_headers("POST", "/selected", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 1,
        response_body_mode: 1,
        send_body_without_waiting_for_header_response: true,
    });
    tx.send(headers).await.expect("send headers");
    let response = inbound.message().await.expect("gRPC response").expect("local reply");
    let Some(RespVariant::ImmediateResponse(immediate)) = response.response else {
        panic!("selected STREAMED mode must fail before body delivery");
    };
    assert_eq!(immediate.status.as_ref().map(|status| status.code), Some(500));
    assert!(immediate.body.contains("BUFFERED"));
}


/// BUFFERED mode carries selected-body bytes and late header mutations in its body response.
#[tokio::test]
async fn buffered_selected_body_translates_and_preserves_header_mutation() {
    use praxis_proto::envoy::service::ext_proc::v3::ProtocolConfiguration;

    let mut registry = praxis_ai_filters::build_ai_registry();
    registry
        .register(
            "wire_selected_body",
            praxis_filter::http_builtin(WireSelectedBodyFilter::from_config),
        )
        .expect("register selected-body test filter");
    let (mut client, _shutdown) = start_server_with_registry(SELECTED_BODY_CONFIG, registry).await;
    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let mut inbound = client
        .process(ReceiverStream::new(rx))
        .await
        .expect("open stream")
        .into_inner();
    let mut headers = make_request_headers("POST", "/selected", false);
    headers.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: 2,
        response_body_mode: 1,
        send_body_without_waiting_for_header_response: false,
    });
    tx.send(headers).await.expect("send headers");
    let header_reply = inbound.message().await.expect("gRPC response").expect("header ack");
    assert!(matches!(header_reply.response, Some(RespVariant::RequestHeaders(_))));

    tx.send(ProcessingRequest {
        request: Some(ReqVariant::RequestBody(HttpBody {
            body: b"original".to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    })
    .await
    .expect("send complete buffered body");
    let body_reply = inbound
        .message()
        .await
        .expect("gRPC response")
        .expect("translated body");
    let Some(RespVariant::RequestBody(body)) = body_reply.response else {
        panic!("BUFFERED translation must return a body response");
    };
    let common = body.response.expect("common response");
    let translated = common
        .body_mutation
        .as_ref()
        .and_then(|mutation| mutation.mutation.as_ref());
    assert!(
        matches!(translated, Some(body_mutation::Mutation::Body(bytes)) if bytes == b"rewritten-on-selected-upstream")
    );
    let early_header = common.header_mutation.as_ref().is_some_and(|mutation| {
        mutation.set_headers.iter().any(|option| {
            option
                .header
                .as_ref()
                .is_some_and(|header| header.key == "x-selected-early" && header.value == "yes")
        })
    });
    assert!(
        early_header,
        "buffered body response must carry the request header mutation"
    );
    let content_length = common.header_mutation.as_ref().is_some_and(|mutation| {
        mutation.set_headers.iter().any(|option| {
            option.header.as_ref().is_some_and(|header| {
                header.key == "content-length" && header.value == b"rewritten-on-selected-upstream".len().to_string()
            })
        })
    });
    assert!(content_length, "buffered rewrite must update content-length");
}
