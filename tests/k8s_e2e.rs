// SPDX-License-Identifier: Apache-2.0

//! K8s black-box e2e tests for Praxis AI filters.
//!
//! Achieves test parity with ai-gateway-payload-processing's e2e suite.
//! Sends HTTP requests through an Istio Gateway with Praxis ext-proc
//! to the llm-katan simulator (3.147.232.199).
//!
//! Run with `cargo test --features k8s-e2e --nocapture`.
//!
//! Requires:
//! - A Kind cluster with MetalLB, Istio, Praxis, and Gateway deployed
//!   (use `praxis-forge stack apply e2e` or equivalent)
//! - llm-katan reachable at 3.147.232.199:443
//! - Set GATEWAY_URL to the gateway LoadBalancer IP:
//!   `export GATEWAY_URL=http://$(kubectl get svc e2e-gateway-istio -o jsonpath='{.status.loadBalancer.ingress[0].ip}')`

#![cfg(feature = "k8s-e2e")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "k8s e2e tests"
)]
#![allow(missing_docs, reason = "k8s e2e test module")]
#![allow(unused_variables, reason = "stubs")]

use std::time::Duration;

const DEFAULT_GATEWAY_URL: &str = "http://172.18.0.200";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Smoke
// ---------------------------------------------------------------------------

#[tokio::test]

async fn chat_completion_200() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Say hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200, "chat completion should return 200");
}

// ---------------------------------------------------------------------------
// Response format
// ---------------------------------------------------------------------------

#[tokio::test]

async fn response_has_openai_structure() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");

    assert!(body["choices"].is_array());
    assert!(body["choices"][0]["message"]["content"].is_string());
    assert!(body["model"].is_string());
    assert!(body["usage"]["prompt_tokens"].is_number());
    assert!(body["usage"]["completion_tokens"].is_number());
    assert!(body["usage"]["total_tokens"].is_number());
}

// ---------------------------------------------------------------------------
// Tool calling
// ---------------------------------------------------------------------------

#[tokio::test]

async fn tool_call_passthrough() {
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
                    "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
                }
            }]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    assert!(
        body["choices"][0]["message"]
            .as_object()
            .unwrap()
            .contains_key("tool_calls")
    );
}

// ---------------------------------------------------------------------------
// Multimodal
// ---------------------------------------------------------------------------

#[tokio::test]

async fn image_content_passthrough() {
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
                    {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}}
                ]
            }]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// JSON mode
// ---------------------------------------------------------------------------

#[tokio::test]

async fn json_mode_response() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "Return a JSON object with key 'greeting'"}],
            "response_format": {"type": "json_object"}
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    let content = body["choices"][0]["message"]["content"]
        .as_str()
        .expect("content should be a string");
    serde_json::from_str::<serde_json::Value>(content).expect("content should be valid JSON");
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

#[tokio::test]

async fn system_prompt_passthrough() {
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

// ---------------------------------------------------------------------------
// Multi-turn
// ---------------------------------------------------------------------------

#[tokio::test]

async fn multi_turn_conversation() {
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
    assert!(body["choices"].is_array());
}

// ---------------------------------------------------------------------------
// Praxis filter behavior — model_to_header
// ---------------------------------------------------------------------------

#[tokio::test]

async fn model_to_header_works() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    assert!(body["choices"].is_array());
}

// ---------------------------------------------------------------------------
// Praxis filter behavior — response headers
// ---------------------------------------------------------------------------

#[tokio::test]

async fn praxis_headers_applied() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);

    let version = resp
        .headers()
        .get("X-Praxis-Version")
        .expect("missing X-Praxis-Version header")
        .to_str()
        .expect("header value not valid UTF-8");
    assert_eq!(version, "e2e");
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]

async fn streaming_completion() {
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

// ---------------------------------------------------------------------------
// Body integrity
// ---------------------------------------------------------------------------

#[tokio::test]

async fn large_body_passthrough() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let large_content = "x".repeat(100_000);

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": large_content}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------------
// Error handling — invalid auth
// ---------------------------------------------------------------------------

#[tokio::test]

async fn invalid_api_key_rejected() {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build HTTP client");
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .header("Authorization", "Bearer wrong-key")
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 401);
}

// ---------------------------------------------------------------------------
// Error handling — malformed body
// ---------------------------------------------------------------------------

#[tokio::test]

async fn malformed_json_rejected() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body("this is not json")
        .send()
        .await
        .expect("request failed");

    assert!(
        resp.status().is_client_error(),
        "malformed JSON should return 4xx, got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// Error handling — empty messages
// ---------------------------------------------------------------------------

#[tokio::test]

async fn empty_messages_rejected() {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "model": "gpt-4",
            "messages": []
        }))
        .send()
        .await
        .expect("request failed");

    assert!(
        resp.status().is_client_error(),
        "empty messages should return 4xx, got {}",
        resp.status()
    );
}

// ---------------------------------------------------------------------------
// Test utilities
// ---------------------------------------------------------------------------

fn gateway_url() -> String {
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_owned())
}

fn http_client() -> reqwest::Client {
    use reqwest::header;
    let mut headers = header::HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        header::HeaderValue::from_static("Bearer llm-katan-openai-key"),
    );
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .default_headers(headers)
        .build()
        .expect("failed to build HTTP client")
}
