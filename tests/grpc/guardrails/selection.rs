//! Guardrail selection headers must have one interpretation across phases.

use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, ProtocolConfiguration, processing_mode::HeaderSendMode,
        processing_request::Request as RequestMessage, processing_response::Response as ResponseMessage,
    },
};

use super::*;

#[rstest::rstest]
#[case::response_condition("[]", "[{when: {status: [200]}}]")]
#[case::multiple_conditions(
    "[{when: {headers: {x-aigateway-guardrail-output: 'true'}}}, {when: {path: /selected}}]",
    "[]"
)]
#[case::combined_condition("[{when: {headers: {x-aigateway-guardrail-output: 'true'}, path: /selected}}]", "[]")]
#[case::unless_condition("[{unless: {headers: {x-aigateway-guardrail-output: 'false'}}}]", "[]")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_selector_shapes_require_complete_requests_without_streaming_overrides(
    #[case] conditions: &str,
    #[case] response_conditions: &str,
    #[values(BodySendMode::None, BodySendMode::Streamed, BodySendMode::Buffered)] request_mode: BodySendMode,
) {
    let harness = Harness::configured(NemoReply::Pass, false, 1_048_576, |cfg| {
        let output = cfg.filter_chains[0]
            .filters
            .iter_mut()
            .find(|entry| {
                entry
                    .config
                    .get("phase")
                    .and_then(|phase| phase.get("response"))
                    .and_then(serde_yaml::Value::as_bool)
                    == Some(true)
            })
            .unwrap();
        output.conditions = serde_yaml::from_str(conditions).unwrap();
        output.response_conditions = serde_yaml::from_str(response_conditions).unwrap();
        let selector = praxis_extproc::response_mode::ResponseModeSelector::from_config(&cfg.filter_chains);
        assert_eq!(
            selector.invalid_reason(),
            Some("unsupported guardrail selection condition")
        );
    })
    .await;
    let mut session = harness.open().await;
    let reply = session
        .exchange(support::request_headers(
            ProtocolConfiguration {
                request_body_mode: request_mode.into(),
                response_body_mode: BodySendMode::Buffered.into(),
                send_body_without_waiting_for_header_response: false,
            },
            (false, false),
            false,
        ))
        .await;
    assert!(
        reply.mode_override.is_none(),
        "unknown selection must not enable response streaming"
    );
    if request_mode == BodySendMode::Buffered {
        assert!(matches!(reply.response, Some(ResponseMessage::RequestHeaders(_))));
    } else {
        assert!(support::immediate_body(&reply).contains("guardrail selection cannot be determined"));
        assert!(support::immediate_body(&reply).contains("BUFFERED or FULL_DUPLEX_STREAMED"));
        assert_eq!(immediate_status(&[reply]), Some(500));
        assert!(session.next().await.is_none());
    }
    assert!(
        harness.calls().is_empty(),
        "admission must not call the guardrail provider"
    );
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_selector_preserves_request_mode_and_header_wait_contract(
    #[values(
        BodySendMode::None,
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed
    )]
    response_mode: BodySendMode,
    #[values((false, false), (true, false), (false, true), (true, true))] selected: (bool, bool),
    #[values(false, true)] no_header_wait: bool,
) {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.open().await;
    let protocol = ProtocolConfiguration {
        request_body_mode: BodySendMode::Buffered.into(),
        response_body_mode: response_mode.into(),
        send_body_without_waiting_for_header_response: no_header_wait,
    };
    let reply = session
        .exchange(support::request_headers(protocol, selected, false))
        .await;
    let should_override =
        !selected.1 && !no_header_wait && matches!(response_mode, BodySendMode::Buffered | BodySendMode::Streamed);
    assert_eq!(
        reply.mode_override.is_some(),
        should_override,
        "{response_mode:?}, {selected:?}, no_header_wait={no_header_wait}"
    );
    if let Some(mode) = reply.mode_override {
        assert_eq!(mode.request_body_mode, i32::from(BodySendMode::Buffered));
        assert_eq!(mode.response_body_mode, i32::from(BodySendMode::Streamed));
        assert_eq!(mode.response_header_mode, i32::from(HeaderSendMode::Send));
    }
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_guardrails_reject_incomplete_request_modes_before_forwarding(
    #[values(BodySendMode::None, BodySendMode::Streamed)] request_mode: BodySendMode,
    #[values(
        BodySendMode::None,
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed
    )]
    response_mode: BodySendMode,
    #[values((true, false), (false, true), (true, true))] selected: (bool, bool),
    #[values(false, true)] no_header_wait: bool,
) {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.open().await;
    let protocol = ProtocolConfiguration {
        request_body_mode: request_mode.into(),
        response_body_mode: response_mode.into(),
        send_body_without_waiting_for_header_response: no_header_wait,
    };
    let reply = session
        .exchange(support::request_headers(protocol, selected, false))
        .await;
    assert_eq!(
        immediate_status(std::slice::from_ref(&reply)),
        Some(500),
        "{request_mode:?}/{response_mode:?}, {selected:?}"
    );
    assert!(support::immediate_body(&reply).contains("require BUFFERED or FULL_DUPLEX_STREAMED"));
    assert!(
        session.next().await.is_none(),
        "rejection must close the stream before body dispatch"
    );
    assert!(
        harness.calls().is_empty(),
        "unsupported transport must fail before provider calls"
    );
}

