//! BBR routing for non-chat JSON OpenAI paths (same pre-chain as chat).
//!
//! RHAIRFE-2245 scope includes JSON suffixes such as `/v1/completions`,
//! `/v1/embeddings`, `/v1/rerank`, `/v1/ocr`, `/v1/moderations`, `/v1/extraction`,
//! `/v1/decisions`, `/v1/responses`, `/v1/images/generations`, `/v1/audio/voices`,
//! and `/v1/search/*` — all use the same `json_body` → `X-Gateway-Model-Name`
//! pre-chain when `"model"` is in the body. Multipart paths (e.g. STT, image edits)
//! are out of scope here.
//!
//! CI exercises `/v1/embeddings` against the inference sim; fill the spike matrix
//! for other suffixes on a MaaS+Praxis cluster.

use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready, json_bbr_post};

#[tokio::test]
async fn granite_8b_bbr_embeddings_not_gateway_404() {
    ensure_gateway_ready().await;
    let resp = json_bbr_post(
        "embeddings",
        &serde_json::json!({
            "model": "granite-8b",
            "input": "hello"
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        200,
        "granite-8b on /v1/embeddings must match header route and reach internal-model"
    );
    assert_praxis_mutations(&resp);
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
    ensure_gateway_ready().await;
    let resp = json_bbr_post(
        "completions",
        &serde_json::json!({
            "model": "granite-8b",
            "prompt": "hello",
            "max_tokens": 8
        }),
    )
    .await;

    assert_eq!(
        resp.status(),
        200,
        "granite-8b on /v1/completions must match header route and reach internal-model"
    );
    assert_praxis_mutations(&resp);
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
