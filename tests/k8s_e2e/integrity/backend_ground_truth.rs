//! Backend ground-truth trust test (#165) — integrity profile only.
//!
//! Complements `header_protection`: the `e2e_header_oracle` reports the
//! ext-proc's *forwarding intent* (what it queued to strip/add), whereas this
//! routes to a header-reflecting echo backend (`model: echo-model`) and asserts
//! what *actually* reached upstream after Envoy applied the ext-proc's
//! mutations. That closes the gap where the oracle says "stripped" but a bug in
//! applying the removal still leaks the header to the backend.

use crate::fixtures::{ensure_gateway_ready, gateway_url, http_client};

/// A client-spoofed `x-maas-*` identity header never reaches the backend.
#[tokio::test]
async fn spoofed_identity_header_never_reaches_backend() {
    ensure_gateway_ready().await;

    let resp = http_client()
        .post(format!("{}/v1/chat/completions", gateway_url()))
        .header("x-maas-tenant", "spoofed-by-client")
        .json(&serde_json::json!({
            "model": "echo-model",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200, "request should route to the echo backend");

    // The echo app reflects the received request (headers included) in its body.
    let body = resp.text().await.expect("read echo body").to_lowercase();

    // Ground truth: identity_header_guard stripped it before the backend saw it.
    assert!(
        !body.contains("x-maas-tenant"),
        "spoofed x-maas-tenant reached the backend; echo body: {body}"
    );
    // Sanity: the request really reached echo carrying the IPP mutation, so the
    // absence above is a strip, not a misroute/empty echo.
    assert!(
        body.contains("x-praxis-processed"),
        "expected the echo backend to reflect the IPP-added header; body: {body}"
    );
}
