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
/// true, otherwise `NotServing`, and reports the FIPS state under
/// [`FIPS_SERVICE`]. When `on_drain` resolves (the graceful-shutdown signal),
/// readiness flips to `NotServing` so Kubernetes and Envoy stop routing before
/// the gRPC drain begins, while the server keeps answering until `shutdown`
/// completes.
///
/// # Errors
///
/// Returns a transport error if the server fails to bind or serve.
pub async fn serve(
    addr: std::net::SocketAddr,
    serving: bool,
    fips_active: bool,
    on_drain: impl Future<Output = ()>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), tonic::transport::Error> {
    let (reporter, svc) = tonic_health::server::health_reporter();
    set_initial_status(&reporter, serving, fips_active).await;

    info!(address = %addr, serving, fips = fips_active, "health server listening");

    let server = tonic::transport::Server::builder()
        .add_service(svc)
        .serve_with_shutdown(addr, shutdown);

    tokio::select! {
        res = server => res,
        () = report_not_serving_on_drain(reporter, on_drain) => Ok(()),
    }
}

/// Register the initial serving status for the ExtProc and FIPS services.
async fn set_initial_status(reporter: &tonic_health::server::HealthReporter, serving: bool, fips_active: bool) {
    use tonic_health::ServingStatus;

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
}

/// Flip ExtProc readiness to `NotServing` once the drain signal fires, then hold
/// that status indefinitely so the server keeps reporting it until `serve`'s own
/// shutdown future stops the health server.
async fn report_not_serving_on_drain(
    reporter: tonic_health::server::HealthReporter,
    on_drain: impl Future<Output = ()>,
) {
    on_drain.await;
    reporter.set_not_serving::<ExtProcServer>().await;
    info!("shutdown signalled; health reporting NotServing during drain");
    std::future::pending::<()>().await;
}
