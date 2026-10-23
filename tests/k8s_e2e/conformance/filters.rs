use crate::fixtures::{chat_completion, ensure_gateway_ready};

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
