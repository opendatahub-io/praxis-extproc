use crate::fixtures::{assert_praxis_mutations, ensure_gateway_ready, gateway_url, http_client};

async fn direct_completion(path_prefix: &str, model: &str, content: &str) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}/direct/{}/v1/chat/completions", gateway_url(), path_prefix);

    client
        .post(&url)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": content}]
        }))
        .send()
        .await
        .expect("request failed")
}

#[tokio::test]
async fn direct_external_returns_200() {
    ensure_gateway_ready().await;
    let resp = direct_completion("external", "gpt-4", "hello").await;

    assert_eq!(resp.status(), 200, "direct external route should reach llm-katan");
}

#[tokio::test]
async fn direct_external_has_praxis_mutations() {
    ensure_gateway_ready().await;
    let resp = direct_completion("external", "gpt-4", "hello").await;

    assert_eq!(resp.status(), 200);
    assert_praxis_mutations(&resp);
}

#[tokio::test]
async fn direct_internal_returns_200() {
    ensure_gateway_ready().await;
    let resp = direct_completion("internal", "granite-8b", "hello").await;

    assert_eq!(resp.status(), 200, "direct internal route should reach inference-sim");
}

#[tokio::test]
async fn direct_internal_has_praxis_mutations() {
    ensure_gateway_ready().await;
    let resp = direct_completion("internal", "granite-8b", "hello").await;

    assert_eq!(resp.status(), 200);
    assert_praxis_mutations(&resp);
}
