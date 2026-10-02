//! `FULL_DUPLEX_STREAMED` streaming scenarios on `maas_two_hop_fd_streamed`.

use futures::StreamExt as _;
use praxis_extproc::config::DEFAULT_MAX_BODY_BYTES;

use super::{
    helpers::{assert_hop_digests, chat_json_with_digest, load_topology_profile, post_raw_chunked},
    report::ExtendedRunMetadata,
};
use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready, gateway_url, http_client};

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
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
#[ignore = "qualification tier: requires make e2e-setup-extended"]
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
    let profile = load_topology_profile();
    ExtendedRunMetadata::publish_chain_executed(&profile.name, "FULL_DUPLEX_STREAMED body oracle on two-hop chain")
        .expect("publish chain=executed metadata");
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
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

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn fd_streamed_response_first_chunk_before_eos() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());
    let body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": "stream please" }],
        "stream": true
    });
    let bytes = serde_json::to_vec(&body).expect("serialize");
    let resp = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(bytes)
        .send()
        .await
        .expect("streaming request failed");
    assert_eq!(resp.status(), 200, "streaming chat completion");
    let mut stream = resp.bytes_stream();
    let first = stream
        .next()
        .await
        .expect("expected first response chunk before full body EOS");
    assert!(
        !first.expect("first chunk bytes").is_empty(),
        "first SSE chunk should be non-empty"
    );
    // llm-katan may buffer; we only assert that at least one chunk arrived before we drain.
    while stream.next().await.is_some() {}
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn fd_streamed_above_cap_rejected() {
    ensure_gateway_ready().await;
    // Extended overlay removes `allow_unbounded_body`; cap matches `DEFAULT_MAX_BODY_BYTES`.
    let pad_len = DEFAULT_MAX_BODY_BYTES + 1;
    let padding = "x".repeat(pad_len);
    let (resp, _digest) = chat_json_with_digest(&serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": padding }]
    }))
    .await;
    assert_eq!(
        resp.status().as_u16(),
        413,
        "above-cap FULL_DUPLEX_STREAMED body should be rejected with 413 (got {})",
        resp.status()
    );
}
