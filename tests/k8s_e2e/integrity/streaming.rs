//! Streaming response over the two-hop chain — integrity-only.
//!
//! `stream: true` conflicts with `ai_guardrails` response-phase on the
//! conformance chain (fail-closed 422), so streaming can only be exercised in
//! the integrity profile, whose chain omits response guardrails (#165).

use futures::StreamExt as _;

use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready, gateway_url, http_client};

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
