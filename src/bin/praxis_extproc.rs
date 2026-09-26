// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

#![deny(unsafe_code)]
#![deny(unreachable_pub)]

//! Binary entry point for the Praxis ExtProc server.

use std::{future::Future, process};

use clap::Parser;
use praxis_extproc::{
    config::{self, ExtProcConfig},
    error::ExtProcError,
    server::PraxisExtProc,
    tls,
};
use praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer;
use tonic::transport::Server;
use tracing::{error, info, warn};

// -----------------------------------------------------------------------------
// CLI
// -----------------------------------------------------------------------------

/// Praxis ExtProc server: run Praxis filter pipelines as an Envoy
/// external processor.
#[derive(Debug, Parser)]
#[command(name = "praxis-extproc", version, about)]
struct Cli {
    /// Path to the YAML configuration file.
    #[arg(short, long, default_value = "praxis-extproc.yaml")]
    config: String,

    /// Override the gRPC listen address.
    #[arg(long)]
    grpc_address: Option<String>,

    /// Override the health check listen address.
    #[arg(long)]
    health_address: Option<String>,

    /// Override the metrics listen address.
    #[arg(long)]
    metrics_address: Option<String>,

    /// Validate configuration and exit.
    #[arg(short = 't', long)]
    validate: bool,
}

// -----------------------------------------------------------------------------
// Main
// -----------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    init_tracing();
    let cli = Cli::parse();

    if let Err(e) = Box::pin(run(cli)).await {
        error!(error = %e, "fatal");
        process::exit(1);
    }
}

// -----------------------------------------------------------------------------
// Startup
// -----------------------------------------------------------------------------

/// Top-level application logic.
async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Before anything that builds a filter registry, a subrequest client or a
    // TLS configuration: the provider installed here is the only one there is.
    let fips = praxis_extproc::fips::install()?;

    let cfg = load_config(&cli.config)?;
    let registry = praxis_ai_filters::build_ai_registry();
    let pipeline = config::build_pipeline(&cfg, &registry);

    if cli.validate {
        if let Some(reason) = fips_refusal(&fips, &registry) {
            return Err(reason.into());
        }
        pipeline?;
        info!("configuration is valid");
        return Ok(());
    }

    let addrs = resolve_addresses(&cli, &cfg)?;

    // PRAXIS_REQUIRE_FIPS makes FIPS mode a hard requirement. The host decides
    // whether it is in effect; when it is not, the process stays up and
    // inspectable (health NotServing) but serves no traffic.
    if let Some(reason) = fips_refusal(&fips, &registry) {
        error!(error = %reason, "refusing to serve");
        return Box::pin(serve_unready(addrs, praxis_extproc::fips::active(&fips))).await;
    }

    Box::pin(serve_pipeline(
        addrs,
        pipeline,
        &cfg.server,
        cfg.max_body_accumulation(),
        praxis_extproc::fips::active(&fips),
    ))
    .await
}

/// Why this process must not serve under `PRAXIS_REQUIRE_FIPS`, if it must
/// not: FIPS mode is not in effect on the host, or the binary registers
/// filters whose dependencies do their own cryptography (the same refusal,
/// by contents rather than by host).
fn fips_refusal(fips: &praxis_extproc::fips::Status, registry: &praxis_filter::FilterRegistry) -> Option<String> {
    if let Err(e) = praxis_extproc::fips::require(fips) {
        return Some(e.to_string());
    }
    if praxis_extproc::fips::required() {
        return praxis_extproc::fips::blocker(registry);
    }
    None
}

/// Serve the built pipeline, or a not-ready endpoint if it failed to build.
async fn serve_pipeline(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    pipeline: Result<std::sync::Arc<praxis_filter::FilterPipeline>, ExtProcError>,
    server_cfg: &config::ServerConfig,
    max_body: Option<usize>,
    fips_active: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match pipeline {
        Ok(pipeline) => {
            info!(
                grpc = %addrs.0, health = %addrs.1,
                metrics = %addrs.2, filters = pipeline.len(),
                "starting ExtProc server"
            );
            Box::pin(start_services(addrs, pipeline, server_cfg, max_body, fips_active)).await
        },
        Err(e) => {
            error!(error = %e, health = %addrs.1, "filter pipeline build failed; reporting NotServing");
            Box::pin(serve_unready(addrs, fips_active)).await
        },
    }
}

