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
    server::{KuadrantPolicy, PraxisExtProc},
    tls,
};
use praxis_proto::envoy::service::ext_proc::v3::external_processor_server::ExternalProcessorServer;
use tonic::transport::Server;
use tracing::{error, info};

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

    /// Optional path to a Kuadrant policy file (`{plugin, upstreams}`). When set,
    /// enables Authorino auth + Limitador rate limiting for every request.
    #[arg(long)]
    kuadrant_config: Option<String>,

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
    let cfg = load_config(&cli.config)?;
    let registry = praxis_ai_filters::build_ai_registry();
    let pipeline = config::build_pipeline(&cfg, &registry);
    let kuadrant = cli.kuadrant_config.as_deref().map(load_kuadrant).transpose()?;

    if cli.validate {
        pipeline?;
        info!("configuration is valid");
        return Ok(());
    }

    let addrs = resolve_addresses(&cli, &cfg)?;

    // The `fips` feature makes approved-mode a hard requirement at compile time.
    // Whether the host is in approved mode is probed at runtime.
    let fips = praxis_extproc::fips::assess();
    if !fips.serve_ok {
        return Box::pin(serve_unready(addrs, fips.active)).await;
    }

    let opts = ServeOptions {
        body_modes: (cfg.server.request_body_mode, cfg.server.response_body_mode),
        kuadrant,
    };
    Box::pin(serve_pipeline(addrs, pipeline, &cfg.server.tls, fips.active, opts)).await
}

/// Serving options threaded from config and CLI into the gRPC service.
struct ServeOptions {
    /// Config-pinned request/response body modes.
    body_modes: (Option<config::BodyModeOverride>, Option<config::BodyModeOverride>),
    /// Kuadrant policy, if native enforcement is enabled.
    kuadrant: Option<KuadrantPolicy>,
}

