use crate::fixtures::{assert_praxis_mutations, chat_completion, ensure_gateway_ready, gateway_url, http_client};

#[tokio::test]
async fn smoke_200() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "Say hello").await;

    assert_eq!(resp.status(), 200, "chat completion should return 200");
    assert_praxis_mutations(&resp);
}

#[tokio::test]
async fn response_has_valid_structure() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "hello").await;

    assert_eq!(resp.status(), 200);
    assert_praxis_mutations(&resp);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");

    if body.get("choices").is_some() {
        assert!(body["choices"].is_array());
        assert!(body["choices"][0]["message"]["content"].is_string());
        assert!(body["model"].is_string());
        assert!(body["usage"]["prompt_tokens"].is_number());
        assert!(body["usage"]["completion_tokens"].is_number());
        assert!(body["usage"]["total_tokens"].is_number());
    } else {
        assert!(body["content"].is_array());
        assert!(body["content"][0]["text"].is_string());
        assert!(body["model"].is_string());
        assert!(body["usage"]["input_tokens"].is_number());
        assert!(body["usage"]["output_tokens"].is_number());
    }
}

// NOTE: a plain streaming-passthrough test (expecting 200 + an SSE body) is
// deliberately absent. The e2e overlay enables `ai_guardrails` with a response
// phase at the gateway level, so every request flows through it. As of
// praxis-ai 0.7.0 (praxis-proxy/ai#1604) the filter fails closed on streaming
// while a response guardrail is active, rejecting `stream: true` with a 422
// before the upstream is reached — SSE passthrough is therefore impossible in
// this overlay. That fail-closed contract is covered by
// `guardrails::streaming_request_rejected_when_response_guardrails_enabled`.

#[tokio::test]
async fn tool_call_passthrough() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "What's the weather in NYC?"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": { "city": { "type": "string" } }
                    }
                }
            }]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    tracing::debug!(
        "dual-format validation: handles both Anthropic Messages and OpenAI \
         ChatCompletions responses; E2E mock simulator echoes text blocks \
         rather than generating actual tool calls"
    );
    if let Some(choices) = body.get("choices") {
        let msg = choices[0]["message"].as_object().unwrap();
        assert!(
            msg.contains_key("tool_calls") || msg.contains_key("content"),
            "OpenAI response should contain tool_calls or content"
        );
    } else if let Some(content) = body.get("content") {
        assert!(
            content
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use" || b["type"] == "text"),
            "Anthropic response should contain a tool_use or text content block"
        );
    } else {
        panic!("Response body is neither OpenAI nor Anthropic format: {body:?}");
    }
}

#[tokio::test]
async fn image_content_passthrough() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Describe this image"},
                    {"type": "image_url", "image_url": {
                        "url": "https://example.com/img.png"
                    }}
                ]
            }]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn system_prompt_passthrough() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "hello"}
            ]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn multi_turn_conversation() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "You are a helpful assistant."},
                {"role": "user", "content": "My name is Alex."},
                {"role": "assistant", "content": "Nice to meet you, Alex!"},
                {"role": "user", "content": "What is my name?"}
            ]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    assert!(body.get("choices").is_some_and(|c| c.is_array()) || body.get("content").is_some_and(|c| c.is_array()));
}

#[tokio::test]
async fn large_body_passthrough() {
    ensure_gateway_ready().await;
    let large_content = "x".repeat(100_000);
    // Deterministic large-body passthrough on the in-cluster model: its throughput
    // is predictable regardless of how much concurrent traffic the suite drives.
    let resp = chat_completion("granite-8b", &large_content).await;

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn large_body_passthrough_external() {
    ensure_gateway_ready().await;
    let large_content = "x".repeat(100_000);
    // Keep a large-body case on the external route: the direct/routing tests only
    // send short prompts, so this preserves large-body coverage end-to-end on the
    // external provider path.
    let resp = chat_completion("gpt-4", &large_content).await;

    assert_eq!(resp.status(), 200);
}
