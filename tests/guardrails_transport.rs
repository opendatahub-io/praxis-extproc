//! Real AI guardrail contracts across the ExtProc transport and body modes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::missing_assert_message,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    missing_docs,
    reason = "integration test utilities and assertions"
)]

#[path = "guardrails_transport/support.rs"]
mod support;

#[path = "guardrails_transport/boundaries.rs"]
mod boundaries;

#[path = "guardrails_transport/selection.rs"]
mod selection;

#[cfg(feature = "integration")]
#[path = "guardrails_transport/envoy.rs"]
mod envoy;

use praxis_proto::envoy::service::ext_proc::v3::BodySendMode;
use serde_json::json;
use support::{Harness, NemoReply, body_bytes, immediate_status, request, response_body, response_headers};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_phases_call_nemo_with_complete_content_in_every_response_mode() {
    for mode in [
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed,
    ] {
        for translation in [false, true] {
            for (input, output) in [(false, false), (true, false), (false, true), (true, true)] {
                let harness = Harness::new(NemoReply::Pass, translation).await;
                let response = harness.complete(mode, (input, output), "hello", "safe answer").await;
                assert_eq!(immediate_status(&response), None, "{mode:?}, translation={translation}");
                assert!(body_bytes(&response).windows(11).any(|bytes| bytes == b"safe answer"));
                let calls = harness.calls();
                assert_eq!(
                    calls.len(),
                    usize::from(input) + usize::from(output),
                    "{mode:?}, {input}/{output}"
                );
                let phases: Vec<_> = calls
                    .iter()
                    .map(|call| call["guardrails"]["rail_types"][0].as_str().unwrap())
                    .collect();
                assert_eq!(phases.contains(&"input"), input);
                assert_eq!(phases.contains(&"output"), output);
                for call in calls {
                    let expected = if call["guardrails"]["rail_types"][0] == "input" {
                        "hello"
                    } else {
                        "safe answer"
                    };
                    assert_eq!(
                        call["messages"][0]["content"], expected,
                        "provider must inspect the actual content"
                    );
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_output_is_withheld_until_the_complete_response_is_checked() {
    for reply in [NemoReply::Pass, NemoReply::BlockOutput] {
        let harness = Harness::new(reply, true).await;
        let mut session = harness.session(BodySendMode::Streamed, false, true).await;
        session.exchange(request("hello", false)).await;
        session.exchange(response_headers("application/json")).await;
        let encoded =
            serde_json::to_vec(&json!({"choices":[{"message":{"role":"assistant","content":"unsafe secret"}}]}))
                .unwrap();
        let split = encoded.len() / 2;
        let first = session.exchange(response_body(&encoded[..split], false)).await;
        assert_eq!(body_bytes(&[first]), b"", "no partial unvetted output may be released");
        assert!(
            harness.calls().is_empty(),
            "the provider must receive a complete document"
        );
        let last = session.exchange(response_body(&encoded[split..], true)).await;
        let output = String::from_utf8(body_bytes(&[last])).unwrap();
        assert_eq!(harness.calls().len(), 1, "one complete Output check must run");
        match reply {
            NemoReply::Pass => assert!(output.contains("unsafe secret")),
            NemoReply::BlockOutput => {
                assert!(output.contains("content_blocked"));
                assert!(!output.contains("unsafe secret"));
            },
            _ => unreachable!(),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_provider_failures_never_release_unvetted_content() {
    for mode in [
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed,
    ] {
        for reply in [
            NemoReply::Unavailable,
            NemoReply::Malformed,
            NemoReply::Unknown,
            NemoReply::Timeout,
        ] {
            let harness = Harness::new(reply, false).await;
            let responses = harness.complete(mode, (false, true), "hello", "unvetted secret").await;
            let bytes = String::from_utf8(body_bytes(&responses)).unwrap();
            assert!(bytes.contains("evaluation_failed"), "{mode:?} {reply:?}: {bytes}");
            assert!(!bytes.contains("unvetted secret"), "provider failure must fail closed");
            assert_eq!(
                harness.calls().len(),
                1,
                "the intended provider failure must be exercised"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_input_blocks_before_any_upstream_response_is_needed() {
    let harness = Harness::new(NemoReply::BlockInput, false).await;
    let mut session = harness.session(BodySendMode::Streamed, true, true).await;
    let blocked = session.exchange(request("unsafe input", false)).await;
    assert_eq!(immediate_status(&[blocked]), Some(403));
    assert_eq!(harness.calls().len(), 1);
    assert_eq!(harness.calls()[0]["guardrails"]["rail_types"][0], "input");
    assert!(session.next().await.is_none(), "the rejected stream must terminate");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_guarded_sse_requests_are_explicitly_rejected_in_all_response_modes() {
    for mode in [
        BodySendMode::Buffered,
        BodySendMode::Streamed,
        BodySendMode::FullDuplexStreamed,
    ] {
        for translation in [false, true] {
            let harness = Harness::new(NemoReply::Pass, translation).await;
            let mut session = harness.session(mode, false, true).await;
            let response = session.exchange(request("hello", true)).await;
            assert_eq!(immediate_status(std::slice::from_ref(&response)), Some(422));
            assert!(support::immediate_body(&response).contains("streaming responses are not supported"));
            assert!(
                harness.calls().is_empty(),
                "unsupported streaming must be rejected before callouts"
            );
        }
    }
}
