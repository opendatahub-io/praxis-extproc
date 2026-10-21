//! `FULL_DUPLEX_STREAMED` request-body scenarios on the two-hop chain.

use crate::{
    fixtures::{assert_praxis_mutations, ensure_gateway_ready},
    oracle_helpers::{assert_hop_digests, chat_json_with_digest, post_raw_chunked},
};

#[tokio::test]
async fn fd_streamed_empty_message_oracle() {
    ensure_gateway_ready().await;
    let (resp, digest) = chat_json_with_digest(&serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": "" }]
    }))
    .await;
    assert_eq!(resp.status(), 200, "empty message content should still route");
    assert_hop_digests(&digest, resp.headers());
}

#[tokio::test]
async fn fd_streamed_small_json_oracle() {
    ensure_gateway_ready().await;
    let (resp, digest) = chat_json_with_digest(&serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": "small fd streamed body" }]
    }))
    .await;
    assert_eq!(resp.status(), 200, "small body chat completion");
    assert_praxis_mutations(&resp);
    assert_hop_digests(&digest, resp.headers());
}

#[tokio::test]
async fn fd_streamed_multi_chunk_request_oracle() {
    ensure_gateway_ready().await;
    let inner = serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": "multi-chunk client upload" }]
    });
    let bytes = serde_json::to_vec(&inner).expect("serialize");
    let mid = bytes.len() / 2;
    let (resp, digest) = post_raw_chunked(
        "/v1/chat/completions",
        vec![bytes[..mid].to_vec(), bytes[mid..].to_vec()],
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "chunked request should succeed when gateway accepts body"
    );
    assert_hop_digests(&digest, resp.headers());
}
