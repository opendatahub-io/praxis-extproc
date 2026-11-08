use crate::fixtures::{
    REQUEST_TIMEOUT, assert_no_ipp_response_marker, body_size_rejections_total, chat_completion, ensure_gateway_ready,
    gateway_url, http_client, immediate_responses_total, request_body_is_buffered,
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
    let before = immediate_responses_total().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-e2e-validate-json", "true")
        .body("this is not json")
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "the header-gated JSON validator must reject malformed bodies with HTTP 400"
    );

    let after = immediate_responses_total().await;
    assert!(
        after > before,
        "the malformed-body validator must issue an ExtProc ImmediateResponse (before={before}, after={after})"
    );
}

#[tokio::test]
async fn json_validator_allows_valid_chat_completion() {
    ensure_gateway_ready().await;
    let resp = http_client()
        .post(format!("{}/v1/chat/completions", gateway_url()))
        .header("x-e2e-validate-json", "true")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "the header-gated JSON validator must allow valid chat completion bodies"
    );
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

/// Prove ExtProc/Envoy return HTTP 413 for an oversized body (not a generic 5xx).
///
/// Surrounding limits: BBR allows up to 32 MiB; IPP e2e `max_body_bytes` is 1 MiB.
/// Under `FULL_DUPLEX_STREAMED`, Envoy streams the body into IPP so the rejection is
/// ExtProc's `ImmediateResponse` 413 and `praxis_extproc_body_size_rejections_total`
/// must increase. Under BUFFERED, Envoy may 413 before ExtProc sees the body, so
/// we require HTTP 413 plus no IPP response marker instead of the IPP counter.
#[tokio::test]
async fn oversized_body_returns_413_with_mode_specific_proof() {
    ensure_gateway_ready().await;
    let buffered = request_body_is_buffered();
    let before = if buffered {
        0
    } else {
        body_size_rejections_total().await
    };

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
        "oversized body must surface HTTP 413 (ExtProc ImmediateResponse or Envoy buffer), got {}",
        resp.status()
    );

    if buffered {
        assert_no_ipp_response_marker(&resp);
        return;
    }

    let after = body_size_rejections_total().await;
    assert!(
        after > before,
        "FULL_DUPLEX_STREAMED must reject in IPP so body-size counter increases (before={before}, after={after})"
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
