use crate::fixtures::{chat_completion, ensure_gateway_ready, gateway_url, http_client};

#[tokio::test]
async fn praxis_version_header_applied() {
    ensure_gateway_ready().await;
    let resp = chat_completion("gpt-4", "hello").await;

    assert_eq!(resp.status(), 200);

    let version = resp
        .headers()
        .get("X-Praxis-Version")
        .expect("missing X-Praxis-Version header")
        .to_str()
        .expect("header value not valid UTF-8");
    assert_eq!(version, "e2e");
}

/// Send a chat completion to `path` carrying an `X-Maas-Username` header,
/// on top of `http_client()`'s default `Authorization` header.
async fn completion_with_identity_headers(path: &str) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}{path}", gateway_url());

    client
        .post(&url)
        .header("X-Maas-Username", "e2e-test-user")
        .json(&serde_json::json!({
            "model": "granite-8b",
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .send()
        .await
        .expect("request failed")
}

/// maas-headers-guard: checks the `X-Guard-Applied` marker the
/// guard filter's own conditions add, not header survival -- a later
/// filter can't observe an earlier filter's queued `request_remove`
/// within the same pass (conditions always see the original request), so
/// a `headers_present` canary doesn't work. See configmap-patch.yaml.
///
/// Uses `/direct/internal` rather than the shared `/v1/chat/completions`,
/// which also carries `gpt-4` tests needing their real `Authorization`
/// header intact for the external llm-katan backend.
#[tokio::test]
async fn maas_headers_guard_strips_on_inference_path() {
    ensure_gateway_ready().await;
    let resp = completion_with_identity_headers("/direct/internal/v1/chat/completions").await;

    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("X-Guard-Applied").is_some(),
        "guard did not apply on an inference path"
    );
}

/// The guard must exempt maas-api's own (non-inference) routes, so a
/// request under /maas-api must not trigger the guard filter.
#[tokio::test]
async fn maas_headers_guard_exempts_maas_api_path() {
    ensure_gateway_ready().await;
    let resp = completion_with_identity_headers("/maas-api/v1/chat/completions").await;

    assert_eq!(resp.status(), 200);
    assert!(
        resp.headers().get("X-Guard-Applied").is_none(),
        "guard applied on an exempted maas-api path"
    );
}
