//! `FULL_DUPLEX_STREAMED` request-body scenarios on the two-hop chain.

use futures::StreamExt as _;

use super::oracle_helpers::{assert_hop_digests, chat_json_with_digest, post_raw_chunked};
use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready, http_client};

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

#[tokio::test]
async fn unselected_guardrail_streams_before_backend_eof() {
    crate::fixtures::ensure_streaming_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", crate::fixtures::streaming_gateway_url());
    let body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": "stream please" }],
        "stream": true
    });
    let bytes = serde_json::to_vec(&body).expect("serialize");
    let pending = client
        .post(url)
        .header("x-e2e-streaming-oracle", "true")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(bytes)
        .send();
    let (mut stream, mut output) = async {
        let resp = pending.await.expect("streaming request failed");
        assert_eq!(resp.status(), 200, "streaming chat completion");
        assert_praxis_mutations(&resp);
        let mut stream = resp.bytes_stream();
        let first = stream
            .next()
            .await
            .expect("first SSE chunk must arrive")
            .expect("SSE transport must succeed");
        (stream, first.to_vec())
    }
    .await;
    while output.len() < b"data: first\n\n".len() {
        let chunk = stream
            .next()
            .await
            .expect("first event must arrive before backend release");
        output.extend_from_slice(&chunk.expect("first event transport must succeed"));
    }
    assert_eq!(
        output, b"data: first\n\n",
        "terminal event must still be held at the backend"
    );
    release_final_event(&client).await;
    while let Some(chunk) = stream.next().await {
        output.extend_from_slice(&chunk.expect("SSE transport must not be truncated"));
    }
    assert_eq!(
        output, b"data: first\n\ndata: [DONE]\n\n",
        "both events and terminal framing must survive the gateway"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

async fn release_final_event(client: &reqwest::Client) {
    client
        .get(format!("{}/oracle/release", crate::fixtures::streaming_gateway_url()))
        .send()
        .await
        .expect("release request must succeed")
        .error_for_status()
        .expect("backend must acknowledge release");
}
