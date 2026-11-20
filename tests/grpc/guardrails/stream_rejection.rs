//! Late rejection follows `ProcessingResponse.immediate_response` in the
//! vendored `proto/proto/ext_proc.proto`: earlier output cannot be retracted.

use praxis_proto::envoy::service::ext_proc::v3::{BodySendMode, processing_response::Response as Reply};

use super::support::{FIRST, RejectingServer, body_bytes, immediate_status, response_body};

#[tokio::test]
async fn immediate_response_ends_stream_after_first_or_later_chunk_rejection() {
    for mode in [BodySendMode::Streamed, BodySendMode::FullDuplexStreamed] {
        for accepted_chunks in [0, 1, 3] {
            for reject_at_eos in [false, true] {
                let server = RejectingServer::new(accepted_chunks * FIRST.len()).await;
                let mut session = server.session(mode).await;
                for index in 0..accepted_chunks {
                    session.send(response_body(FIRST, false)).await;
                    if mode == BodySendMode::FullDuplexStreamed && index == 0 {
                        assert!(matches!(
                            session.next().await.unwrap().response,
                            Some(Reply::ResponseHeaders(_))
                        ));
                    }
                    let accepted = session.next().await.unwrap();
                    assert_eq!(
                        body_bytes(&[accepted]),
                        FIRST,
                        "accepted chunks must be emitted before rejection"
                    );
                }

                session
                    .sender
                    .try_send(response_body(b"reject", reject_at_eos))
                    .unwrap();
                session.sender.try_send(response_body(b"later", true)).unwrap();
                let rejected = session.next().await.unwrap();
                assert_eq!(immediate_status(std::slice::from_ref(&rejected)), Some(403));
                assert!(body_bytes(&[rejected]).is_empty());
                assert!(
                    session.next().await.is_none(),
                    "no queued body or trailer reply may follow rejection"
                );
                let calls = server.calls();
                assert_eq!(
                    calls.len(),
                    accepted_chunks + 1,
                    "queued later chunks must not reach filters"
                );
                assert_eq!(calls.last().unwrap(), b"reject");
            }
        }
    }
}
