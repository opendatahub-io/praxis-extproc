use std::time::Duration;

const DEFAULT_GATEWAY_URL: &str = "http://172.18.0.200";
const DEFAULT_IPP_METRICS_URL: &str = "http://127.0.0.1:19090/metrics";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const GATEWAY_READY_TIMEOUT: Duration = Duration::from_secs(120);
const GATEWAY_POLL_INTERVAL: Duration = Duration::from_secs(3);

static GATEWAY_READY: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();
static STREAMING_GATEWAY_READY: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::const_new();

/// Wait until the gateway is accepting HTTP connections.
///
/// Polls with a simple GET — any HTTP response (even 404) means the
/// gateway is up. Does NOT depend on ext-proc or routing being functional.
pub(crate) async fn ensure_gateway_ready() {
    ensure_gateway_ready_at(&GATEWAY_READY, gateway_url()).await;
}

/// Wait until the isolated streaming gateway is accepting HTTP connections.
pub(crate) async fn ensure_streaming_gateway_ready() {
    ensure_gateway_ready_at(&STREAMING_GATEWAY_READY, streaming_gateway_url()).await;
}

/// Poll one gateway endpoint once per test binary.
async fn ensure_gateway_ready_at(ready: &'static tokio::sync::OnceCell<bool>, url: String) {
    let ready = ready
        .get_or_init(|| async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("failed to build HTTP client");
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
    chat_completion_with_headers(model, content, &[]).await
}

/// Chat completion with extra request headers (e.g. a conflicting routing header).
pub(crate) async fn chat_completion_with_headers(
    model: &str,
    content: &str,
    headers: &[(&str, &str)],
) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());
    let mut req = client.post(&url).json(&serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": content}]
    }));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.send().await.expect("request failed")
}

/// Send a chat completion request with the e2e guardrail selection header.
pub(crate) async fn guardrail_chat_completion(model: &str, content: &str) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", gateway_url());

    client
        .post(&url)
        .header("x-aigateway-guardrail-e2e", "true")
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": content}]
        }))
        .send()
        .await
        .expect("request failed")
}

/// Send a chat completion with the e2e guardrail selection header through the
/// isolated streaming Gateway.
pub(crate) async fn guardrail_streaming_chat_completion(model: &str, content: &str) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}/v1/chat/completions", streaming_gateway_url());

    client
        .post(&url)
        .header("x-aigateway-guardrail-e2e", "true")
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": content}]
        }))
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

/// Assert that the IPP response-phase marker is absent.
///
/// The marker proves that the IPP response filter ran, but it does not prove
/// that an upstream backend was contacted: Envoy local replies can traverse
/// the response filter too. Use an upstream request counter when that stronger
/// property matters.
pub(crate) fn assert_no_ipp_response_marker(resp: &reqwest::Response) {
    assert!(
        resp.headers().get("X-Praxis-Version").is_none(),
        "X-Praxis-Version present - IPP response filter ran"
    );
}

/// Whether the request direction uses BUFFERED transport in this E2E mode.
pub(crate) fn request_body_is_buffered() -> bool {
    std::env::var("E2E_BODY_MODE")
        .is_ok_and(|mode| mode.eq_ignore_ascii_case("buffered") || mode.eq_ignore_ascii_case("streamed"))
}

/// Prometheus scrape URL for the fail-closed IPP ExtProc metrics port.
pub(crate) fn ipp_metrics_url() -> String {
    std::env::var("IPP_METRICS_URL").unwrap_or_else(|_| DEFAULT_IPP_METRICS_URL.to_owned())
}

/// Current value of `praxis_extproc_body_size_rejections_total` from IPP metrics.
///
/// Sends [`ipp_metrics_token`] as a bearer token. A scrape without a token
/// that passes `TokenReview` and `SubjectAccessReview` is rejected.
///
/// Missing samples count as `0`: Prometheus exporters often omit a counter until
/// it has been incremented at least once.
pub(crate) async fn body_size_rejections_total() -> u64 {
    metric_counter("praxis_extproc_body_size_rejections_total").await
}

/// Current value of `praxis_extproc_immediate_responses_total` from IPP metrics.
pub(crate) async fn immediate_responses_total() -> u64 {
    metric_counter("praxis_extproc_immediate_responses_total").await
}

async fn metric_counter(name: &str) -> u64 {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("build metrics HTTP client");
    let text = client
        .get(ipp_metrics_url())
        .bearer_auth(ipp_metrics_token())
        .send()
        .await
        .expect("scrape IPP metrics")
        .error_for_status()
        .expect("IPP metrics HTTP status")
        .text()
        .await
        .expect("read IPP metrics body");
    parse_prometheus_counter(&text, name).unwrap_or(0)
}

/// Bearer token for the IPP metrics scrape, minted by `hack/scripts/e2e-test.sh`.
fn ipp_metrics_token() -> String {
    std::env::var("IPP_METRICS_TOKEN").expect("IPP_METRICS_TOKEN is unset; hack/scripts/e2e-test.sh mints it")
}

/// Parse an unlabeled (or any-label) counter sample from Prometheus text.
fn parse_prometheus_counter(text: &str, name: &str) -> Option<u64> {
    text.lines().filter(|line| !line.starts_with('#')).find_map(|line| {
        let (metric, value) = line.rsplit_once(' ')?;
        let base = metric.split('{').next()?;
        (base == name).then(|| value.parse().ok())?
    })
}


pub(crate) fn gateway_url() -> String {
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_owned())
}

/// Return the E2E endpoint whose ExtProc chain permits streamed responses.
pub(crate) fn streaming_gateway_url() -> String {
    std::env::var("STREAMING_GATEWAY_URL")
        .expect("E2E harness must set STREAMING_GATEWAY_URL for the unguarded response stream")
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

#[cfg(test)]
mod tests {
    use super::parse_prometheus_counter;

    #[test]
    fn parse_prometheus_counter_reads_sample_and_missing() {
        let text = "\
# HELP praxis_extproc_body_size_rejections_total Total requests rejected
# TYPE praxis_extproc_body_size_rejections_total counter
praxis_extproc_body_size_rejections_total 3
praxis_extproc_requests_total 9
";
        assert_eq!(
            parse_prometheus_counter(text, "praxis_extproc_body_size_rejections_total"),
            Some(3)
        );
        assert_eq!(parse_prometheus_counter(text, "does_not_exist"), None);
        assert_eq!(
            parse_prometheus_counter("# only comments\n", "praxis_extproc_body_size_rejections_total"),
            None
        );
    }
}
