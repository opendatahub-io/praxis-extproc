// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Prometheus metrics endpoint for the ExtProc server.
//!
//! Serves metrics in Prometheus text exposition format on a dedicated
//! HTTP port.

use std::{future::Future, net::SocketAddr, sync::OnceLock};

use http_body_util::Full;
use hyper::{Request, Response, body::Bytes};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing::{error, info};

// -----------------------------------------------------------------------------
// Metric Registration
// -----------------------------------------------------------------------------

/// Register all ExtProc metrics with the global recorder.
///
/// Call once at startup before any metrics are recorded.
pub fn register() {
    metrics::describe_counter!("praxis_extproc_requests_total", "Total ExtProc streams processed");
    metrics::describe_counter!(
        "praxis_extproc_immediate_responses_total",
        "Total ImmediateResponse rejections"
    );
    metrics::describe_histogram!(
        "praxis_extproc_request_duration_seconds",
        "Per-stream processing duration"
    );
}

/// Record a completed stream.
pub fn record_request(duration_secs: f64) {
    metrics::counter!("praxis_extproc_requests_total").increment(1);
    metrics::histogram!("praxis_extproc_request_duration_seconds").record(duration_secs);
}

/// Record an immediate response (rejection).
pub fn record_immediate_response() {
    metrics::counter!("praxis_extproc_immediate_responses_total").increment(1);
}

// -----------------------------------------------------------------------------
// Metrics Server
// -----------------------------------------------------------------------------

/// Start a Prometheus metrics HTTP server on the given address.
///
/// Installs a global `PrometheusRecorder` and serves the `/metrics`
/// endpoint. Blocks until the provided shutdown future completes.
///
/// # Errors
///
/// Returns an error if the recorder cannot be installed or the
/// server fails to bind.
pub async fn serve(addr: SocketAddr, shutdown: impl Future<Output = ()>) -> crate::error::Result<()> {
    let handle = install_recorder()?;

    register();

    info!(address = %addr, "metrics server listening");

    let listener = bind_listener(addr).await?;

    accept_loop(listener, handle, shutdown).await;

    Ok(())
}

/// Accept connections and serve Prometheus metrics until shutdown.
async fn accept_loop(listener: tokio::net::TcpListener, handle: PrometheusHandle, shutdown: impl Future<Output = ()>) {
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            result = listener.accept() => {
                let Ok((stream, _)) = result else {
                    continue;
                };
                serve_connection(stream, handle.clone());
            },
        }
    }
}

/// Spawn a task to serve a single metrics HTTP connection.
fn serve_connection(stream: tokio::net::TcpStream, handle: PrometheusHandle) {
    tokio::spawn(async move {
        let svc = hyper::service::service_fn(move |_req: Request<hyper::body::Incoming>| {
            let body = handle.render();
            async move { Ok::<_, std::convert::Infallible>(Response::new(Full::new(Bytes::from(body)))) }
        });

        if let Err(e) = hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(stream), svc)
            .await
        {
            error!(error = %e, "metrics connection error");
        }
    });
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Install the Prometheus recorder as the global metrics backend.
///
/// Safe to call multiple times; the recorder is installed on the
/// first call and subsequent calls return the existing handle.
fn install_recorder() -> crate::error::Result<PrometheusHandle> {
    static RESULT: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .map_err(|e| format!("metrics recorder: {e}"))
        })
        .clone()
        .map_err(crate::error::Error::Config)
}

/// Bind the TCP listener for the metrics endpoint.
async fn bind_listener(addr: SocketAddr) -> crate::error::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::Error::Config(format!("metrics bind: {e}")))
}