/// Start gRPC, health (`Serving`), and metrics servers concurrently.
async fn start_services(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    pipeline: std::sync::Arc<praxis_filter::FilterPipeline>,
    server_cfg: &config::ServerConfig,
    max_body: Option<usize>,
    fips_active: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Box::pin(run_with_sidecars(addrs, true, fips_active, move |drain_rx| {
        serve_grpc(addrs.0, pipeline, server_cfg, max_body, drain_rx)
    }))
    .await
}

/// Serve only health (`NotServing`) and metrics when the pipeline failed to
/// build or FIPS mode is required and not in effect, keeping the process
/// alive and inspectable until shutdown.
async fn serve_unready(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    fips_active: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Box::pin(run_with_sidecars(addrs, false, fips_active, |drain_rx| async move {
        wait_drain(drain_rx).await;
        Ok(())
    }))
    .await
}

/// Which of the supervised futures completed first in [`run_with_sidecars`].
#[derive(PartialEq)]
enum Selected {
    /// The foreground future (gRPC serving or the shutdown wait).
    Foreground,
    /// The health check sidecar.
    Health,
    /// The metrics sidecar.
    Metrics,
}

/// Run the health and metrics sidecars alongside a foreground future.
///
/// A single shutdown-signal listener drives a shared drain latch: `foreground`
/// receives its [`watch::Receiver`] to start its own drain, and the health
/// sidecar uses it to flip readiness to `NotServing` the moment the signal
/// fires. All three futures are supervised together: whichever completes first
/// triggers shutdown of the remaining tasks, and its result (including a
/// sidecar's bind failure) is returned as the originating error.
///
/// [`watch::Receiver`]: tokio::sync::watch::Receiver
async fn run_with_sidecars<F, Fut>(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    serving: bool,
    fips_active: bool,
    foreground: F,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    F: FnOnce(tokio::sync::watch::Receiver<bool>) -> Fut,
    Fut: Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send,
{
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let drain_rx = spawn_drain_signal();

    let health_rx = shutdown_tx.subscribe();
    let health_drain = wait_drain(drain_rx.clone());
    let mut health = tokio::spawn(async move {
        praxis_extproc::health::serve(addrs.1, serving, fips_active, health_drain, wait_broadcast(health_rx)).await
    });

    let metrics_rx = shutdown_tx.subscribe();
    let mut metrics =
        tokio::spawn(async move { praxis_extproc::metrics::serve(addrs.2, wait_broadcast(metrics_rx)).await });

    let foreground = foreground(drain_rx);
    tokio::pin!(foreground);

    let (outcome, selected) = tokio::select! {
        r = &mut foreground => (r, Selected::Foreground),
        r = &mut health => (task_outcome(r), Selected::Health),
        r = &mut metrics => (task_outcome(r), Selected::Metrics),
    };

    drop(shutdown_tx); // signal the remaining sidecars to stop

    // Await and log every sidecar except the one already consumed by the select.
    if selected != Selected::Health {
        drain("health", health).await;
    }
    if selected != Selected::Metrics {
        drain("metrics", metrics).await;
    }

    if outcome.is_ok() {
        info!("server shut down");
    }
    outcome
}

/// Collapse a task's join result and its inner service result into one error.
fn task_outcome<E>(
    joined: Result<Result<(), E>, tokio::task::JoinError>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    E: std::error::Error + Send + Sync + 'static,
{
    match joined {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(Box::new(e)), // service error (e.g. failed to bind)
        Err(e) => Err(Box::new(e)),     // join error (panic / cancel)
    }
}

/// Await a still-running sidecar during shutdown, logging either error layer.
async fn drain<E>(name: &str, handle: tokio::task::JoinHandle<Result<(), E>>)
where
    E: std::error::Error + Send + Sync + 'static,
{
    if let Err(e) = task_outcome(handle.await) {
        error!(error = %e, "{name} server stopped with error");
    }
}

/// Start the main gRPC ExtProc server.
async fn serve_grpc(
    addr: std::net::SocketAddr,
    pipeline: std::sync::Arc<praxis_filter::FilterPipeline>,
    server_cfg: &config::ServerConfig,
    max_body: Option<usize>,
    drain_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // The latch fires when the drain deadline expires, forcing any streams still
    // running after graceful shutdown began to cancel.
    let (force_tx, force_rx) = tokio::sync::watch::channel(false);
    let svc = ExternalProcessorServer::new(
        PraxisExtProc::new(pipeline)
            .with_max_body_accumulation(max_body)
            .with_force_shutdown(force_rx.clone()),
    );
    let drain = std::time::Duration::from_secs(server_cfg.shutdown_drain_timeout_secs.get());
    let controls = ShutdownControls {
        signal: Box::pin(shutdown_with_deadline(drain_rx, force_tx, drain)),
        force_rx,
    };

    match tls::build_tls_config(&server_cfg.tls)? {
        None => Box::pin(serve_plaintext(addr, svc, controls)).await,
        Some(acceptor) => Box::pin(serve_tls(addr, svc, acceptor, &server_cfg.tls, controls)).await,
    }
}

/// Shutdown wiring shared by the plaintext and TLS serving paths.
struct ShutdownControls {
    /// Resolves when graceful shutdown should begin, starting tonic's drain.
    ///
    /// tonic's `serve_with_shutdown` closes the listener immediately here — it
    /// has no in-process lameduck that keeps accepting new connections during a
    /// grace window (see grpc-rust#1940). Deployments therefore rely on a k8s
    /// preStop lameduck to stop routing before SIGTERM; see `deploy/` and the
    /// "Kubernetes deployment" section of `docs/configuration.md`.
    signal: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>,
    /// Force-close latch; flips when the drain deadline expires.
    force_rx: tokio::sync::watch::Receiver<bool>,
}

/// Serve gRPC over plaintext TCP.
async fn serve_plaintext(
    addr: std::net::SocketAddr,
    svc: ExternalProcessorServer<PraxisExtProc>,
    controls: ShutdownControls,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let serve = Box::pin(
        Server::builder()
            .add_service(svc)
            .serve_with_shutdown(addr, controls.signal),
    );
    serve_bounded(serve, controls.force_rx).await
}

/// Serve gRPC over TLS using the provided acceptor.
async fn serve_tls(
    addr: std::net::SocketAddr,
    svc: ExternalProcessorServer<PraxisExtProc>,
    acceptor: openssl::ssl::SslAcceptor,
    tls_cfg: &tls::TlsConfig,
    controls: ShutdownControls,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let timeout = std::time::Duration::from_secs(tls_cfg.handshake_timeout_secs);
    let incoming = tls::build_tls_incoming(listener, acceptor, tls_cfg.handshake_concurrency, timeout);
    let serve = Box::pin(
        Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(incoming, controls.signal),
    );
    serve_bounded(serve, controls.force_rx).await
}

/// Await a tonic serving future, but abandon it once the drain deadline latch
/// fires.
///
/// tonic's graceful drain can hang when a client stops reading and fills a
/// stream's response channel: the accepted connection never becomes idle.
/// Dropping the serving future when the latch flips force-closes any such
/// connections, so completion stays bounded by the configured drain timeout.
///
/// # Errors
///
/// Returns the serving future's transport error if it fails before the latch
/// fires.
async fn serve_bounded(
    serve: impl Future<Output = Result<(), tonic::transport::Error>> + Send,
    mut force_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::select! {
        res = serve => res?,
        Ok(_) = force_rx.wait_for(|forced| *forced) => {
            warn!("drain deadline expired; force-closing remaining connections");
        },
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Shutdown
// -----------------------------------------------------------------------------

/// Wait for the shared drain signal, then arm the drain deadline.
///
/// Returning starts tonic's graceful drain; a detached timer force-cancels any
/// streams still running once `drain` elapses by flipping the shared latch.
async fn shutdown_with_deadline(
    drain_rx: tokio::sync::watch::Receiver<bool>,
    force_tx: tokio::sync::watch::Sender<bool>,
    drain: std::time::Duration,
) {
    wait_drain(drain_rx).await;
    tokio::spawn(async move {
        tokio::time::sleep(drain).await;
        warn!(
            timeout_secs = drain.as_secs(),
            "graceful drain deadline exceeded; forcing stream cancellation"
        );
        if force_tx.send(true).is_err() {
            info!("drain deadline expired but no streams remained to cancel");
        }
    });
}

/// Spawn the single SIGTERM/SIGINT listener, returning a latch that flips to
/// `true` when graceful shutdown should begin.
///
/// Both the gRPC serving path and the health sidecar observe this one receiver,
/// so shutdown has a single signal source and a single log line.
fn spawn_drain_signal() -> tokio::sync::watch::Receiver<bool> {
    let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        if drain_tx.send(true).is_err() {
            info!("shutdown signal fired but no drain receivers remained");
        }
    });
    drain_rx
}

/// Wait until the shared drain latch flips to `true`.
async fn wait_drain(mut drain_rx: tokio::sync::watch::Receiver<bool>) {
    drop(drain_rx.wait_for(|started| *started).await);
}

/// Wait for SIGTERM or SIGINT for graceful shutdown.
#[expect(
    clippy::cognitive_complexity,
    reason = "platform-specific signal select is intentionally inline"
)]
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let Ok(mut sigterm) = signal(SignalKind::terminate()) else {
            error!("failed to install SIGTERM handler");
            return;
        };

        tokio::select! {
            _ = ctrl_c => info!("received SIGINT"),
            _ = sigterm.recv() => info!("received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        if ctrl_c.await.is_err() {
            error!("ctrl-c handler failed");
        } else {
            info!("received SIGINT");
        }
    }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Initialize the tracing subscriber with env-filter support.
fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// Resolve all three listen addresses from CLI overrides or config.
fn resolve_addresses(
    cli: &Cli,
    cfg: &ExtProcConfig,
) -> Result<(std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr), Box<dyn std::error::Error + Send + Sync>>
{
    let grpc = parse_addr(&cli.grpc_address, &cfg.server.grpc_address)?;
    let health = parse_addr(&cli.health_address, &cfg.server.health_address)?;
    let metrics = parse_addr(&cli.metrics_address, &cfg.server.metrics_address)?;
    Ok((grpc, health, metrics))
}

/// Wait for a broadcast shutdown signal.
async fn wait_broadcast(mut rx: tokio::sync::broadcast::Receiver<()>) {
    drop(rx.recv().await);
}

/// Load and parse the YAML configuration file.
fn load_config(path: &str) -> Result<ExtProcConfig, ExtProcError> {
    let content = std::fs::read_to_string(path).map_err(|e| ExtProcError::Config(format!("{path}: {e}")))?;

    serde_yaml::from_str(&content).map_err(|e| ExtProcError::Config(e.to_string()))
}

/// Parse a socket address from CLI override or config default.
fn parse_addr(
    cli_override: &Option<String>,
    config_default: &str,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error + Send + Sync>> {
    let s = cli_override.as_deref().unwrap_or(config_default);
    Ok(s.parse()?)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A hung serving future (graceful drain that never completes, e.g. a
    /// non-reading client holding a connection open) must still be abandoned
    /// once the drain deadline latch fires, bounding shutdown. Both the
    /// plaintext and TLS paths delegate to `serve_bounded`, so this covers both.
    #[tokio::test]
    async fn serve_bounded_returns_when_latch_fires() {
        let (force_tx, force_rx) = tokio::sync::watch::channel(false);
        let serve = std::future::pending::<Result<(), tonic::transport::Error>>();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            force_tx.send(true).unwrap();
        });

        tokio::time::timeout(Duration::from_secs(1), serve_bounded(serve, force_rx))
            .await
            .expect("serve_bounded must return once the latch fires")
            .expect("bounded shutdown is not an error");
    }

    /// Normal shutdown: the serving future completes on its own before the
    /// latch ever fires.
    #[tokio::test]
    async fn serve_bounded_returns_when_serve_completes() {
        let (_force_tx, force_rx) = tokio::sync::watch::channel(false);
        let serve = std::future::ready(Ok::<(), tonic::transport::Error>(()));

        serve_bounded(serve, force_rx)
            .await
            .expect("normal completion is not an error");
    }
}
