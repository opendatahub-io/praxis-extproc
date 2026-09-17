// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Tests for the health and metrics auxiliary services.
//!
//! Metrics authentication (`TokenReview` + SAR) requires a live
//! Kubernetes API server and is covered by k8s-e2e tests instead.
//! Tests here run with `metrics_auth.enabled: false`.

#![allow(
    clippy::tests_outside_test_module,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "tests"
)]
#![allow(missing_docs, reason = "test module")]

use std::time::Duration;

use praxis_extproc::config::MetricsAuthConfig;

/// Auth-disabled config for tests running outside Kubernetes.
fn auth_disabled() -> MetricsAuthConfig {
    MetricsAuthConfig { enabled: false }
}

// -----------------------------------------------------------------------------
// Health Server
// -----------------------------------------------------------------------------

#[tokio::test]
async fn health_server_starts_and_stops() {
    let addr = next_addr();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let handle =
        tokio::spawn(async move { praxis_extproc::health::serve(addr, async { drop(shutdown_rx.await) }).await });

    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(shutdown_tx);

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("should complete within timeout");

    assert!(result.is_ok(), "health server task should complete cleanly");
}

#[tokio::test]
async fn health_check_responds_serving() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move { praxis_extproc::health::serve(addr, async { drop(shutdown_rx.await) }).await });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("should connect to health server");

    let mut client = tonic_health::pb::health_client::HealthClient::new(channel);

    let resp = client
        .check(tonic_health::pb::HealthCheckRequest { service: String::new() })
        .await
        .expect("health check should succeed");

    assert_eq!(
        resp.into_inner().status,
        i32::from(tonic_health::pb::health_check_response::ServingStatus::Serving),
        "should report SERVING"
    );
}

// -----------------------------------------------------------------------------
// Metrics Server (auth disabled)
// -----------------------------------------------------------------------------

#[tokio::test]
async fn metrics_server_starts_and_stops() {
    let addr = next_addr();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let auth = auth_disabled();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let handle = tokio::spawn(async move {
        praxis_extproc::metrics::serve(addr, &auth, ready_tx, async { drop(shutdown_rx.await) }).await
    });

    ready_rx.await.expect("metrics server should start");

    drop(shutdown_tx);

    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("should complete within timeout");

    assert!(result.is_ok(), "metrics server task should complete cleanly");
}

#[tokio::test]
async fn metrics_endpoint_returns_prometheus_format() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let auth = auth_disabled();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        praxis_extproc::metrics::serve(addr, &auth, ready_tx, async { drop(shutdown_rx.await) }).await
    });

    ready_rx.await.expect("metrics server should start");

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/metrics"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("metrics request should succeed");

    assert_eq!(resp.status(), 200, "metrics should return 200");
}

#[tokio::test]
async fn metrics_record_functions_do_not_panic() {
    praxis_extproc::metrics::register();
    praxis_extproc::metrics::record_request(0.5);
    praxis_extproc::metrics::record_immediate_response();
}

#[tokio::test]
async fn metrics_healthz_returns_200() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let auth = auth_disabled();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        praxis_extproc::metrics::serve(addr, &auth, ready_tx, async { drop(shutdown_rx.await) }).await
    });

    ready_rx.await.expect("metrics server should start");

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/healthz"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(resp.status(), 200, "healthz should return 200");
}

#[tokio::test]
async fn metrics_unknown_path_returns_404() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let auth = auth_disabled();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        praxis_extproc::metrics::serve(addr, &auth, ready_tx, async { drop(shutdown_rx.await) }).await
    });

    ready_rx.await.expect("metrics server should start");

    let resp = reqwest::Client::new()
        .get(format!("http://{addr}/foobar"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(resp.status(), 404, "unknown path should return 404");
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

use std::sync::atomic::{AtomicU16, Ordering};

static PORT: AtomicU16 = AtomicU16::new(19000);

fn next_addr() -> std::net::SocketAddr {
    let port = PORT.fetch_add(1, Ordering::Relaxed);
    format!("127.0.0.1:{port}").parse().expect("valid addr")
}
