//! Anthropic provider column.
//!
//! Exercises the Anthropic-format endpoint (`/v1/messages`) end to end. The
//! `anthropic_messages_to_chat_completions` filter translates **both**
//! directions: the request body Anthropic Messages → OpenAI chat-completions
//! before it reaches the OpenAI-mode backend, and the non-streaming 200 JSON
//! response OpenAI → Anthropic Messages on the way back. So a client that
//! speaks Anthropic sends Anthropic and receives Anthropic — responses come
//! back in Anthropic shape (`type: "message"`, `content: [{type, text}]`),
//! not OpenAI shape.
//!
//! Routing is header-based like every other column: BBR's `model_to_header`
//! lifts the body `model` (`gpt-4`) into `X-Gateway-Model-Name`; the
//! `/v1/messages` route matches that header, rewrites the path to
//! `/v1/chat/completions`, and forwards to the OpenAI-mode llm-katan the
//! OpenAI column uses. If FDS deferral drops the header mutation, no route
//! matches → 404, so these tests also cover the BBR path.
//!
//! Streaming (`stream: true`) is the exception: the non-streaming filter
//! declines to transform SSE responses (the `_stream` filter is out of
//! scope), so streaming responses pass through as OpenAI SSE — the test
//! only asserts the request is accepted and streamed, not its shape.

use crate::fixtures::{anthropic_message, anthropic_request, ensure_gateway_ready};

#[tokio::test]
async fn anthropic_smoke_200() {
    ensure_gateway_ready().await;
    let resp = anthropic_message("gpt-4", "Say hello").await;

    assert_eq!(resp.status(), 200, "anthropic chat request should return 200");
}

#[tokio::test]
async fn anthropic_response_has_anthropic_structure() {
    ensure_gateway_ready().await;
    let resp = anthropic_message("gpt-4", "hello").await;

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");

    // Bidirectional translation: the OpenAI backend response is rewritten
    // back to Anthropic Messages shape before it reaches the client.
    assert_eq!(body["type"], "message", "response must be anthropic-shaped, got {body}");
    assert_eq!(body["role"], "assistant", "response role, got {body}");
    assert_eq!(
        body["content"][0]["type"], "text",
        "first content block must be text, got {body}"
    );
    assert!(
        body["content"][0]["text"].is_string(),
        "content block must carry text, got {body}"
    );
}

#[tokio::test]
async fn anthropic_tool_call_passthrough() {
    ensure_gateway_ready().await;
    let resp = anthropic_request(serde_json::json!({
        "model": "gpt-4",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "What's the weather in NYC?"}],
        "tools": [{
            "name": "get_weather",
            "description": "Get weather for a city",
            "input_schema": {
                "type": "object",
                "properties": { "city": { "type": "string" } }
            }
        }]
    }))
    .await;

    // A request carrying Anthropic `tools` must translate to OpenAI `tools`,
    // route, and translate back without breaking the pipeline. Whether the
    // model actually calls the tool is backend-dependent, so assert only that
    // the round-trip succeeds and the response is Anthropic-shaped.
    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    assert_eq!(body["type"], "message", "response must be anthropic-shaped, got {body}");
    assert!(
        body["content"].is_array(),
        "anthropic content must be an array, got {body}"
    );
}

#[tokio::test]
async fn anthropic_image_content_passthrough() {
    ensure_gateway_ready().await;
    // 1x1 transparent PNG.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let resp = anthropic_request(serde_json::json!({
        "model": "gpt-4",
        "max_tokens": 1024,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Describe this image"},
                {"type": "image", "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": png
                }}
            ]
        }]
    }))
    .await;

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn anthropic_system_prompt_passthrough() {
    ensure_gateway_ready().await;
    // Anthropic carries the system prompt as a top-level field, not a message.
    let resp = anthropic_request(serde_json::json!({
        "model": "gpt-4",
        "max_tokens": 1024,
        "system": "You are a helpful assistant.",
        "messages": [{"role": "user", "content": "hello"}]
    }))
    .await;

    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn anthropic_multi_turn_conversation() {
    ensure_gateway_ready().await;
    let resp = anthropic_request(serde_json::json!({
        "model": "gpt-4",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "My name is Alex."},
            {"role": "assistant", "content": "Nice to meet you, Alex!"},
            {"role": "user", "content": "What is my name?"}
        ]
    }))
    .await;

    assert_eq!(resp.status(), 200);

    let body: serde_json::Value = resp.json().await.expect("failed to parse JSON");
    assert_eq!(body["type"], "message", "response must be anthropic-shaped, got {body}");
    assert!(
        body["content"].is_array(),
        "anthropic content must be an array, got {body}"
    );
}

#[tokio::test]
async fn anthropic_streaming_not_rejected() {
    ensure_gateway_ready().await;
    let resp = anthropic_request(serde_json::json!({
        "model": "gpt-4",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hello"}],
        "stream": true
    }))
    .await;

    assert_ne!(
        resp.status(),
        400,
        "streaming anthropic request must not be rejected with 400"
    );
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
}
