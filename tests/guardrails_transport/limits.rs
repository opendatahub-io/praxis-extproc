//! Effective body limits for full-duplex guardrail processing.

use praxis_proto::envoy::service::ext_proc::v3::{
    HttpBody, HttpTrailers, ProcessingResponse, ProtocolConfiguration, body_mutation,
    processing_request::Request as RequestMessage, processing_response::Response as ResponseMessage,
};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_fds_checks_reject_response_headers_before_request_completion() {
    for mode in [
        BodySendMode::None,
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed,
    ] {
        for no_header_wait in [false, true] {
            for selected in [(true, false), (false, true), (true, true)] {
                let harness = Harness::new(NemoReply::Pass, false).await;
                let mut session = harness.open().await;
                let protocol = ProtocolConfiguration {
                    request_body_mode: BodySendMode::FullDuplexStreamed.into(),
                    response_body_mode: mode.into(),
                    send_body_without_waiting_for_header_response: no_header_wait,
                };
                session.send(support::request_headers(protocol, selected, false)).await;
                session
                    .send(request_chunk(br#"{"messages":[{"role":"user","content":"#, false))
                    .await;
                session.send(response_headers("application/json")).await;
                session.send(request_chunk(br#""hello"}]}"#, true)).await;
                let response = session.next().await.unwrap();
                assert_eq!(
                    immediate_status(std::slice::from_ref(&response)),
                    Some(500),
                    "{mode:?}, no_header_wait={no_header_wait}"
                );
                assert!(support::immediate_body(&response).contains("complete request body before response"));
                assert!(
                    harness.calls().is_empty(),
                    "reject interleaving before partial screening or forwarding"
                );
                assert!(session.next().await.is_none());
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unselected_fds_requests_preserve_full_duplex_interleaving() {
    for mode in [
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed,
    ] {
        for no_header_wait in [false, true] {
            let harness = Harness::new(NemoReply::Pass, false).await;
            let mut session = harness.open().await;
            let protocol = ProtocolConfiguration {
                request_body_mode: BodySendMode::FullDuplexStreamed.into(),
                response_body_mode: mode.into(),
                send_body_without_waiting_for_header_response: no_header_wait,
            };
            session
                .send(support::request_headers(protocol, (false, false), false))
                .await;
            session
                .send(request_chunk(br#"{"messages":[{"role":"user","content":"#, false))
                .await;
            session.send(response_headers("application/json")).await;
            if mode != BodySendMode::FullDuplexStreamed {
                assert!(matches!(
                    session.next().await.unwrap().response,
                    Some(ResponseMessage::ResponseHeaders(_))
                ));
            }
            session.send(request_chunk(br#""hello"}]}"#, true)).await;
            let requests = collect_fds(&mut session, false).await;
            assert_eq!(immediate_status(&requests), None);
            session.send(response_body(b"unselected response", true)).await;
            let responses = if mode == BodySendMode::FullDuplexStreamed {
                collect_fds(&mut session, false).await
            } else {
                vec![session.next().await.unwrap()]
            };
            assert_eq!(body_bytes(&responses), b"unselected response");
            assert!(harness.calls().is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn buffered_output_enforces_the_runtime_guardrail_cap() {
    for overflow in [false, true] {
        let harness = Harness::new(NemoReply::Pass, false).await;
        let mut session = harness.session(BodySendMode::Buffered, false, true).await;
        session.exchange(request("hello", false)).await;
        session.exchange(response_headers("application/json")).await;
        let mut body =
            serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":"bounded output"}}]}))
                .unwrap();
        body.resize(1_048_576 + usize::from(overflow), b' ');
        session.send(response_body(&body, true)).await;
        if overflow {
            let error = tokio::time::timeout(std::time::Duration::from_secs(10), session.responses.message())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(error.code(), tonic::Code::ResourceExhausted);
            assert!(harness.calls().is_empty());
        } else {
            assert_eq!(body_bytes(&[session.next().await.unwrap()]), body);
            assert_eq!(harness.calls().len(), 1);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fds_input_enforces_the_effective_cap_before_provider_evaluation() {
    for (server_limit, limit) in [(256, 256), (2_097_152, 1_048_576)] {
        for trailers in [false, true] {
            for overflow in [false, true] {
                let harness = Harness::configured(NemoReply::Pass, false, server_limit, |cfg| {
                    cfg.filter_chains[0].filters.retain(|entry| {
                        entry
                            .config
                            .get("phase")
                            .and_then(|phase| phase.get("response"))
                            .and_then(serde_yaml::Value::as_bool)
                            != Some(true)
                    });
                })
                .await;
                let mut session = start_fds(&harness, (true, false)).await;
                let mut body =
                    serde_json::to_vec(&json!({"messages":[{"role":"user","content":"bounded input"}]})).unwrap();
                body.resize(limit, b' ');
                let split = body.len() / 2;
                session.send(request_chunk(&body[..split], false)).await;
                session
                    .send(request_chunk(&body[split..], !overflow && !trailers))
                    .await;
                if overflow {
                    session.send(request_chunk(b" ", !trailers)).await;
                    let rejected = session.next().await.unwrap();
                    assert_eq!(
                        immediate_status(&[rejected]),
                        Some(413),
                        "limit={limit}, trailers={trailers}"
                    );
                    assert!(harness.calls().is_empty(), "oversized input must never reach NeMo");
                    assert!(session.next().await.is_none());
                } else {
                    if trailers {
                        session
                            .send(support::wrap(RequestMessage::RequestTrailers(HttpTrailers::default())))
                            .await;
                    }
                    let responses = collect_fds(&mut session, trailers).await;
                    assert_eq!(immediate_status(&responses), None);
                    assert!(matches!(
                        responses.first().unwrap().response,
                        Some(ResponseMessage::RequestHeaders(_))
                    ));
                    assert_eq!(harness.calls().len(), 1);
                    assert_eq!(harness.calls()[0]["messages"][0]["content"], "bounded input");
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fds_output_enforces_the_effective_cap_before_provider_evaluation() {
    for (server_limit, limit) in [(256, 256), (2_097_152, 1_048_576)] {
        for trailers in [false, true] {
            for overflow in [false, true] {
                let harness = Harness::with_limit(NemoReply::Pass, false, server_limit).await;
                let mut session = start_fds(&harness, (false, true)).await;
                session.send(request("hello", false)).await;
                collect_fds(&mut session, false).await;
                session.send(response_headers("application/json")).await;
                let mut body = serde_json::to_vec(
                    &json!({"choices":[{"message":{"role":"assistant","content":"bounded output"}}]}),
                )
                .unwrap();
                body.resize(limit, b' ');
                let split = body.len() / 2;
                session.send(response_body(&body[..split], false)).await;
                session
                    .send(response_body(&body[split..], !overflow && !trailers))
                    .await;
                if overflow {
                    session.send(response_body(b" ", !trailers)).await;
                    let error = tokio::time::timeout(std::time::Duration::from_secs(10), session.responses.message())
                        .await
                        .unwrap()
                        .unwrap_err();
                    assert_eq!(
                        error.code(),
                        tonic::Code::ResourceExhausted,
                        "limit={limit}, trailers={trailers}"
                    );
                    assert!(harness.calls().is_empty(), "oversized output must never reach NeMo");
                } else {
                    if trailers {
                        session
                            .send(support::wrap(RequestMessage::ResponseTrailers(HttpTrailers::default())))
                            .await;
                    }
                    let responses = collect_fds(&mut session, trailers).await;
                    assert_eq!(body_bytes(&responses), body, "the exact limit must be accepted");
                    assert_eq!(harness.calls().len(), 1);
                    assert_eq!(harness.calls()[0]["messages"][0]["content"], "bounded output");
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

pub(super) async fn start_fds(harness: &Harness, selected: (bool, bool)) -> support::Session {
    let session = harness.open().await;
    let protocol = ProtocolConfiguration {
        request_body_mode: BodySendMode::FullDuplexStreamed.into(),
        response_body_mode: BodySendMode::FullDuplexStreamed.into(),
        send_body_without_waiting_for_header_response: true,
    };
    session.send(support::request_headers(protocol, selected, false)).await;
    session
}

pub(super) fn request_chunk(
    body: &[u8],
    end_of_stream: bool,
) -> praxis_proto::envoy::service::ext_proc::v3::ProcessingRequest {
    support::wrap(RequestMessage::RequestBody(HttpBody {
        body: body.to_vec(),
        end_of_stream,
    }))
}

pub(super) async fn collect_fds(session: &mut support::Session, trailers: bool) -> Vec<ProcessingResponse> {
    let mut responses = Vec::new();
    loop {
        let response = session.next().await.expect("complete FDS response");
        let done = match response.response.as_ref().unwrap() {
            ResponseMessage::ImmediateResponse(_) => true,
            ResponseMessage::RequestTrailers(_) | ResponseMessage::ResponseTrailers(_) => trailers,
            ResponseMessage::RequestBody(body) | ResponseMessage::ResponseBody(body) => {
                let mutation = body
                    .response
                    .as_ref()
                    .unwrap()
                    .body_mutation
                    .as_ref()
                    .unwrap()
                    .mutation
                    .as_ref()
                    .unwrap();
                let body_mutation::Mutation::StreamedResponse(body) = mutation else {
                    panic!("FDS must use streamed mutations")
                };
                if trailers {
                    assert!(!body.end_of_stream, "trailers terminate the wire body");
                }
                body.end_of_stream
            },
            _ => false,
        };
        assert!(response.mode_override.is_none(), "FDS must not change either body mode");
        responses.push(response);
        if done {
            return responses;
        }
    }
}
