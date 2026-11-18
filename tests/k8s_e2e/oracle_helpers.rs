//! Shared helpers for the `e2e_body_oracle` two-hop body-digest tests.

use reqwest::header::{self, HeaderMap};

use crate::fixtures::{gateway_url, http_client};

/// Hop-1 request-body digest header (pre-IPP).
pub(crate) const HOP_DIGEST_HEADER_1: &str = "x-qualification-request-sha256-hop-1";
/// Hop-2 request-body digest header (post-IPP).
pub(crate) const HOP_DIGEST_HEADER_2: &str = "x-qualification-request-sha256-hop-2";

/// Lowercase hex SHA-256 of `bytes` (matches [`praxis_extproc::e2e::body_oracle::hex_sha256`]).
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    praxis_extproc::e2e::body_oracle::hex_sha256(bytes)
}

/// POST raw bytes to `path` (relative to gateway base URL).
pub(crate) async fn post_raw(path: &str, body: &[u8]) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}{}", gateway_url(), path);
    client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("post_raw request failed")
}

/// POST a JSON chat completion; returns the response and the client body digest.
pub(crate) async fn chat_completion_with_digest(model: &str, content: &str) -> (reqwest::Response, String) {
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": content }]
    });
    let bytes = serde_json::to_vec(&body).expect("serialize chat body");
    let digest = hex_sha256(&bytes);
    let resp = post_raw("/v1/chat/completions", &bytes).await;
    (resp, digest)
}

/// POST chat completion with an explicit JSON body value.
pub(crate) async fn chat_json_with_digest(body: &serde_json::Value) -> (reqwest::Response, String) {
    let bytes = serde_json::to_vec(body).expect("serialize chat body");
    let digest = hex_sha256(&bytes);
    let resp = post_raw("/v1/chat/completions", &bytes).await;
    (resp, digest)
}

/// POST with a chunked request body stream (multiple client chunks).
pub(crate) async fn post_raw_chunked(path: &str, chunks: Vec<Vec<u8>>) -> (reqwest::Response, String) {
    let mut combined = Vec::new();
    for chunk in &chunks {
        combined.extend_from_slice(chunk);
    }
    let digest = hex_sha256(&combined);
    let client = http_client();
    let url = format!("{}{}", gateway_url(), path);
    let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    let resp = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .expect("chunked post failed");
    (resp, digest)
}

/// Compare hop oracle headers to the client-side digest.
pub(crate) fn assert_hop_digests(client_digest: &str, headers: &HeaderMap) {
    for (hop, name) in [(1, HOP_DIGEST_HEADER_1), (2, HOP_DIGEST_HEADER_2)] {
        let value = headers
            .get(name)
            .unwrap_or_else(|| panic!("missing {name} for hop {hop}"));
        let got = value.to_str().unwrap_or_else(|_| panic!("invalid {name} header value"));
        assert_eq!(got, client_digest, "hop {hop} digest mismatch (header {name})");
    }
}
