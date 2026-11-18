use crate::fixtures::{assert_praxis_mutations, chat_completion, chat_completion_with_headers, ensure_gateway_ready};

#[tokio::test]
async fn gpt4_routes_via_header() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "hello").await;

    assert_eq!(
        resp.status(),
        200,
        "gpt-4 route must match via X-Gateway-Model-Name header"
    );
    assert_praxis_mutations(&resp);
}

#[tokio::test]
async fn granite_8b_routes_to_internal_model() {
    ensure_gateway_ready().await;
    let resp = chat_completion("granite-8b", "hello").await;

    assert_eq!(
        resp.status(),
        200,
        "granite-8b route must match via X-Gateway-Model-Name header \
         and reach the internal model backend"
    );
    assert_praxis_mutations(&resp);
}

#[tokio::test]
async fn unknown_model_returns_404() {
    ensure_gateway_ready().await;
    let resp = chat_completion("nonexistent-model-xyz", "hello").await;

    assert_eq!(
        resp.status(),
        404,
        "unknown model should 404 — no HTTPRoute matches \
         X-Gateway-Model-Name: nonexistent-model-xyz"
    );
}

/// Body model wins over a caller-supplied routing header (ODH BBR posture).
///
/// Spoofed `X-Gateway-Model-Name: granite-8b` with body `model=gpt-4` must
/// still hit the gpt-4 route after BBR strips and re-extracts from the body.
#[tokio::test]
async fn conflicting_routing_header_uses_body_model() {
    ensure_gateway_ready().await;
    let resp = chat_completion_with_headers("gpt-4", "hello", &[("X-Gateway-Model-Name", "granite-8b")]).await;

    assert_eq!(
        resp.status(),
        200,
        "body model gpt-4 must win over spoofed X-Gateway-Model-Name granite-8b"
    );
    assert_praxis_mutations(&resp);

    let body: serde_json::Value = resp.json().await.expect("parse JSON");
    let model = body["model"].as_str().unwrap_or_default();
    assert!(
        model.contains("gpt-4") || model.contains("gpt4"),
        "backend must be the gpt-4 route, not granite-8b; got model={model:?}"
    );
}
