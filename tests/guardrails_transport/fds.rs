//! Guardrail selection and termination contracts in full-duplex transport.

use praxis_proto::envoy::service::ext_proc::v3::{
    HttpTrailers, ProtocolConfiguration, body_mutation, processing_request::Request as RequestMessage,
    processing_response::Response as ResponseMessage,
};

use super::{
    limits::{collect_fds, request_chunk, start_fds},
    *,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fds_input_checks_complete_chunks_at_body_eos_and_trailers() {
    for trailers in [false, true] {
        for reply in [
            NemoReply::Pass,
            NemoReply::BlockInput,
            NemoReply::Unavailable,
            NemoReply::Malformed,
            NemoReply::Unknown,
            NemoReply::Timeout,
        ] {
            let harness = Harness::new(reply, false).await;
            let mut session = start_fds(&harness, (true, false)).await;
            let document = serde_json::to_vec(&json!({"messages":[{"role":"user","content":"split prompt"}]})).unwrap();
            let split = document.len() / 2;
            session.send(request_chunk(&document[..split], false)).await;
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(25), session.responses.message())
                    .await
                    .is_err(),
                "FDS request must remain private before completion"
            );
            assert!(harness.calls().is_empty(), "NeMo must receive the complete prompt");
            session.send(request_chunk(&document[split..], !trailers)).await;
            if trailers {
                session
                    .send(support::wrap(RequestMessage::RequestTrailers(HttpTrailers::default())))
                    .await;
            }
            if !matches!(reply, NemoReply::Pass | NemoReply::BlockInput) {
                let error = tokio::time::timeout(std::time::Duration::from_secs(2), session.responses.message())
                    .await
                    .unwrap()
                    .unwrap_err();
                assert_eq!(error.code(), tonic::Code::Internal, "{reply:?}, trailers={trailers}");
                assert_eq!(harness.calls().len(), 1);
                assert_eq!(harness.calls()[0]["messages"][0]["content"], "split prompt");
                continue;
            }
            let responses = collect_fds(&mut session, trailers).await;
            let expected = matches!(reply, NemoReply::BlockInput).then_some(403);
            assert_eq!(immediate_status(&responses), expected, "{reply:?}, trailers={trailers}");
            assert_eq!(harness.calls().len(), 1);
            assert_eq!(harness.calls()[0]["guardrails"]["rail_types"][0], "input");
            assert_eq!(harness.calls()[0]["messages"][0]["content"], "split prompt");
            if expected.is_some() {
                assert_eq!(
                    responses.len(),
                    1,
                    "a rejected input must emit no forwarding mutation or trailer ack"
                );
                assert!(session.next().await.is_none());
            } else {
                assert!(matches!(
                    responses.first().unwrap().response,
                    Some(ResponseMessage::RequestHeaders(_))
                ));
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fds_output_trailers_release_only_the_checked_replacement_before_ack() {
    for reply in [
        NemoReply::Pass,
        NemoReply::BlockOutput,
        NemoReply::Unavailable,
        NemoReply::Malformed,
        NemoReply::Unknown,
        NemoReply::Timeout,
    ] {
        let harness = Harness::new(reply, false).await;
        let mut session = start_fds(&harness, (false, true)).await;
        session.send(request("hello", false)).await;
        collect_fds(&mut session, false).await;
        session.send(response_headers("application/json")).await;
        let document =
            serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":"private answer"}}]}))
                .unwrap();
        session.send(response_body(&document, false)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), session.responses.message())
                .await
                .is_err(),
            "FDS output must remain private until trailers close the body"
        );
        assert!(harness.calls().is_empty());
        session
            .send(support::wrap(RequestMessage::ResponseTrailers(HttpTrailers::default())))
            .await;
        let responses = collect_fds(&mut session, true).await;
        assert!(matches!(
            responses.first().unwrap().response,
            Some(ResponseMessage::ResponseHeaders(_))
        ));
        assert!(matches!(
            responses.last().unwrap().response,
            Some(ResponseMessage::ResponseTrailers(_))
        ));
        let body = String::from_utf8(body_bytes(&responses)).unwrap();
        match reply {
            NemoReply::Pass => assert_eq!(body.as_bytes(), document),
            NemoReply::BlockOutput => {
                assert!(body.contains("content_blocked"));
                assert!(!body.contains("private answer"));
            },
            _ => {
                assert!(body.contains("evaluation_failed"));
                assert!(!body.contains("private answer"));
            },
        }
        assert_eq!(harness.calls().len(), 1);
        assert_eq!(harness.calls()[0]["messages"][0]["content"], "private answer");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconditional_and_host_selected_fds_output_checks_are_enforced() {
    for (host_condition, selected) in [(false, true), (true, false), (true, true)] {
        let harness = Harness::configured(NemoReply::BlockOutput, false, 2_097_152, |cfg| {
            cfg.filter_chains[0].filters.retain(|entry| {
                entry.filter_type != "ai_guardrails"
                    || entry.config["phase"]["response"] == serde_yaml::Value::Bool(true)
            });
            for entry in &mut cfg.filter_chains[0].filters {
                if entry.filter_type == "ai_guardrails" {
                    entry.conditions = if host_condition {
                        serde_yaml::from_str("- when:\n    headers: { host: model.test }\n").unwrap()
                    } else {
                        Vec::new()
                    };
                }
            }
        })
        .await;
        let mut session = harness.open().await;
        let protocol = ProtocolConfiguration {
            request_body_mode: BodySendMode::FullDuplexStreamed.into(),
            response_body_mode: BodySendMode::FullDuplexStreamed.into(),
            send_body_without_waiting_for_header_response: true,
        };
        let mut message = support::request_headers(protocol, (false, false), false);
        let Some(RequestMessage::RequestHeaders(headers)) = message.request.as_mut() else {
            unreachable!()
        };
        headers.headers.as_mut().unwrap().headers.push(support::header(
            ":authority",
            if selected { "model.test" } else { "other.test" },
        ));
        session.send(message).await;
        session.send(request("hello", false)).await;
        collect_fds(&mut session, false).await;
        session.send(response_headers("application/json")).await;
        let document =
            serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":"private answer"}}]}))
                .unwrap();
        session.send(response_body(&document, true)).await;
        let responses = collect_fds(&mut session, false).await;
        let body = String::from_utf8(body_bytes(&responses)).unwrap();
        assert_eq!(body.contains("content_blocked"), selected);
        assert_eq!(body.contains("private answer"), !selected);
        assert_eq!(harness.calls().len(), usize::from(selected));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_fds_modes_preserve_selection_and_header_body_order() {
    for (request_mode, response_mode) in [
        (BodySendMode::FullDuplexStreamed, BodySendMode::Buffered),
        (BodySendMode::FullDuplexStreamed, BodySendMode::Streamed),
        (BodySendMode::FullDuplexStreamed, BodySendMode::FullDuplexStreamed),
        (BodySendMode::Buffered, BodySendMode::FullDuplexStreamed),
    ] {
        for no_header_wait in [false, true] {
            for selected in [(false, false), (true, false), (false, true), (true, true)] {
                let harness = Harness::new(NemoReply::Pass, false).await;
                let mut session = harness.open().await;
                let protocol = ProtocolConfiguration {
                    request_body_mode: request_mode.into(),
                    response_body_mode: response_mode.into(),
                    send_body_without_waiting_for_header_response: no_header_wait,
                };
                session.send(support::request_headers(protocol, selected, false)).await;
                if request_mode == BodySendMode::Buffered {
                    let header = session.next().await.unwrap();
                    assert!(matches!(header.response, Some(ResponseMessage::RequestHeaders(_))));
                    assert!(header.mode_override.is_none());
                }
                let prompt = request("mode prompt", false);
                let Some(RequestMessage::RequestBody(original)) = prompt.request.as_ref() else {
                    unreachable!()
                };
                let original = original.body.clone();
                session.send(prompt).await;
                if request_mode == BodySendMode::FullDuplexStreamed {
                    let replies = collect_fds(&mut session, false).await;
                    assert_eq!(replies.len(), 2, "one headers reply before one complete request body");
                    assert!(matches!(replies[0].response, Some(ResponseMessage::RequestHeaders(_))));
                    let Some(ResponseMessage::RequestBody(body)) = replies[1].response.as_ref() else {
                        panic!("request headers must precede the body")
                    };
                    let Some(body_mutation::Mutation::StreamedResponse(body)) = body
                        .response
                        .as_ref()
                        .unwrap()
                        .body_mutation
                        .as_ref()
                        .unwrap()
                        .mutation
                        .as_ref()
                    else {
                        panic!("FDS wire body mutation required")
                    };
                    assert_eq!(body.body, original);
                    assert!(body.end_of_stream);
                } else {
                    let body = session.next().await.unwrap();
                    assert!(matches!(body.response, Some(ResponseMessage::RequestBody(_))));
                    assert!(body.mode_override.is_none());
                }
                assert_eq!(harness.calls().len(), usize::from(selected.0));
                session.send(response_headers("application/json")).await;
                if response_mode != BodySendMode::FullDuplexStreamed {
                    let header = session.next().await.unwrap();
                    assert!(matches!(header.response, Some(ResponseMessage::ResponseHeaders(_))));
                    assert!(header.mode_override.is_none());
                }
                let document =
                    serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":"mode answer"}}]}))
                        .unwrap();
                session.send(response_body(&document, true)).await;
                let responses = if response_mode == BodySendMode::FullDuplexStreamed {
                    collect_fds(&mut session, false).await
                } else {
                    vec![session.next().await.unwrap()]
                };
                if response_mode == BodySendMode::FullDuplexStreamed {
                    assert_eq!(
                        responses.len(),
                        2,
                        "one headers reply before one complete response body"
                    );
                    assert!(matches!(
                        responses[0].response,
                        Some(ResponseMessage::ResponseHeaders(_))
                    ));
                    assert!(matches!(responses[1].response, Some(ResponseMessage::ResponseBody(_))));
                }
                assert_eq!(
                    body_bytes(&responses),
                    document,
                    "{request_mode:?}/{response_mode:?}, {selected:?}, no_header_wait={no_header_wait}"
                );
                assert!(responses.iter().all(|reply| reply.mode_override.is_none()));
                let calls = harness.calls();
                assert_eq!(calls.len(), usize::from(selected.0) + usize::from(selected.1));
                let expected: Vec<_> = [
                    (selected.0, "input", "mode prompt"),
                    (selected.1, "output", "mode answer"),
                ]
                .into_iter()
                .filter(|(enabled, ..)| *enabled)
                .collect();
                for (call, (_, phase, content)) in calls.iter().zip(expected) {
                    assert_eq!(call["guardrails"]["rail_types"], json!([phase]));
                    assert_eq!(call["messages"][0]["content"], content);
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_fds_provider_calls_release_resources_and_next_stream_state_is_fresh() {
    for input in [false, true] {
        let harness = Harness::new(NemoReply::HoldFirst, false).await;
        let selected = (input, !input);
        let mut session = start_fds(&harness, selected).await;
        session.send(request("abandoned prompt", false)).await;
        if !input {
            collect_fds(&mut session, false).await;
            session.send(response_headers("application/json")).await;
            session
                .send(response_body(
                    br#"{"choices":[{"message":{"role":"assistant","content":"abandoned answer"}}]}"#,
                    true,
                ))
                .await;
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            harness.pending_call.started.notified(),
        )
        .await
        .expect("the first provider call must be pending before cancellation");
        assert_eq!(harness.calls().len(), 1);
        drop(session);
        // The output hook bridges through block_in_place; the configured 500ms
        // provider timeout bounds cancellation even while that hook is synchronous.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            harness.pending_call.finished.notified(),
        )
        .await
        .expect("cancellation or the provider deadline must release the pending HTTP request");
        let mut fresh = start_fds(&harness, selected).await;
        fresh.send(request("fresh prompt", false)).await;
        let request_replies = collect_fds(&mut fresh, false).await;
        assert_eq!(immediate_status(&request_replies), None);
        fresh.send(response_headers("application/json")).await;
        let body = br#"{"choices":[{"message":{"role":"assistant","content":"fresh answer"}}]}"#;
        fresh.send(response_body(body, true)).await;
        let responses = collect_fds(&mut fresh, false).await;
        assert_eq!(body_bytes(&responses), body);
        let calls = harness.calls();
        assert_eq!(calls.len(), 2, "one independent check per selected stream");
        assert_eq!(
            calls[0]["messages"][0]["content"],
            if input { "abandoned prompt" } else { "abandoned answer" }
        );
        assert_eq!(
            calls[1]["messages"][0]["content"],
            if input { "fresh prompt" } else { "fresh answer" }
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn headers_only_requests_and_responses_complete_without_body_callouts() {
    for request_mode in [
        BodySendMode::None,
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed,
    ] {
        for response_mode in [
            BodySendMode::None,
            BodySendMode::Buffered,
            BodySendMode::Streamed,
            BodySendMode::FullDuplexStreamed,
        ] {
            let harness = Harness::new(NemoReply::Pass, false).await;
            let mut session = harness.open().await;
            let protocol = ProtocolConfiguration {
                request_body_mode: request_mode.into(),
                response_body_mode: response_mode.into(),
                send_body_without_waiting_for_header_response: true,
            };
            let headers = session
                .exchange(support::request_headers(protocol, (true, true), true))
                .await;
            assert!(matches!(headers.response, Some(ResponseMessage::RequestHeaders(_))));
            let mut response = response_headers("application/json");
            let Some(RequestMessage::ResponseHeaders(headers)) = response.request.as_mut() else {
                unreachable!()
            };
            headers.end_of_stream = true;
            let reply = session.exchange(response).await;
            assert!(matches!(reply.response, Some(ResponseMessage::ResponseHeaders(_))));
            assert!(harness.calls().is_empty());
            drop(session.sender);
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(2), session.responses.message())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fds_empty_eos_and_redelivery_do_not_repeat_guardrail_checks() {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = start_fds(&harness, (true, true)).await;
    let mut document = request("complete prompt", false);
    let Some(RequestMessage::RequestBody(body)) = document.request.as_mut() else {
        unreachable!()
    };
    body.end_of_stream = false;
    session.send(document).await;
    session.send(request_chunk(b"", true)).await;
    collect_fds(&mut session, false).await;
    assert_eq!(harness.calls().len(), 1);
    session.send(request_chunk(b"ignored duplicate", true)).await;
    session.send(response_headers("application/json")).await;
    let document = br#"{"choices":[{"message":{"role":"assistant","content":"complete answer"}}]}"#;
    session.send(response_body(document, false)).await;
    session.send(response_body(b"", true)).await;
    let responses = collect_fds(&mut session, false).await;
    assert_eq!(body_bytes(&responses), document);
    session.send(response_body(b"ignored duplicate", true)).await;
    drop(session.sender);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), session.responses.message())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(harness.calls().len(), 2);
    assert_eq!(harness.calls()[0]["messages"][0]["content"], "complete prompt");
    assert_eq!(harness.calls()[1]["messages"][0]["content"], "complete answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interleaved_fds_response_preserves_request_header_filter_state() {
    for no_header_wait in [false, true] {
        let harness = Harness::new(NemoReply::Pass, false).await;
        let mut session = harness.open().await;
        let protocol = ProtocolConfiguration {
            request_body_mode: BodySendMode::FullDuplexStreamed.into(),
            response_body_mode: BodySendMode::FullDuplexStreamed.into(),
            send_body_without_waiting_for_header_response: no_header_wait,
        };
        let mut message = support::request_headers(protocol, (false, false), false);
        let Some(RequestMessage::RequestHeaders(headers)) = message.request.as_mut() else {
            unreachable!()
        };
        headers
            .headers
            .as_mut()
            .unwrap()
            .headers
            .push(support::header("x-request-id", "client-correlation-id"));
        session.send(message).await;
        session
            .send(request_chunk(br#"{"messages":[{"role":"user","content":"#, false))
            .await;
        session.send(response_headers("application/json")).await;
        session.send(request_chunk(br#""hello"}]}"#, true)).await;
        let request_replies = collect_fds(&mut session, false).await;
        let Some(ResponseMessage::RequestHeaders(headers)) = request_replies[0].response.as_ref() else {
            panic!("request header mutation before body")
        };
        let mutations = headers.response.as_ref().unwrap().header_mutation.as_ref().unwrap();
        assert!(
            mutations
                .remove_headers
                .iter()
                .any(|name| name == "x-aigateway-guardrail-output"),
            "deferred request header mutations survive early execution"
        );
        session.send(response_body(b"unselected output", true)).await;
        let replies = collect_fds(&mut session, false).await;
        let Some(ResponseMessage::ResponseHeaders(headers)) = replies[0].response.as_ref() else {
            panic!("response headers before body")
        };
        assert!(
            headers
                .response
                .as_ref()
                .unwrap()
                .header_mutation
                .as_ref()
                .is_some_and(|mutation| mutation
                    .set_headers
                    .iter()
                    .filter_map(|entry| entry.header.as_ref())
                    .any(|header| header.key == "x-request-id" && header.raw_value == b"client-correlation-id")),
            "response hook must see state from the request header hook"
        );
        assert_eq!(body_bytes(&replies), b"unselected output");
        assert!(harness.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_only_and_unselected_fds_output_streams_before_body_eos_or_trailers() {
    for input in [false, true] {
        for no_header_wait in [false, true] {
            for trailers in [false, true] {
                let harness = Harness::new(NemoReply::Pass, false).await;
                let mut session = harness.open().await;
                let protocol = ProtocolConfiguration {
                    request_body_mode: BodySendMode::FullDuplexStreamed.into(),
                    response_body_mode: BodySendMode::FullDuplexStreamed.into(),
                    send_body_without_waiting_for_header_response: no_header_wait,
                };
                session
                    .send(support::request_headers(protocol, (input, false), false))
                    .await;
                session.send(request("streaming prompt", true)).await;
                collect_fds(&mut session, false).await;
                session.send(response_headers("text/event-stream")).await;
                session.send(response_body(b"data: first\n\n", false)).await;
                let first = tokio::time::timeout(std::time::Duration::from_secs(2), session.responses.message())
                    .await
                    .expect("first FDS response must arrive before the next upstream chunk exists")
                    .unwrap()
                    .unwrap();
                assert!(matches!(first.response, Some(ResponseMessage::ResponseHeaders(_))));
                let body = session.next().await.unwrap();
                assert_eq!(body_bytes(&[body]), b"data: first\n\n");
                session.send(response_body(b"data: second\n\n", false)).await;
                let second = session.next().await.unwrap();
                assert!(
                    matches!(second.response, Some(ResponseMessage::ResponseBody(_))),
                    "headers must be emitted only once"
                );
                assert_eq!(body_bytes(&[second]), b"data: second\n\n");
                if trailers {
                    session
                        .send(support::wrap(RequestMessage::ResponseTrailers(HttpTrailers::default())))
                        .await;
                    assert!(
                        matches!(
                            session.next().await.unwrap().response,
                            Some(ResponseMessage::ResponseTrailers(_))
                        ),
                        "trailer closure must not duplicate headers or an empty body"
                    );
                } else {
                    session.send(response_body(b"data: [DONE]\n\n", true)).await;
                    let final_responses = collect_fds(&mut session, false).await;
                    assert_eq!(final_responses.len(), 1);
                    assert_eq!(body_bytes(&final_responses), b"data: [DONE]\n\n");
                }
                assert_eq!(harness.calls().len(), usize::from(input));
                if input {
                    assert_eq!(harness.calls()[0]["guardrails"]["rail_types"], json!(["input"]));
                }
            }
        }
    }
}
