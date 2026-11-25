// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! RHAIGW-656 guarded byte boundaries over the ExtProc full-duplex protocol.

use std::time::Duration;

use praxis_proto::envoy::service::ext_proc::v3::{
    HttpTrailers, ProcessingRequest, ProcessingResponse, processing_request::Request as RequestMessage,
    processing_response::Response as ResponseMessage,
};

use super::{
    limits::{collect_fds, request_chunk, start_fds},
    support::*,
};
use crate::common::payloads::{Framing, GUARDED_CAP, Phase, SEEDS, exact_document, fragments};

#[derive(Clone, Copy, Debug)]
enum Ending {
    Body,
    EmptyEos,
    Trailers,
}

#[derive(Clone, Copy, Debug)]
struct BoundaryCase {
    ending: Ending,
    framing: Framing,
    phase: Phase,
    seed: &'static str,
    size: usize,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guarded_json_boundary_matrix_preserves_content_and_rejects_before_nemo() {
    for phase in [Phase::Input, Phase::Output] {
        for seed in SEEDS {
            for framing in [Framing::Single, Framing::Chunks, Framing::Splits] {
                let harness = Harness::configured(NemoReply::Pass, false, 2_097_152, |cfg| {
                    if matches!(phase, Phase::Input) {
                        cfg.filter_chains[0].filters.retain(|entry| {
                            entry
                                .config
                                .get("phase")
                                .and_then(|phase| phase.get("response"))
                                .and_then(serde_yaml::Value::as_bool)
                                != Some(true)
                        });
                    }
                })
                .await;
                for ending in [Ending::Body, Ending::EmptyEos, Ending::Trailers] {
                    for size in [GUARDED_CAP - 1, GUARDED_CAP, GUARDED_CAP + 1] {
                        assert_boundary(
                            &harness,
                            BoundaryCase {
                                ending,
                                framing,
                                phase,
                                seed,
                                size,
                            },
                        )
                        .await;
                    }
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

async fn assert_boundary(harness: &Harness, case: BoundaryCase) {
    let BoundaryCase {
        ending,
        framing,
        phase,
        seed,
        size,
    } = case;
    let (body, content) = exact_document(phase, size, seed);
    let before = harness.calls().len();
    let mut session = Box::pin(start_fds(harness, phase.selected())).await;
    if matches!(phase, Phase::Output) {
        Box::pin(session.send(request("small unchecked prompt", false))).await;
        Box::pin(collect_fds(&mut session, false)).await;
        Box::pin(session.send(response_headers("application/json"))).await;
    }
    let chunks = fragments(&body, framing);
    for (index, chunk) in chunks.iter().enumerate() {
        let eos = matches!(ending, Ending::Body) && index + 1 == chunks.len();
        Box::pin(session.send(body_message(phase, chunk, eos))).await;
    }
    if size > GUARDED_CAP {
        assert_overflow(&mut session, phase, framing, ending).await;
        assert_eq!(
            harness.calls().len(),
            before,
            "oversized {phase:?} must not contact NeMo"
        );
        return;
    }
    match ending {
        Ending::Body => {},
        Ending::EmptyEos => Box::pin(session.send(body_message(phase, b"", true))).await,
        Ending::Trailers => {
            let trailers = HttpTrailers::default();
            Box::pin(session.send(wrap(match phase {
                Phase::Input => RequestMessage::RequestTrailers(trailers),
                Phase::Output => RequestMessage::ResponseTrailers(trailers),
            })))
            .await;
        },
    }
    let replies = Box::pin(collect_fds(&mut session, matches!(ending, Ending::Trailers))).await;
    assert_eq!(
        released_bytes(&replies, phase),
        body,
        "{phase:?}, {framing:?}, {ending:?}, size={size}"
    );
    let calls = harness.calls();
    assert_eq!(calls.len(), before + 1, "exactly one evaluation per selected phase");
    assert_eq!(calls[before]["messages"][0]["content"].as_str().unwrap(), content);
    drop(calls);
    drop(session.sender);
    assert!(
        session.responses.message().await.unwrap().is_none(),
        "no duplicate release after completion"
    );
}

fn body_message(phase: Phase, bytes: &[u8], eos: bool) -> ProcessingRequest {
    match phase {
        Phase::Input => request_chunk(bytes, eos),
        Phase::Output => response_body(bytes, eos),
    }
}

async fn assert_overflow(session: &mut Session, phase: Phase, framing: Framing, ending: Ending) {
    loop {
        let reply = tokio::time::timeout(Duration::from_secs(10), session.responses.message())
            .await
            .unwrap();
        match phase {
            Phase::Input => {
                let reply = reply.unwrap().expect("input rejection response");
                assert!(
                    released_bytes(std::slice::from_ref(&reply), phase).is_empty(),
                    "{framing:?}, {ending:?}: oversized input released {} bytes",
                    released_bytes(std::slice::from_ref(&reply), phase).len()
                );
                if immediate_status(&[reply]) == Some(413) {
                    assert!(session.responses.message().await.unwrap().is_none());
                    break;
                }
            },
            Phase::Output => match reply {
                Ok(Some(reply)) => assert!(body_bytes(&[reply]).is_empty(), "no unvetted output"),
                Err(error) => {
                    assert_eq!(error.code(), tonic::Code::ResourceExhausted);
                    break;
                },
                Ok(None) => panic!("oversized output must fail"),
            },
        }
    }
}

fn released_bytes(replies: &[ProcessingResponse], phase: Phase) -> Vec<u8> {
    let bodies: Vec<_> = replies
        .iter()
        .filter_map(|reply| {
            let response = match (phase, reply.response.as_ref()?) {
                (Phase::Input, ResponseMessage::RequestBody(body))
                | (Phase::Output, ResponseMessage::ResponseBody(body)) => ResponseMessage::ResponseBody(body.clone()),
                _ => return None,
            };
            Some(ProcessingResponse {
                response: Some(response),
                ..Default::default()
            })
        })
        .collect();
    body_bytes(&bodies)
}
