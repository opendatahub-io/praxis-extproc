//! Streaming response over the two-hop chain — integrity-only.
//!
//! `stream: true` conflicts with `ai_guardrails` response-phase on the
//! conformance chain (fail-closed 422), so streaming can only be exercised in
//! the integrity profile, whose chain omits response guardrails (#165).

use futures::StreamExt as _;

use crate::fixtures::{ensure_gateway_ready, gateway_url, http_client};

#[tokio::test]
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
    while stream.next().await.is_some() {}
}
