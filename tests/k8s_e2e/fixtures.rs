use std::time::Duration;

const DEFAULT_GATEWAY_URL: &str = "http://172.18.0.200";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const GATEWAY_READY_TIMEOUT: Duration = Duration::from_secs(120);
const GATEWAY_POLL_INTERVAL: Duration = Duration::from_secs(3);

static GATEWAY_READY: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

/// Wait until the gateway is accepting HTTP connections.
///
/// Polls with a simple GET — any HTTP response (even 404) means the
/// gateway is up. Does NOT depend on ext-proc or routing being functional.
pub(crate) async fn ensure_gateway_ready() {
    let ready = GATEWAY_READY
        .get_or_init(|| async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build HTTP client");
            let url = gateway_url();
            let deadline = tokio::time::Instant::now() + GATEWAY_READY_TIMEOUT;

            loop {
                match client.get(&url).send().await {
                    Ok(_) => return true,
                    _ if tokio::time::Instant::now() >= deadline => return false,
                    _ => tokio::time::sleep(GATEWAY_POLL_INTERVAL).await,
                }
            }
        })
        .await;

    assert!(*ready, "gateway not ready after {GATEWAY_READY_TIMEOUT:?}");
}

/// Send a chat completion request with the given model and content.
pub(crate) async fn chat_completion(model: &str, content: &str) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

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

/// POST an Anthropic Messages-format chat request to `/v1/messages`.
///
/// The `anthropic_messages_*` filter chain (gated to `/v1/messages` via
/// request `conditions`) translates the request body to OpenAI
/// chat-completions and the non-streaming response back to Anthropic
/// Messages shape; the `/v1/messages` route rewrites the upstream path to
/// `/v1/chat/completions` for the OpenAI-mode backend. The response
/// therefore comes back in Anthropic shape.
pub(crate) async fn anthropic_message(model: &str, content: &str) -> reqwest::Response {
    anthropic_request(serde_json::json!({
        "model": model,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": content}]
    }))
    .await
}

/// POST an arbitrary Anthropic Messages-format body to `/v1/messages`.
///
/// Sets the `anthropic-version` header so the translation filter activates
/// regardless of path rewriting.
pub(crate) async fn anthropic_request(body: serde_json::Value) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}/v1/messages", gateway_url());

    client
        .post(&url)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .expect("request failed")
}

/// Assert that IPP response mutations are present.
///
/// `X-Praxis-Version` is set by the IPP ext-proc `headers` filter.
/// Missing header means FDS deferral is broken or IPP is not running.
pub(crate) fn assert_praxis_mutations(resp: &reqwest::Response) {
    resp.headers().get("X-Praxis-Version").expect(
        "missing X-Praxis-Version — IPP response mutations not applied \
             (FDS deferral broken or IPP ext-proc not running)",
    );
}

pub(crate) fn gateway_url() -> String {
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_owned())
}

/// Build an HTTP client with the llm-katan auth token pre-configured.
pub(crate) fn http_client() -> reqwest::Client {
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
