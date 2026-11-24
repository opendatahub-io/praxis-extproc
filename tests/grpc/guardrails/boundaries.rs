//! Selection, isolation, response limits, and unsupported stream termination.

use praxis_proto::envoy::service::ext_proc::v3::{
    HttpTrailers, body_mutation, processing_request::Request as RequestMessage,
    processing_response::Response as ResponseMessage,
};

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_only_and_unselected_checks_release_each_sse_chunk_before_eos() {
    for translation in [false, true] {
        for input in [false, true] {
            let harness = Harness::new(NemoReply::Pass, translation).await;
            let mut session = harness.session(BodySendMode::Streamed, input, false).await;
            session.exchange(request("hello", true)).await;
            session.exchange(response_headers("text/event-stream")).await;
            for chunk in [b"data: first\n\n".as_slice(), b"data: second\n\n"] {
                let response = session.exchange(response_body(chunk, false)).await;
                assert_eq!(
                    body_bytes(&[response]),
                    chunk,
                    "a chunk must be acknowledged before the next is sent"
                );
            }
            session.exchange(response_body(b"data: [DONE]\n\n", true)).await;
            assert_eq!(harness.calls().len(), usize::from(input));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_final_body_frame_checks_and_releases_the_buffered_document() {
    for mode in [BodySendMode::Streamed, BodySendMode::FullDuplexStreamed] {
        for reply in [NemoReply::Pass, NemoReply::BlockOutput] {
            let harness = Harness::new(reply, false).await;
            let mut session = harness.session(mode, false, true).await;
            session.exchange(request("hello", false)).await;
            session.send(response_headers("application/json")).await;
            if mode == BodySendMode::Streamed {
                session.next().await.expect("streamed headers acknowledgement");
            }
            let document = json!({"choices":[{"message":{"role":"assistant","content":"complete answer"}}]});
            let encoded = serde_json::to_vec(&document).unwrap();
            session.send(response_body(&encoded, false)).await;
            if mode == BodySendMode::Streamed {
                let pending = session.next().await.unwrap();
                assert_eq!(body_bytes(&[pending]), b"", "{mode:?}: output must wait for EOS");
            }
            assert!(harness.calls().is_empty(), "no verdict before EOS");
            session.send(response_body(b"", true)).await;
            drop(session.sender);
            let mut responses = Vec::new();
            while let Some(response) =
                tokio::time::timeout(std::time::Duration::from_secs(10), session.responses.message())
                    .await
                    .unwrap()
                    .unwrap()
            {
                responses.push(response);
            }
            if mode == BodySendMode::FullDuplexStreamed {
                let Some(ResponseMessage::ResponseBody(body)) = &responses.last().unwrap().response else {
                    panic!("the final FDS message must carry a response body");
                };
                let mutation = body.response.as_ref().unwrap().body_mutation.as_ref().unwrap();
                assert!(
                    matches!(&mutation.mutation, Some(body_mutation::Mutation::StreamedResponse(body)) if body.end_of_stream),
                    "the final FDS mutation must be StreamedResponse with EOS set"
                );
            }
            let output: serde_json::Value = serde_json::from_slice(&body_bytes(&responses)).unwrap();
            let calls = harness.calls();
            assert_eq!(calls.len(), 1, "{mode:?}: exactly one complete Output check");
            assert_eq!(calls[0]["messages"][0]["content"], "complete answer");
            match reply {
                NemoReply::Pass => assert_eq!(output, document),
                NemoReply::BlockOutput => {
                    assert_eq!(output["error"]["code"], "content_blocked");
                    assert!(output.get("choices").is_none(), "blocked content must be replaced");
                },
                _ => unreachable!(),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_selected_and_unselected_requests_do_not_share_guardrail_state() {
    let harness = Harness::new(NemoReply::BlockOutput, true).await;
    let requests = (0..16).map(|index| {
        let harness = &harness;
        async move {
            let selected = index % 2 == 0;
            let replies = harness
                .complete(BodySendMode::Streamed, (false, selected), "hello", "unsafe answer")
                .await;
            let output = String::from_utf8(body_bytes(&replies)).unwrap();
            assert_eq!(output.contains("content_blocked"), selected);
            assert_eq!(output.contains("unsafe answer"), !selected);
        }
    });
    futures::future::join_all(requests).await;
    assert_eq!(harness.calls().len(), 8, "only selected requests call the provider");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_and_filter_response_limits_fail_closed_before_provider_evaluation() {
    for (server_limit, accepted) in [(256, 256), (2_097_152, 1_048_576)] {
        let harness = Harness::with_limit(NemoReply::Pass, false, server_limit).await;
        let mut session = harness.session(BodySendMode::Streamed, false, true).await;
        session.exchange(request("hello", false)).await;
        session.exchange(response_headers("application/json")).await;
        let first = session.exchange(response_body(&vec![b' '; accepted], false)).await;
        assert_eq!(body_bytes(&[first]), b"", "at-limit buffered bytes must remain private");
        session.send(response_body(b"x", true)).await;
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), session.responses.message())
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert!(status.message().contains("buffered response exceeds maximum size"));
        assert!(harness.calls().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trailers_cannot_silently_discard_pending_output_checks() {
    let harness = Harness::new(NemoReply::Pass, true).await;
    let mut session = harness.session(BodySendMode::Streamed, false, true).await;
    session.exchange(request("hello", false)).await;
    session.exchange(response_headers("application/json")).await;
    let partial = session.exchange(response_body(br#"{"choices":[]}"#, false)).await;
    assert_eq!(body_bytes(&[partial]), b"");
    let response = session
        .exchange(support::wrap(RequestMessage::ResponseTrailers(HttpTrailers::default())))
        .await;
    assert_eq!(immediate_status(std::slice::from_ref(&response)), Some(502));
    assert!(support::immediate_body(&response).contains("final body message before trailers"));
    assert!(harness.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unexpected_sse_response_is_rejected_even_when_request_was_non_streaming() {
    let harness = Harness::new(NemoReply::Pass, true).await;
    let mut session = harness.session(BodySendMode::Streamed, false, true).await;
    session.exchange(request("hello", false)).await;
    let response = session.exchange(response_headers("text/event-stream")).await;
    assert_eq!(immediate_status(std::slice::from_ref(&response)), Some(502));
    assert!(support::immediate_body(&response).contains("streaming responses are not supported"));
    assert!(harness.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoning_a_buffered_stream_does_not_poison_the_next_request() {
    let harness = Harness::new(NemoReply::Pass, true).await;
    let mut session = harness.session(BodySendMode::Streamed, false, true).await;
    session.exchange(request("hello", false)).await;
    session.exchange(response_headers("application/json")).await;
    let response = session
        .exchange(response_body(b"unfinished private output", false))
        .await;
    assert_eq!(body_bytes(&[response]), b"");
    drop(session);
    let responses = harness
        .complete(BodySendMode::Streamed, (false, true), "new prompt", "new answer")
        .await;
    assert!(
        String::from_utf8(body_bytes(&responses))
            .unwrap()
            .contains("new answer")
    );
    assert_eq!(harness.calls().len(), 1);
    assert_eq!(harness.calls()[0]["messages"][0]["content"], "new answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn withholding_a_chunk_requires_an_explicit_empty_body_mutation() {
    let harness = Harness::new(NemoReply::Pass, false).await;
    let mut session = harness.session(BodySendMode::Streamed, false, true).await;
    session.exchange(request("hello", false)).await;
    session.exchange(response_headers("application/json")).await;
    let response = session.exchange(response_body(b"private prefix", false)).await;
    let Some(ResponseMessage::ResponseBody(body)) = response.response else {
        panic!("expected body acknowledgement")
    };
    assert!(
        matches!(body.response.unwrap().body_mutation.unwrap().mutation, Some(body_mutation::Mutation::Body(bytes)) if bytes.is_empty()),
        "an absent mutation would forward the original private chunk"
    );
}
