//! Malformed-JSON rejection — conformance-only.
//!
//! Rejection relies on a fail-closed body-parsing filter (`ai_guardrails`),
//! which only the conformance chain has; the integrity chain tolerates bad JSON
//! (`json_body on_invalid: continue`), so this contract is conformance-specific.

use crate::fixtures::{ensure_gateway_ready, gateway_url, http_client};

/// `X-Praxis-Version` is only set by the IPP response-phase `headers` filter,
/// which runs once a real upstream response comes back. Its absence means the
/// request was rejected before the backend was ever dialed.
fn assert_not_forwarded(resp: &reqwest::Response) {
    assert!(
        resp.headers().get("X-Praxis-Version").is_none(),
        "X-Praxis-Version present — request reached the backend instead of being rejected"
    );
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
    assert_not_forwarded(&resp);
}