/// Serve the built pipeline, or a not-ready endpoint if it failed to build.
async fn serve_pipeline(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    pipeline: Result<std::sync::Arc<praxis_filter::FilterPipeline>, ExtProcError>,
    tls_cfg: &tls::TlsConfig,
    fips_active: bool,
    opts: ServeOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match pipeline {
        Ok(pipeline) => {
            info!(
                grpc = %addrs.0, health = %addrs.1,
                metrics = %addrs.2, filters = pipeline.len(),
                "starting ExtProc server"
            );
            Box::pin(start_services(addrs, pipeline, tls_cfg, fips_active, opts)).await
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
    tls_cfg: &tls::TlsConfig,
    fips_active: bool,
    opts: ServeOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Box::pin(run_with_sidecars(
        addrs,
        true,
        fips_active,
        serve_grpc(addrs.0, pipeline, tls_cfg, opts),
    ))
    .await
}

/// Serve only health (`NotServing`) and metrics when the pipeline failed to
/// build or the FIPS gate refused, keeping the process alive and inspectable
/// until shutdown.
async fn serve_unready(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    fips_active: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Box::pin(run_with_sidecars(addrs, false, fips_active, async {
        shutdown_signal().await;
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
/// Health is registered as serving per `serving`. All three futures are
/// supervised together: whichever completes first triggers shutdown of the
/// remaining tasks, and its result (including a sidecar's bind failure) is
/// returned as the originating error.
async fn run_with_sidecars(
    addrs: (std::net::SocketAddr, std::net::SocketAddr, std::net::SocketAddr),
    serving: bool,
    fips_active: bool,
    foreground: impl Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    let health_rx = shutdown_tx.subscribe();
    let mut health = tokio::spawn(async move {
        praxis_extproc::health::serve(addrs.1, serving, fips_active, wait_broadcast(health_rx)).await
    });

    let metrics_rx = shutdown_tx.subscribe();
    let mut metrics =
        tokio::spawn(async move { praxis_extproc::metrics::serve(addrs.2, wait_broadcast(metrics_rx)).await });

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
    tls_cfg: &tls::TlsConfig,
    opts: ServeOptions,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ext = PraxisExtProc::new(pipeline).with_body_modes(opts.body_modes.0, opts.body_modes.1);
    if let Some(policy) = opts.kuadrant {
        ext = ext.with_kuadrant(policy);
    }
    let svc = ExternalProcessorServer::new(ext);
    match tls::build_tls_config(tls_cfg)? {
        None => Box::pin(serve_plaintext(addr, svc)).await,
        Some(acceptor) => Box::pin(serve_tls(addr, svc, acceptor, tls_cfg)).await,
    }
}

/// Serve gRPC over plaintext TCP.
async fn serve_plaintext(
    addr: std::net::SocketAddr,
    svc: ExternalProcessorServer<PraxisExtProc>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Box::pin(
        Server::builder()
            .add_service(svc)
            .serve_with_shutdown(addr, shutdown_signal()),
    )
    .await?;
    Ok(())
}

/// Serve gRPC over TLS using the provided acceptor.
async fn serve_tls(
    addr: std::net::SocketAddr,
    svc: ExternalProcessorServer<PraxisExtProc>,
    acceptor: openssl::ssl::SslAcceptor,
    tls_cfg: &tls::TlsConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let timeout = std::time::Duration::from_secs(tls_cfg.handshake_timeout_secs);
    let incoming = tls::build_tls_incoming(listener, acceptor, tls_cfg.handshake_concurrency, timeout);
    Box::pin(
        Server::builder()
            .add_service(svc)
            .serve_with_incoming_shutdown(incoming, shutdown_signal()),
    )
    .await?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Shutdown
// -----------------------------------------------------------------------------

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

/// Kuadrant policy file: the `kuadrant-filter` plugin configuration plus the
/// cluster-name -> gRPC endpoint map the transport dials.
#[derive(serde::Deserialize)]
struct KuadrantFile {
    /// The `kuadrant-filter` plugin configuration (services + action sets).
    plugin: kuadrant_filter::configuration::PluginConfiguration,
    /// Cluster-name -> gRPC endpoint map the transport dials.
    #[serde(default)]
    upstreams: std::collections::HashMap<String, String>,
    /// Per-upstream TLS keyed by cluster name; absent means plaintext.
    /// Authorino serves TLS (signed by the cluster service CA); Limitador does not.
    #[serde(default)]
    tls: std::collections::HashMap<String, UpstreamTlsFile>,
}

/// TLS settings for one upstream, as written in the Kuadrant policy file.
#[derive(serde::Deserialize)]
struct UpstreamTlsFile {
    /// Path to a PEM-encoded CA certificate to trust for this upstream.
    ca_cert: String,
    /// Server name to send as SNI and verify the certificate against.
    sni: String,
}

/// Load and parse a Kuadrant policy file into a [`KuadrantPolicy`].
///
/// # Errors
///
/// Returns [`ExtProcError::Config`] if the file or any referenced CA cannot be
/// read, or the YAML fails to parse.
fn load_kuadrant(path: &str) -> Result<KuadrantPolicy, ExtProcError> {
    let content = std::fs::read_to_string(path).map_err(|e| ExtProcError::Config(format!("{path}: {e}")))?;
    let file: KuadrantFile = serde_yaml::from_str(&content).map_err(|e| ExtProcError::Config(e.to_string()))?;
    let mut tls = std::collections::HashMap::new();
    for (name, entry) in file.tls {
        let ca_pem =
            std::fs::read(&entry.ca_cert).map_err(|e| ExtProcError::Config(format!("{}: {e}", entry.ca_cert)))?;
        tls.insert(
            name,
            praxis_extproc::kuadrant_transport::UpstreamTls { ca_pem, sni: entry.sni },
        );
    }
    Ok(KuadrantPolicy::new(file.plugin, file.upstreams, tls))
}

/// Parse a socket address from CLI override or config default.
fn parse_addr(
    cli_override: &Option<String>,
    config_default: &str,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error + Send + Sync>> {
    let s = cli_override.as_deref().unwrap_or(config_default);
    Ok(s.parse()?)
}
