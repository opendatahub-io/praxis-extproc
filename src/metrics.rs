// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

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
    metrics::describe_counter!(
        "praxis_extproc_body_size_rejections_total",
        "Total requests rejected for exceeding max body accumulation"
    );
    metrics::describe_counter!(
        "praxis_extproc_invalid_argument_total",
        "Total invalid_argument rejections, by reason and detail"
    );
    metrics::describe_counter!(
        "praxis_extproc_local_replies_total",
        "Total streams that opened with a local reply from an earlier filter, by status class"
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

/// Record a rejection for exceeding max body accumulation.
pub fn record_body_size_rejection() {
    metrics::counter!("praxis_extproc_body_size_rejections_total").increment(1);
}

/// Record a stream that opened with a local reply from an earlier filter,
/// labelled by the class of the reply's `:status`.
///
/// Such a stream bypasses the filter pipeline. Auth and quota rejections land
/// in `4xx`; a steady `2xx` count points at a route that skips request headers,
/// so an upstream response went out unfiltered.
pub fn record_local_reply(status: Option<u16>) {
    metrics::counter!("praxis_extproc_local_replies_total", "status_class" => status_class(status)).increment(1);
}

/// Bounded class of an HTTP status: `2xx` to `5xx`, or `other` for anything
/// else, including a missing or unparsable `:status`.
const fn status_class(status: Option<u16>) -> &'static str {
    match status {
        Some(200..=299) => "2xx",
        Some(300..=399) => "3xx",
        Some(400..=499) => "4xx",
        Some(500..=599) => "5xx",
        _ => "other",
    }
}

/// Record an `invalid_argument` rejection under bounded `reason` and `detail` labels.
pub fn record_invalid_argument(reason: &'static str, detail: &'static str) {
    metrics::counter!(
        "praxis_extproc_invalid_argument_total",
        "reason" => reason,
        "detail" => detail,
    )
    .increment(1);
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

    let listener = bind_listener(addr).await?;

    info!(address = %addr, "metrics server listening");

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
        .map_err(crate::error::ExtProcError::Config)
}

/// Bind the TCP listener for the metrics endpoint.
async fn bind_listener(addr: SocketAddr) -> crate::error::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::ExtProcError::Config(format!("metrics bind: {e}")))
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_class_is_bounded() {
        for (status, class) in [
            (Some(200), "2xx"),
            (Some(302), "3xx"),
            (Some(401), "4xx"),
            (Some(429), "4xx"),
            (Some(503), "5xx"),
            (Some(101), "other"),
            (Some(600), "other"),
            (None, "other"),
        ] {
            assert_eq!(status_class(status), class, "status {status:?}");
        }
    }
}
