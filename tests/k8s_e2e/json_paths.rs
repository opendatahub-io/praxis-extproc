//! BBR routing for non-chat JSON OpenAI paths (same pre-chain as chat).
//!
//! Two backends in Kind e2e:
//! - **`granite-8b`** → `llm-d-inference-sim` (`/v1/completions`, `/v1/embeddings`, `/v1/responses`, …)
//! - **`bbr-path-stub`** → WireMock (`/v1/moderations`, `/v1/rerank`, …) for paths the sim does not serve
//!
//! See `examples/bbr-json-paths/README.md` for curl examples.

use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready, json_bbr_post};

const SIM_MODEL: &str = "granite-8b";
const STUB_MODEL: &str = "bbr-path-stub";

async fn assert_bbr_reaches_backend(path_suffix: &str, body: serde_json::Value) {
    ensure_gateway_ready().await;
    let resp = json_bbr_post(path_suffix, &body).await;
    assert_eq!(
        resp.status(),
        200,
        "{path_suffix} must match header route and reach backend (not gateway 404)"
    );
    assert_praxis_mutations(&resp);
}

#[tokio::test]
async fn granite_8b_bbr_embeddings_not_gateway_404() {
    assert_bbr_reaches_backend(
        "embeddings",
        serde_json::json!({ "model": SIM_MODEL, "input": "hello" }),
    )
    .await;
}

#[tokio::test]
async fn unknown_model_bbr_embeddings_gateway_404() {
    ensure_gateway_ready().await;
    let resp = json_bbr_post(
        "embeddings",
        &serde_json::json!({
            "model": "nonexistent-model-xyz",
            "input": "hello"
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        404,
        "unknown model on /v1/embeddings should 404 — no HTTPRoute matches \
         X-Gateway-Model-Name"
    );
}

#[tokio::test]
async fn granite_8b_bbr_completions_routes() {
    assert_bbr_reaches_backend(
        "completions",
        serde_json::json!({
            "model": SIM_MODEL,
            "prompt": "hello",
            "max_tokens": 8
        }),
    )
    .await;
}

#[tokio::test]
async fn granite_8b_bbr_responses_routes() {
    assert_bbr_reaches_backend(
        "responses",
        serde_json::json!({
            "model": SIM_MODEL,
            "input": "hello"
        }),
    )
    .await;
}

#[tokio::test]
async fn unknown_model_bbr_completions_gateway_404() {
    ensure_gateway_ready().await;
    let resp = json_bbr_post(
        "completions",
        &serde_json::json!({
            "model": "nonexistent-model-xyz",
            "prompt": "hello",
            "max_tokens": 8
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        404,
        "unknown model on /v1/completions should 404 — no HTTPRoute matches \
         X-Gateway-Model-Name"
    );
}

#[tokio::test]
async fn stub_model_bbr_moderations_routes() {
    assert_bbr_reaches_backend(
        "moderations",
        serde_json::json!({ "model": STUB_MODEL, "input": "test text" }),
    )
    .await;
}

#[tokio::test]
async fn stub_model_bbr_rerank_routes() {
    assert_bbr_reaches_backend(
        "rerank",
        serde_json::json!({
            "model": STUB_MODEL,
            "query": "q",
            "documents": ["a", "b"]
        }),
    )
    .await;
}

#[tokio::test]
async fn stub_model_bbr_images_generations_routes() {
    assert_bbr_reaches_backend(
        "images/generations",
        serde_json::json!({
            "model": STUB_MODEL,
            "prompt": "a red circle",
            "n": 1,
            "size": "256x256"
        }),
    )
    .await;
}

#[tokio::test]
async fn stub_model_bbr_search_query_routes() {
    assert_bbr_reaches_backend(
        "search/query",
        serde_json::json!({ "model": STUB_MODEL, "query": "hello" }),
    )
    .await;
}
