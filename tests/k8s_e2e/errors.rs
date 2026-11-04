use crate::fixtures::{
    REQUEST_TIMEOUT, assert_not_forwarded_to_backend, body_size_rejections_total, chat_completion,
    ensure_gateway_ready, gateway_url, http_client, is_buffered_body_mode,
};


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
async fn malformed_json_rejected() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body("this is not json")
        .send()
        .await
        .expect("request failed");

    // Malformed JSON must be rejected, not forwarded. How it surfaces depends on
    // which filter parses the body first: a converter that reports bad input as a
    // 4xx, or a fail-closed (failure_mode_allow: false) filter like ai_guardrails
    // that returns a FilterError on unparseable input, which Envoy surfaces as a
    // 5xx. Accept either — the contract is "malformed body rejected".
    let status = resp.status();
    assert!(
        status.is_client_error() || status.is_server_error(),
        "malformed JSON must be rejected (4xx when a filter reports bad input, 5xx when a fail-closed filter trips), got {status}"
    );
    assert_not_forwarded_to_backend(&resp);
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
    // rejection surfaces depends on the body mode: under BUFFERED, Envoy may
    // answer with a 413 before ext-proc sees it all; under FULL_DUPLEX_STREAMED,
    // Envoy streams it through and our check_body_limit trips with an
    // ImmediateResponse 413. Accept 413 (preferred) or a fail-closed 5xx if an
    // outer Envoy limit races first — the contract is "oversized body rejected".
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
        "an oversized body must be rejected (413 from ExtProc/Envoy, or 5xx if an outer limit fails closed), got {status}"
    );
}

/// Prove the ExtProc ImmediateResponse 413 path specifically (not a generic 5xx).
///
/// Surrounding limits: BBR allows up to 32 MiB so a 2 MiB body reaches IPP, whose
/// e2e `max_body_bytes` is 1 MiB. That yields HTTP 413 and increments
/// `praxis_extproc_body_size_rejections_total`. BUFFERED also asserts the request
/// never reached the backend; streamed fixtures may have forwarded earlier chunks.
#[tokio::test]
async fn oversized_body_returns_extproc_413_and_increments_rejection_counter() {
    ensure_gateway_ready().await;
    let before = body_size_rejections_total().await;

    let oversized = "x".repeat(2 * 1024 * 1024);
    let resp = http_client()
        .post(format!("{}/v1/chat/completions", gateway_url()))
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": oversized}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::PAYLOAD_TOO_LARGE,
        "body between BBR and IPP caps must surface ExtProc ImmediateResponse 413, got {}",
        resp.status()
    );
    if is_buffered_body_mode() {
        assert_not_forwarded_to_backend(&resp);
    }

    let after = body_size_rejections_total().await;
    assert!(
        after > before,
        "IPP body-size rejection counter must increase (before={before}, after={after})"
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
