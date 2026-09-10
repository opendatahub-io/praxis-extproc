use crate::fixtures::{assert_praxis_mutations, chat_completion, ensure_gateway_ready, gateway_url, http_client};

#[tokio::test]
async fn smoke_200() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "Say hello").await;

    assert_eq!(resp.status(), 200, "chat completion should return 200");
    assert_praxis_mutations(&resp);
}

#[tokio::test]
async fn response_has_openai_structure() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "hello").await;

    assert_eq!(resp.status(), 200);
    assert_praxis_mutations(&resp);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");

    if body.get("content").is_some() {
        assert!(body["content"].is_array());
        assert!(body["content"][0]["text"].is_string());
        assert!(body["model"].is_string());
        assert!(body["usage"]["input_tokens"].is_number());
        assert!(body["usage"]["output_tokens"].is_number());
    } else {
        assert!(body["choices"].is_array());
        assert!(body["choices"][0]["message"]["content"].is_string());
        assert!(body["model"].is_string());
        assert!(body["usage"]["prompt_tokens"].is_number());
        assert!(body["usage"]["completion_tokens"].is_number());
        assert!(body["usage"]["total_tokens"].is_number());
    }
}

#[tokio::test]
async fn streaming() {
    ensure_gateway_ready().await;
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let content_type = resp
        .headers()
        .get("content-type")
        .expect("missing content-type")
        .to_str()
        .unwrap();
    assert!(
        content_type.contains("text/event-stream"),
        "expected text/event-stream, got {content_type}"
    );

    let body = resp.text().await.expect("failed to read body");
    assert!(body.contains("data: "), "body should contain SSE data lines");
}

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
    assert!(body.get("choices").is_some() || body.get("content").is_some());
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
    let resp = chat_completion("gpt-4", &large_content).await;

    assert_eq!(resp.status(), 200);
}
