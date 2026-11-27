// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

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

    let handle = tokio::spawn(async move {
        praxis_extproc::health::serve(addr, true, true, std::future::pending(), async {
            drop(shutdown_rx.await);
        })
        .await
    });

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

    tokio::spawn(async move {
        praxis_extproc::health::serve(addr, true, true, std::future::pending(), async {
            drop(shutdown_rx.await);
        })
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("should connect to health server");

    let mut client = tonic_health::pb::health_client::HealthClient::new(channel);

    let service = <praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer<
        praxis_extproc::server::PraxisExtProc,
    > as tonic::server::NamedService>::NAME
        .to_owned();

    let resp = client
        .check(tonic_health::pb::HealthCheckRequest { service })
        .await
        .expect("health check should succeed");

    assert_eq!(
        resp.into_inner().status,
        i32::from(tonic_health::pb::health_check_response::ServingStatus::Serving),
        "should report SERVING"
    );
}

#[tokio::test]
async fn health_flips_not_serving_on_drain() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();

    // serving=true initially; on_drain fires when `drain_tx` is dropped.
    tokio::spawn(async move {
        praxis_extproc::health::serve(addr, true, true, async { drop(drain_rx.await) }, async {
            drop(shutdown_rx.await);
        })
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("should connect to health server");

    let mut client = tonic_health::pb::health_client::HealthClient::new(channel);

    let service = <praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer<
        praxis_extproc::server::PraxisExtProc,
    > as tonic::server::NamedService>::NAME
        .to_owned();

    // Fire the drain signal; the health server must stay up and flip to NotServing.
    drop(drain_tx);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = client
        .check(tonic_health::pb::HealthCheckRequest { service })
        .await
        .expect("health check should still succeed while draining");

    assert_eq!(
        resp.into_inner().status,
        i32::from(tonic_health::pb::health_check_response::ServingStatus::NotServing),
        "should report NotServing once the drain signal fires"
    );
}

#[tokio::test]
async fn health_check_responds_not_serving() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        praxis_extproc::health::serve(addr, false, false, std::future::pending(), async {
            drop(shutdown_rx.await);
        })
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("should connect to health server");

    let mut client = tonic_health::pb::health_client::HealthClient::new(channel);

    let service = <praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer<
        praxis_extproc::server::PraxisExtProc,
    > as tonic::server::NamedService>::NAME
        .to_owned();

    let resp = client
        .check(tonic_health::pb::HealthCheckRequest { service })
        .await
        .expect("health check should succeed");

    assert_eq!(
        resp.into_inner().status,
        i32::from(tonic_health::pb::health_check_response::ServingStatus::NotServing),
        "should report NotServing"
    );
}

#[tokio::test]
async fn health_reports_fips_service_status() {
    let addr = next_addr();

    let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // serving=false, fips_active=true: the ExtProc and FIPS statuses are independent.
    tokio::spawn(async move {
        praxis_extproc::health::serve(addr, false, true, std::future::pending(), async {
            drop(shutdown_rx.await);
        })
        .await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("valid uri")
        .connect()
        .await
        .expect("should connect to health server");

    let mut client = tonic_health::pb::health_client::HealthClient::new(channel);

    let resp = client
        .check(tonic_health::pb::HealthCheckRequest {
            service: praxis_extproc::health::FIPS_SERVICE.to_owned(),
        })
        .await
        .expect("fips health check should succeed");

    assert_eq!(
        resp.into_inner().status,
        i32::from(tonic_health::pb::health_check_response::ServingStatus::Serving),
        "fips service should report SERVING when FIPS is active"
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

#[test]
fn metrics_record_functions_emit_exact_values_and_labels() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use praxis_extproc::metrics::{
        record_body_size_rejection, record_immediate_response, record_invalid_argument, record_local_reply,
        record_request, register,
    };

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        register();
        for duration in [0.25, 0.5] {
            record_request(duration);
        }
        record_immediate_response();
        record_body_size_rejection();
        for status in [Some(200), Some(302), Some(404), Some(503), None, Some(100)] {
            record_local_reply(status);
        }
        record_invalid_argument("message_order", "invalid_phase_transition");
        record_invalid_argument("message_order", "invalid_phase_transition");
        record_invalid_argument("body_mode", "body_message_in_none_mode");
    });
    let snapshot = snapshotter.snapshot().into_vec();
    let mut seen = std::collections::BTreeSet::new();
    for (composite, _, _, value) in snapshot {
        let key = composite.key();
        let mut labels: Vec<_> = key.labels().map(|label| (label.key(), label.value())).collect();
        labels.sort_unstable();
        assert!(
            seen.insert((
                key.name().to_owned(),
                labels
                    .clone()
                    .into_iter()
                    .map(|(key, value)| (key.to_owned(), value.to_owned()))
                    .collect::<Vec<_>>()
            ))
        );
        match key.name() {
            "praxis_extproc_requests_total" => {
                assert!(labels.is_empty());
                assert_eq!(value, DebugValue::Counter(2));
            },
            "praxis_extproc_immediate_responses_total" | "praxis_extproc_body_size_rejections_total" => {
                assert!(labels.is_empty());
                assert_eq!(value, DebugValue::Counter(1));
            },
            "praxis_extproc_request_duration_seconds" => {
                assert!(labels.is_empty());
                let DebugValue::Histogram(values) = value else {
                    panic!("duration must be a histogram")
                };
                let samples: Vec<f64> = values.into_iter().map(Into::into).collect();
                assert_eq!(samples, [0.25, 0.5]);
            },
            "praxis_extproc_local_replies_total" => {
                assert_eq!(labels.len(), 1);
                assert_eq!(labels[0].0, "status_class");
                let expected = match labels[0].1 {
                    "2xx" | "3xx" | "4xx" | "5xx" => 1,
                    "other" => 2,
                    unexpected => panic!("unexpected status class {unexpected}"),
                };
                assert_eq!(value, DebugValue::Counter(expected));
            },
            "praxis_extproc_invalid_argument_total" => {
                let expected = if labels == [("detail", "invalid_phase_transition"), ("reason", "message_order")] {
                    2
                } else {
                    assert_eq!(
                        labels,
                        [("detail", "body_message_in_none_mode"), ("reason", "body_mode")]
                    );
                    1
                };
                assert_eq!(value, DebugValue::Counter(expected));
            },
            unexpected => panic!("unexpected metric {unexpected}"),
        }
    }
    assert_eq!(
        seen.len(),
        11,
        "every counter/label combination and histogram must be present"
    );
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

fn next_addr() -> std::net::SocketAddr {
    // Ask the OS for a free ephemeral port, then release it so the service can
    // bind it. A shared in-process counter would hand out colliding ports once
    // tests run process-per-test (e.g. under nextest). Brief TOCTOU window, same
    // approach as the adapter suite's `free_port()`.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    addr
}
