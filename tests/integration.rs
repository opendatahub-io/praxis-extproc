// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Integration tests requiring a Kubernetes cluster with Istio and
//! praxis-extproc deployed.
//!
//! Run with `cargo test --features integration -- --ignored`.
//!
//! Requires:
//! - A KIND cluster with Istio and praxis-extproc (`make kind-up`)
//! - An active port-forward to the Gateway:
//!   `kubectl -n praxis-test port-forward deploy/praxis-test-istio 18080:8080`

#![cfg(feature = "integration")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::default_trait_access,
    clippy::match_wildcard_for_single_variants,
    clippy::missing_assert_message,
    clippy::needless_pass_by_value,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    reason = "integration tests"
)]
#![allow(missing_docs, reason = "integration test module")]

use std::time::Duration;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

const DEFAULT_GATEWAY_URL: &str = "http://localhost:18080";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Istio echo server includes this in every response body.
const ECHO_MARKER: &str = "ServiceVersion=";

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn gateway_routes_traffic_to_echo_backend() {
    let client = http_client();
    let url = gateway_url();

    let resp = client.get(&url).send().await.expect("request failed");

    assert_eq!(resp.status(), 200, "expected 200 OK from echo backend");

    let body = resp.text().await.expect("failed to read body");

    assert!(
        body.contains(ECHO_MARKER),
        "response body should contain echo marker, got: {body}"
    );
}

#[tokio::test]
#[ignore]
async fn ext_proc_adds_response_header() {
    let client = http_client();
    let url = gateway_url();

    let resp = client.get(&url).send().await.expect("request failed");

    assert_eq!(resp.status(), 200, "expected 200 OK");

    let praxis_header = resp.headers().get("x-praxis");

    assert!(praxis_header.is_some(), "X-Praxis header should be present");
    assert_eq!(
        praxis_header
            .expect("header present after is_some check")
            .to_str()
            .expect("header should be valid UTF-8"),
        "true",
        "X-Praxis header should be 'true'"
    );
}

#[tokio::test]
#[ignore]
async fn post_with_body_passes_through() {
    let client = http_client();
    let url = gateway_url();

    let resp = client
        .post(&url)
        .body("test payload")
        .send()
        .await
        .expect("request failed");

    assert_eq!(resp.status(), 200, "POST should succeed through ext_proc pipeline");
}

#[tokio::test]
#[ignore]
async fn multiple_sequential_requests_succeed() {
    let client = http_client();
    let url = gateway_url();

    for i in 0..5 {
        let resp = client.get(&url).send().await.expect("request failed");

        assert_eq!(resp.status(), 200, "request {i} should succeed");
    }
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

fn gateway_url() -> String {
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| DEFAULT_GATEWAY_URL.to_owned())
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build HTTP client")
}
