//! Header trust-boundary e2e tests (issue #165) — integrity profile only.
//!
//! The integrity profile runs `identity_header_guard` (prefix `x-maas-`) on hop
//! 1 and `e2e_header_oracle` on both hops. The oracle publishes the *effective*
//! forwarded value of each watched header as an
//! `x-qualification-fwd-<name>-hop-N` response header, encoded as `fwd:<value>`,
//! `stripped`, or `absent`. These tests assert the trust boundary: client-spoofed
//! `x-maas-*` identity headers are stripped, and legitimate filter mutations are
//! forwarded.
//!
//! Requires `PROFILE=integrity` (no `ai_guardrails`; guard + oracles wired).

use reqwest::header::HeaderMap;

use crate::fixtures::{self, gateway_url, http_client};

/// Read a `x-qualification-fwd-*` oracle header value.
fn fwd(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .unwrap_or_else(|| panic!("missing oracle header {name}"))
        .to_str()
        .unwrap_or_else(|_| panic!("invalid {name} header value"))
        .to_owned()
}

/// POST a chat completion for `model` with optional extra request headers.
async fn chat_with_headers(model: &str, extra: &[(&str, &str)]) -> reqwest::Response {
    let mut req = http_client()
        .post(format!("{}/v1/chat/completions", gateway_url()))
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}]
        }));
    for (name, value) in extra {
        req = req.header(*name, *value);
    }
    req.send().await.expect("request failed")
}

/// A client-spoofed `x-maas-*` identity header is stripped at hop 1.
#[tokio::test]
async fn spoofed_x_maas_header_stripped_at_hop1() {
    fixtures::ensure_gateway_ready().await;
    let resp = chat_with_headers("granite-8b", &[("x-maas-tenant", "spoofed-by-client")]).await;
    assert_eq!(
        fwd(resp.headers(), "x-qualification-fwd-x-maas-tenant-hop-1"),
        "stripped",
        "identity_header_guard must strip client-supplied x-maas-* at hop 1"
    );
}

/// The spoofed identity header does not survive to hop 2 as a trusted value.
#[tokio::test]
async fn spoofed_x_maas_header_does_not_cross_to_hop2() {
    fixtures::ensure_gateway_ready().await;
    let resp = chat_with_headers("granite-8b", &[("x-maas-tenant", "spoofed-by-client")]).await;
    let got = fwd(resp.headers(), "x-qualification-fwd-x-maas-tenant-hop-2");
    assert!(
        got == "absent" || got == "stripped",
        "spoofed x-maas-tenant must not reach hop 2 as a forwarded value, got {got:?}"
    );
}

/// The routing header is injected from the body and forwarded at hop 1.
#[tokio::test]
async fn routing_header_injected_and_forwarded_at_hop1() {
    fixtures::ensure_gateway_ready().await;
    let resp = chat_with_headers("granite-8b", &[]).await;
    assert_eq!(
        fwd(resp.headers(), "x-qualification-fwd-x-gateway-model-name-hop-1"),
        "fwd:granite-8b",
        "json_body must inject X-Gateway-Model-Name from the body model at hop 1"
    );
}

/// The IPP `headers` filter adds `X-Praxis-Processed` at hop 2.
#[tokio::test]
async fn praxis_processed_header_added_at_hop2() {
    fixtures::ensure_gateway_ready().await;
    let resp = chat_with_headers("granite-8b", &[]).await;
    assert_eq!(
        fwd(resp.headers(), "x-qualification-fwd-x-praxis-processed-hop-2"),
        "fwd:true",
        "headers filter must add X-Praxis-Processed=true at hop 2"
    );
}

/// A non-identity header the client supplies is left intact (no over-stripping).
#[tokio::test]
async fn non_identity_header_is_not_stripped() {
    fixtures::ensure_gateway_ready().await;
    // X-Gateway-Model-Name is client-supplied here: identity_header_guard
    // (prefix x-maas-) must not touch it, and json_body's `unless
    // headers_present` must leave the client value intact.
    let resp = chat_with_headers("granite-8b", &[("x-gateway-model-name", "granite-8b")]).await;
    assert_eq!(
        fwd(resp.headers(), "x-qualification-fwd-x-gateway-model-name-hop-1"),
        "fwd:granite-8b",
        "non-identity header must pass through unstripped"
    );
}
