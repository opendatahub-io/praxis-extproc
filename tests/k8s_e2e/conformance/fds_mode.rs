use crate::fixtures::{ensure_gateway_ready, guardrail_chat_completion};

/// Selected guardrails cannot be enforced while either Envoy body mode is
/// `FULL_DUPLEX_STREAMED`, so the ext-proc request must fail closed.
#[tokio::test]
async fn selected_guardrail_is_rejected_in_fds_mode() {
    ensure_gateway_ready().await;
    let response = guardrail_chat_completion("gpt-4", "hello").await;

    assert!(
        response.status().is_server_error(),
        "a selected guardrail must be rejected in FDS mode, got {}",
        response.status()
    );

    let body = response.text().await.expect("failed to read response body");
    assert!(
        body.contains("FULL_DUPLEX_STREAMED"),
        "the FDS rejection should explain why the request was blocked, got {body:?}"
    );
}
