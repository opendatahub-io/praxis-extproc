// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! gRPC health check service for the ExtProc server.
//!
//! Runs on a separate port so Envoy and Kubernetes can probe
//! readiness without going through the ExtProc protocol.

use std::future::Future;

use tracing::info;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Health service name carrying the FIPS approved-mode state.
///
/// Reported as `Serving` when FIPS is active, `NotServing` otherwise, so a probe
/// can distinguish a FIPS gate failure from pipeline readiness.
pub const FIPS_SERVICE: &str = "fips";

// -----------------------------------------------------------------------------
// Health Service
// -----------------------------------------------------------------------------

/// The `ExternalProcessor` gRPC server type whose serving status is reported.
type ExtProcServer = praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer<
    crate::server::PraxisExtProc,
>;

/// Start a gRPC health check server on the given address.
///
/// Registers the `ExternalProcessor` service as `Serving` when `serving` is
/// true, otherwise `NotServing`, reports the FIPS state under [`FIPS_SERVICE`],
/// and blocks until the provided shutdown future completes.
///
/// # Errors
///
/// Returns a transport error if the server fails to bind or serve.
pub async fn serve(
    addr: std::net::SocketAddr,
    serving: bool,
    fips_active: bool,
    shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    use tonic_health::ServingStatus;

    let (reporter, svc) = tonic_health::server::health_reporter();

    if serving {
        reporter.set_serving::<ExtProcServer>().await;
    } else {
        reporter.set_not_serving::<ExtProcServer>().await;
    }

    let fips_status = if fips_active {
        ServingStatus::Serving
    } else {
        ServingStatus::NotServing
    };
    reporter.set_service_status(FIPS_SERVICE, fips_status).await;

    info!(address = %addr, serving, fips = fips_active, "health server listening");

    tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_shutdown(addr, shutdown)
        .await
}