#[rstest::rstest]
#[case::unselected_body((false, false), false)]
#[case::bodyless_output((false, true), true)]
#[case::bodyless_both((true, true), true)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_modes_allow_unselected_and_truly_bodyless_requests(
    #[values(BodySendMode::None, BodySendMode::Streamed)] request_mode: BodySendMode,
    #[case] selected: (bool, bool),
    #[case] eos: bool,
) {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.open().await;
    let protocol = ProtocolConfiguration {
        request_body_mode: request_mode.into(),
        response_body_mode: BodySendMode::FullDuplexStreamed.into(),
        ..Default::default()
    };
    let reply = session
        .exchange(support::request_headers(protocol, selected, eos))
        .await;
    assert_eq!(
        immediate_status(&[reply]),
        None,
        "{request_mode:?}, {selected:?}, eos={eos}"
    );
    assert!(harness.calls().is_empty());
}

#[rstest::rstest]
#[case::raw_selected("true", "false", true)]
#[case::raw_unselected("false", "true", false)]
#[case::text_selected("", "true", true)]
#[case::text_unselected("", "false", false)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_header_values_are_authoritative_for_actual_guardrail_execution(
    #[case] raw: &str,
    #[case] text: &str,
    #[case] selected: bool,
) {
    let harness = Harness::new(NemoReply::BlockInput, false).await;
    let mut session = harness.open().await;
    session
        .exchange(headers(vec![HeaderValue {
            key: "x-aigateway-guardrail-input".to_owned(),
            value: text.to_owned(),
            raw_value: raw.as_bytes().to_vec(),
        }]))
        .await;
    let response = session.exchange(request("hello", false)).await;
    assert_eq!(
        immediate_status(&[response]),
        selected.then_some(403),
        "raw={raw}, value={text}"
    );
    assert_eq!(harness.calls().len(), usize::from(selected));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_selection_headers_do_not_evaluate_attached_checks() {
    let harness = Harness::new(NemoReply::BlockInput, false).await;
    let mut session = harness.open().await;
    session.exchange(headers(Vec::new())).await;
    let response = session.exchange(request("hello", false)).await;
    assert_eq!(immediate_status(&[response]), None);
    assert!(harness.calls().is_empty());
}

#[rstest::rstest]
#[case::false_then_true(vec![support::header("x-aigateway-guardrail-input", "false"), support::header("x-aigateway-guardrail-input", "true")])]
#[case::true_then_false(vec![support::header("x-aigateway-guardrail-input", "true"), support::header("x-aigateway-guardrail-input", "false")])]
#[case::mixed_case_duplicate(vec![support::header("x-aigateway-guardrail-input", "true"), support::header("X-AIGATEWAY-GUARDRAIL-INPUT", "true")])]
#[case::comma_separated(vec![support::header("x-aigateway-guardrail-input", "true,false")])]
#[case::uppercase_boolean(vec![support::header("x-aigateway-guardrail-input", "TRUE")])]
#[case::empty_value(vec![support::header("x-aigateway-guardrail-input", "")])]
#[case::invalid_utf8(vec![HeaderValue { key: "x-aigateway-guardrail-input".to_owned(), value: "true".to_owned(), raw_value: vec![255] }])]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_or_malformed_selection_is_rejected_before_any_callout(#[case] selections: Vec<HeaderValue>) {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.open().await;
    let response = session.exchange(headers(selections)).await;
    assert_eq!(immediate_status(std::slice::from_ref(&response)), Some(400));
    assert!(support::immediate_body(&response).contains("unique boolean values"));
    assert!(harness.calls().is_empty());
    assert!(session.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_guardrail_metadata_does_not_require_boolean_values() {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.open().await;
    let response = session
        .exchange(headers(vec![
            support::header("x-aigateway-guardrail-version", "v2"),
            support::header("X-AIGATEWAY-GUARDRAIL-VERSION", "preview"),
            support::header("x-aigateway-guardrail-input-format", "json"),
            support::header("x-aigateway-guardrail-input", "true"),
        ]))
        .await;
    assert_eq!(
        immediate_status(&[response]),
        None,
        "metadata must not be treated as selection"
    );
    let response = session.exchange(request("hello", false)).await;
    assert_eq!(immediate_status(&[response]), None);
    assert_eq!(harness.calls().len(), 1, "the actual Input selector must still run");
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_check_ids_and_output_selectors_still_reject_ambiguity(
    #[values("x-aigateway-guardrail-output".to_owned(),
        format!("x-aigateway-guardrail-{}", "a".repeat(64)),)]
    name: String,
    #[values(false, true)] duplicate: bool,
) {
    let selections = if duplicate {
        vec![
            support::header(&name, "true"),
            support::header(&name.to_ascii_uppercase(), "false"),
        ]
    } else {
        vec![support::header(&name, "invalid")]
    };
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.open().await;
    let response = session.exchange(headers(selections)).await;
    assert_eq!(immediate_status(&[response]), Some(400), "{name}");
    assert!(harness.calls().is_empty());
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_output_cannot_be_silently_disabled_by_none_response_mode(#[values(false, true)] selected: bool) {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.session(BodySendMode::None, false, selected).await;
    session.exchange(request("hello", false)).await;
    let response = session.exchange(response_headers("application/json")).await;
    assert_eq!(
        immediate_status(std::slice::from_ref(&response)),
        selected.then_some(502)
    );
    if selected {
        assert!(support::immediate_body(&response).contains("NONE mode is unsupported"));
    }
    assert!(harness.calls().is_empty());
}

fn headers(selections: Vec<HeaderValue>) -> ProcessingRequest {
    let mut headers = vec![
        support::header(":method", "POST"),
        support::header(":path", "/v1/chat/completions"),
    ];
    headers.extend(selections);
    let mut request = support::wrap(RequestMessage::RequestHeaders(HttpHeaders {
        headers: Some(HeaderMap { headers }),
        ..Default::default()
    }));
    request.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: BodySendMode::Buffered.into(),
        response_body_mode: BodySendMode::Streamed.into(),
        ..Default::default()
    });
    request
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_guardrail_services_require_complete_request_modes_without_a_selector(
    #[values(BodySendMode::None, BodySendMode::Streamed)] request_mode: BodySendMode,
    #[values((false, false), (true, false), (false, true), (true, true))] selected: (bool, bool),
) {
    let harness = Harness::embedded(NemoReply::Pass).await;
    let mut session = harness.open().await;
    let protocol = ProtocolConfiguration {
        request_body_mode: request_mode.into(),
        response_body_mode: BodySendMode::FullDuplexStreamed.into(),
        send_body_without_waiting_for_header_response: true,
    };
    let reply = session
        .exchange(support::request_headers(protocol, selected, false))
        .await;
    assert!(support::immediate_body(&reply).contains("guardrail selection cannot be determined"));
    assert_eq!(immediate_status(&[reply]), Some(500));
    assert!(session.next().await.is_none());
    assert!(harness.calls().is_empty());
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn embedded_guardrail_services_accept_complete_request_modes_without_a_selector(
    #[values(BodySendMode::Buffered, BodySendMode::FullDuplexStreamed)] request_mode: BodySendMode,
    #[values(BodySendMode::Buffered, BodySendMode::FullDuplexStreamed)] response_mode: BodySendMode,
) {
    let harness = Harness::embedded(NemoReply::Pass).await;
    let mut session = harness.open().await;
    session
        .send(support::request_headers(
            ProtocolConfiguration {
                request_body_mode: request_mode.into(),
                response_body_mode: response_mode.into(),
                send_body_without_waiting_for_header_response: false,
            },
            (true, true),
            false,
        ))
        .await;
    if request_mode == BodySendMode::Buffered {
        assert!(matches!(
            session.next().await.unwrap().response,
            Some(ResponseMessage::RequestHeaders(_))
        ));
    }
    session.send(request("embedded prompt", false)).await;
    if request_mode == BodySendMode::FullDuplexStreamed {
        assert!(matches!(
            session.next().await.unwrap().response,
            Some(ResponseMessage::RequestHeaders(_))
        ));
    }
    assert!(matches!(
        session.next().await.unwrap().response,
        Some(ResponseMessage::RequestBody(_))
    ));
    session.send(response_headers("application/json")).await;
    if response_mode == BodySendMode::Buffered {
        assert!(matches!(
            session.next().await.unwrap().response,
            Some(ResponseMessage::ResponseHeaders(_))
        ));
    }
    let document =
        serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":"embedded answer"}}]})).unwrap();
    session.send(response_body(&document, true)).await;
    if response_mode == BodySendMode::FullDuplexStreamed {
        assert!(matches!(
            session.next().await.unwrap().response,
            Some(ResponseMessage::ResponseHeaders(_))
        ));
    }
    let reply = session.next().await.unwrap();
    assert!(matches!(reply.response, Some(ResponseMessage::ResponseBody(_))));
    assert_eq!(body_bytes(&[reply]), document);
    assert_eq!(harness.calls().len(), 2);
}

#[rstest::rstest]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_guardrail_selection_explains_premature_response_rejection(
    #[values(false, true)] embedded: bool,
    #[values(false, true)] no_header_wait: bool,
) {
    let harness = if embedded {
        Harness::embedded(NemoReply::Pass).await
    } else {
        Harness::configured(NemoReply::Pass, false, 1_048_576, |cfg| {
            let guardrail = cfg.filter_chains[0]
                .filters
                .iter_mut()
                .find(|entry| entry.filter_type == "ai_guardrails")
                .unwrap();
            guardrail.conditions =
                serde_yaml::from_str("[{when: {headers: {x-aigateway-guardrail-input: 'true'}, path: /selected}}]")
                    .unwrap();
        })
        .await
    };
    let mut session = harness.open().await;
    session
        .send(support::request_headers(
            ProtocolConfiguration {
                request_body_mode: BodySendMode::FullDuplexStreamed.into(),
                response_body_mode: BodySendMode::FullDuplexStreamed.into(),
                send_body_without_waiting_for_header_response: no_header_wait,
            },
            (false, false),
            false,
        ))
        .await;
    session
        .send(support::wrap(RequestMessage::RequestBody(HttpBody {
            body: br#"{"messages":["#.to_vec(),
            end_of_stream: false,
        })))
        .await;
    let reply = session.exchange(response_headers("application/json")).await;
    assert_eq!(immediate_status(std::slice::from_ref(&reply)), Some(500));
    assert!(reply.mode_override.is_none());
    assert!(
        support::immediate_body(&reply).contains("guardrail selection cannot be determined"),
        "{reply:?}"
    );
    assert!(session.next().await.is_none());
    assert!(harness.calls().is_empty());
}

#[test]
fn invalid_guardrail_phase_fails_pipeline_construction() {
    let mut cfg = support::configuration("http://127.0.0.1:1/v1/checks", false, 1_048_576);
    let guardrail = cfg.filter_chains[0]
        .filters
        .iter_mut()
        .find(|entry| entry.filter_type == "ai_guardrails")
        .unwrap();
    guardrail.config["phase"] = serde_yaml::Value::String("invalid".into());
    let error = praxis_extproc::config::build_pipeline(&cfg, &praxis_ai_filters::build_ai_registry())
        .err()
        .expect("invalid phase must prevent serving")
        .to_string();
    assert!(error.to_ascii_lowercase().contains("phase"), "{error}");
}
