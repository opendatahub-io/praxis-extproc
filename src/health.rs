// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! gRPC health check service for the ExtProc server.
//!
//! Runs on a separate port so Envoy and Kubernetes can probe
//! readiness without going through the ExtProc protocol.

use std::future::Future;

use tracing::info;

// -----------------------------------------------------------------------------
// Health Service
// -----------------------------------------------------------------------------

/// Start a gRPC health check server on the given address.
///
/// Registers the `ExternalProcessor` service as `SERVING` and blocks
/// until the provided shutdown future completes.
///
/// # Errors
///
/// Returns a transport error if the server fails to bind or serve.
pub async fn serve(
    addr: std::net::SocketAddr,
    shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    let (reporter, svc) = tonic_health::server::health_reporter();

    reporter
        .set_serving::<praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer<
            crate::server::PraxisExtProc,
        >>()
        .await;

    info!(address = %addr, "health server listening");

    tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_shutdown(addr, shutdown)
        .await
}
