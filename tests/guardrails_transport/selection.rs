//! Guardrail selection headers must have one interpretation across phases.

use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{HeaderMap, HttpHeaders, ProtocolConfiguration, processing_request::Request as RequestMessage},
};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_header_values_are_authoritative_for_actual_guardrail_execution() {
    for (raw, text, selected) in [
        ("true", "false", true),
        ("false", "true", false),
        ("", "true", true),
        ("", "false", false),
    ] {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_or_malformed_selection_is_rejected_before_any_callout() {
    let header = |value| support::header("x-aigateway-guardrail-input", value);
    for selections in [
        vec![header("false"), header("true")],
        vec![header("true"), header("false")],
        vec![header("true"), support::header("X-AIGATEWAY-GUARDRAIL-INPUT", "true")],
        vec![header("true,false")],
        vec![header("TRUE")],
        vec![header("")],
        vec![HeaderValue {
            key: "x-aigateway-guardrail-input".to_owned(),
            value: "true".to_owned(),
            raw_value: vec![255],
        }],
    ] {
        let harness = Harness::new(NemoReply::Pass, false).await;
        let mut session = harness.open().await;
        let response = session.exchange(headers(selections)).await;
        assert_eq!(immediate_status(std::slice::from_ref(&response)), Some(400));
        assert!(support::immediate_body(&response).contains("unique boolean values"));
        assert!(harness.calls().is_empty());
        assert!(session.next().await.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_output_cannot_be_silently_disabled_by_none_response_mode() {
    for selected in [false, true] {
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
}

fn headers(selections: Vec<HeaderValue>) -> praxis_proto::envoy::service::ext_proc::v3::ProcessingRequest {
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
