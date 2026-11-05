use crate::fixtures::{assert_praxis_mutations, chat_completion, ensure_gateway_ready};

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
