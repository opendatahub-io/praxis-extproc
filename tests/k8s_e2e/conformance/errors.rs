use crate::fixtures::{REQUEST_TIMEOUT, chat_completion, ensure_gateway_ready, gateway_url, http_client};

#[tokio::test]
async fn invalid_api_key_rejected() {
    ensure_gateway_ready().await;
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build HTTP client");
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .header("Authorization", "Bearer wrong-key")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn empty_messages_handled() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "").await;

    let status = resp.status();
    assert!(
        status == 200 || status.is_client_error(),
        "expected 200 or 4xx, got {status}"
    );
}

#[tokio::test]
async fn oversized_body_rejected() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    // The IPP ext-proc caps request bodies at 1 MiB (server.max_body_bytes in
    // the e2e overlay). A 2 MiB payload must be rejected, not forwarded. How the
    // rejection surfaces depends on the body mode: under BUFFERED, Envoy buffers
    // the body and answers with a 413 before ext-proc sees it all; under
    // FULL_DUPLEX_STREAMED, Envoy streams it through and our check_body_limit
    // trips, returning RESOURCE_EXHAUSTED which (failure_mode_allow: false) fails
    // closed as a 5xx. Accept either — the contract is "oversized body rejected".
    let oversized = "x".repeat(2 * 1024 * 1024);
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": oversized}]
        }))
        .send()
        .await
        .expect("request failed");

    let status = resp.status();
    assert!(
        status == reqwest::StatusCode::PAYLOAD_TOO_LARGE || status.is_server_error(),
        "an oversized body must be rejected (413 when Envoy buffers it, 5xx when ext-proc trips its cap), got {status}"
    );
}

#[tokio::test]
async fn local_reply_ahead_of_ipp_keeps_its_status() {
    ensure_gateway_ready().await;
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = http_client()
        .post(&url)
        .header("x-e2e-local-reply", "deny")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        403,
        "a local reply from a filter ahead of ipp must reach the client as sent, not as a 500"
    );
}
