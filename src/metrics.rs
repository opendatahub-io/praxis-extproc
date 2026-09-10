// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Prometheus metrics endpoint for the ExtProc server.
//!
//! Serves metrics in Prometheus text exposition format on a dedicated
//! HTTP port, optionally gated by Kubernetes `TokenReview` +
//! `SubjectAccessReview` authentication. Health probes at `/healthz`
//! are always unauthenticated.

use std::{future::Future, net::SocketAddr, sync::OnceLock};

use http_body_util::Full;
use hyper::{Request, Response, StatusCode, body::Bytes};
use hyper_util::rt::TokioIo;
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use tracing::{error, info, warn};

use crate::config::MetricsAuthConfig;

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
// Kubernetes Client Initialization
// -----------------------------------------------------------------------------

/// Create a Kubernetes API client for `TokenReview` / SAR calls.
///
/// Uses the in-cluster `ServiceAccount` credentials automatically
/// mounted by the kubelet at
/// `/var/run/secrets/kubernetes.io/serviceaccount/`.
///
/// Returns `None` when authentication is disabled.
///
/// # Errors
///
/// Returns [`ExtProcError::Config`] if the kube client cannot be
/// constructed (e.g. running outside a Kubernetes cluster with
/// auth enabled).
///
/// [`ExtProcError::Config`]: crate::error::ExtProcError::Config
async fn build_kube_client(config: &MetricsAuthConfig) -> crate::error::Result<Option<kube::Client>> {
    if !config.enabled {
        return Ok(None);
    }

    let client = kube::Client::try_default()
        .await
        .map_err(|e| crate::error::ExtProcError::Config(format!("metrics_auth kube client: {e}")))?;

    Ok(Some(client))
}

// -----------------------------------------------------------------------------
// TokenReview + SubjectAccessReview
// -----------------------------------------------------------------------------

/// Validate a bearer token via the Kubernetes `TokenReview` API.
///
/// Sends the raw bearer token to the API server, which verifies
/// the JWT signature and expiry. On success, returns the
/// authenticated username and group list.
///
/// # Errors
///
/// Returns an error if the API call fails (network, RBAC, etc.).
async fn authenticate_token(client: &kube::Client, token: &str) -> Result<TokenReviewResult, String> {
    use k8s_openapi::api::authentication::v1::TokenReview;

    let review = TokenReview {
        spec: k8s_openapi::api::authentication::v1::TokenReviewSpec {
            token: Some(token.to_owned()),
            ..Default::default()
        },
        ..Default::default()
    };

    let api: kube::Api<TokenReview> = kube::Api::all(client.clone());
    let result = api
        .create(&kube::api::PostParams::default(), &review)
        .await
        .map_err(|e| format!("TokenReview API call failed: {e}"))?;

    let status = result.status.ok_or("TokenReview response missing status")?;

    if !status.authenticated.unwrap_or(false) {
        return Err("token not authenticated".to_owned());
    }

    let user = status.user.unwrap_or_default();
    Ok(TokenReviewResult {
        username: user.username.unwrap_or_default(),
        groups: user.groups.unwrap_or_default(),
    })
}

/// Result of a successful `TokenReview`: the identity behind the token.
struct TokenReviewResult {
    /// Kubernetes username (e.g. `system:serviceaccount:ns:name`).
    username: String,
    /// Groups the user belongs to.
    groups: Vec<String>,
}

/// Check whether the authenticated user may `GET /metrics` via
/// the Kubernetes `SubjectAccessReview` API.
///
/// # Errors
///
/// Returns an error if the API call fails (network, RBAC, etc.).
async fn authorize_metrics_access(client: &kube::Client, username: &str, groups: &[String]) -> Result<bool, String> {
    use k8s_openapi::api::authorization::v1::{NonResourceAttributes, SubjectAccessReview, SubjectAccessReviewSpec};

    let sar = SubjectAccessReview {
        spec: SubjectAccessReviewSpec {
            user: Some(username.to_owned()),
            groups: Some(groups.to_vec()),
            non_resource_attributes: Some(NonResourceAttributes {
                path: Some("/metrics".to_owned()),
                verb: Some("get".to_owned()),
            }),
            ..Default::default()
        },
        ..Default::default()
    };

    let api: kube::Api<SubjectAccessReview> = kube::Api::all(client.clone());
    let result = api
        .create(&kube::api::PostParams::default(), &sar)
        .await
        .map_err(|e| format!("SubjectAccessReview API call failed: {e}"))?;

    Ok(result.status.is_some_and(|s| s.allowed))
}

// -----------------------------------------------------------------------------
// Metrics Server
// -----------------------------------------------------------------------------

/// Start a Prometheus metrics HTTP server on the given address.
///
/// Installs a global `PrometheusRecorder` and serves the `/metrics`
/// endpoint, optionally protected by Kubernetes `TokenReview` +
/// `SubjectAccessReview` authentication. Health probes at `/healthz`
/// are always unauthenticated.
///
/// Sends `()` on `ready` once the recorder is installed, the kube
/// client is constructed, and the TCP listener is bound. If any of
/// those steps fail, `ready` is dropped without sending, which lets
/// the caller detect the failure via `RecvError`.
///
/// Blocks until the provided shutdown future completes.
///
/// # Errors
///
/// Returns an error if the recorder cannot be installed, the
/// kube client cannot be created, or the server fails to bind.
pub async fn serve(
    addr: SocketAddr,
    auth_config: &MetricsAuthConfig,
    ready: tokio::sync::oneshot::Sender<()>,
    shutdown: impl Future<Output = ()>,
) -> crate::error::Result<()> {
    let handle = install_recorder()?;
    let kube_client = build_kube_client(auth_config).await?;

    register();

    let listener = bind_listener(addr).await?;

    info!(address = %addr, authenticated = kube_client.is_some(), "metrics server listening");

    let _sent = ready.send(());

    accept_loop(listener, handle, kube_client, shutdown).await;

    Ok(())
}

/// Accept connections and serve Prometheus metrics until shutdown.
async fn accept_loop(
    listener: tokio::net::TcpListener,
    handle: PrometheusHandle,
    kube_client: Option<kube::Client>,
    shutdown: impl Future<Output = ()>,
) {
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            () = &mut shutdown => break,
            result = listener.accept() => {
                let Ok((stream, _)) = result else {
                    continue;
                };
                serve_connection(stream, handle.clone(), kube_client.clone());
            },
        }
    }
}

/// Spawn a task to serve a single metrics HTTP connection.
fn serve_connection(stream: tokio::net::TcpStream, handle: PrometheusHandle, kube_client: Option<kube::Client>) {
    tokio::spawn(async move {
        let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
            let h = handle.clone();
            let kc = kube_client.clone();
            async move { Ok::<_, std::convert::Infallible>(route_request(&req, &h, kc.as_ref()).await) }
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
// Request Routing
// -----------------------------------------------------------------------------

/// Route an incoming HTTP request to the appropriate handler.
///
/// - `/healthz` — unauthenticated health probe (always 200).
/// - `/metrics` — Prometheus render, gated by `TokenReview` + SAR when configured.
/// - Any other path — 404 Not Found.
///
/// Generic over the body type because only the URI and headers are
/// inspected; this allows unit tests to use `Request<()>`.
async fn route_request<B: Sync>(
    req: &Request<B>,
    handle: &PrometheusHandle,
    kube_client: Option<&kube::Client>,
) -> Response<Full<Bytes>> {
    match req.uri().path() {
        "/healthz" => response_with_status(StatusCode::OK, "ok"),
        "/metrics" => serve_metrics(req, handle, kube_client).await,
        _ => response_with_status(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Serve the Prometheus metrics render, gated by optional
/// Kubernetes `TokenReview` + `SubjectAccessReview` authentication.
///
/// Generic over the body type because only the `Authorization`
/// header is inspected.
// TODO: consider a short-lived cache (e.g. `mini-moka`, TTL 30–60s) keyed by
// token hash to avoid a TokenReview + SAR round-trip on every scrape when
// multiple Prometheus replicas or short intervals are configured.
#[expect(clippy::large_stack_frames, reason = "async state machine for API calls")]
#[expect(clippy::cognitive_complexity, reason = "sequential authn/authz steps")]
async fn serve_metrics<B: Sync>(
    req: &Request<B>,
    handle: &PrometheusHandle,
    kube_client: Option<&kube::Client>,
) -> Response<Full<Bytes>> {
    let Some(client) = kube_client else {
        return Response::new(Full::new(Bytes::from(handle.render())));
    };

    let Some(bearer) = extract_bearer_token(req) else {
        warn!("metrics request rejected: missing or malformed Authorization header");
        return response_with_status(StatusCode::UNAUTHORIZED, "unauthorized");
    };

    let identity = match authenticate_token(client, bearer).await {
        Ok(id) => id,
        Err(e) => {
            warn!(error = %e, "metrics TokenReview failed");
            return response_with_status(StatusCode::UNAUTHORIZED, "unauthorized");
        },
    };

    match authorize_metrics_access(client, &identity.username, &identity.groups).await {
        Ok(true) => {},
        Ok(false) => {
            warn!(user = %identity.username, "metrics access denied by SubjectAccessReview");
            return response_with_status(StatusCode::FORBIDDEN, "forbidden");
        },
        Err(e) => {
            error!(error = %e, "metrics SubjectAccessReview failed");
            return response_with_status(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        },
    }

    Response::new(Full::new(Bytes::from(handle.render())))
}

/// Extract the bearer token from the `Authorization` header.
///
/// The `Bearer` scheme is matched case-insensitively per RFC 7235 §2.1.
/// Returns `None` if the header is missing, not valid UTF-8, or
/// does not use the `Bearer` scheme.
fn extract_bearer_token<B>(req: &Request<B>) -> Option<&str> {
    const PREFIX: &[u8] = b"Bearer ";
    let value = req.headers().get(hyper::header::AUTHORIZATION)?;
    let value_str = value.to_str().ok()?;
    value_str
        .as_bytes()
        .get(..PREFIX.len())
        .filter(|p| p.eq_ignore_ascii_case(PREFIX))
        .and_then(|_| value_str.get(PREFIX.len()..))
}

/// Build an HTTP response with the given status code and body.
fn response_with_status(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    let mut resp = Response::new(Full::new(Bytes::from(body.to_owned())));
    *resp.status_mut() = status;
    resp
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
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Bearer Token Extraction
    // -------------------------------------------------------------------------

    #[test]
    fn extract_bearer_token_valid() {
        let req = Request::builder()
            .header("Authorization", "Bearer secret123")
            .body(())
            .unwrap();

        assert_eq!(
            extract_bearer_token(&req),
            Some("secret123"),
            "valid bearer header should return the token"
        );
    }

    #[test]
    fn extract_bearer_token_missing_header() {
        let req = Request::builder().body(()).unwrap();

        assert_eq!(extract_bearer_token(&req), None, "missing header should return None");
    }

    #[test]
    fn extract_bearer_token_lowercase_scheme() {
        let req = Request::builder()
            .header("Authorization", "bearer secret123")
            .body(())
            .unwrap();

        assert_eq!(
            extract_bearer_token(&req),
            Some("secret123"),
            "lowercase 'bearer' should be accepted per RFC 7235"
        );
    }

    #[test]
    fn extract_bearer_token_uppercase_scheme() {
        let req = Request::builder()
            .header("Authorization", "BEARER secret123")
            .body(())
            .unwrap();

        assert_eq!(
            extract_bearer_token(&req),
            Some("secret123"),
            "uppercase 'BEARER' should be accepted per RFC 7235"
        );
    }

    #[test]
    fn extract_bearer_token_basic_auth() {
        let req = Request::builder()
            .header("Authorization", "Basic dXNlcjpwYXNz")
            .body(())
            .unwrap();

        assert_eq!(extract_bearer_token(&req), None, "Basic auth should return None");
    }

    // -------------------------------------------------------------------------
    // Request Routing (unauthenticated paths)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn route_unknown_path_returns_404() {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let req = Request::builder().uri("/foobar").body(()).unwrap();

        let resp = route_request(&req, &handle, None).await;

        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unknown path should return 404");
    }

    #[tokio::test]
    async fn route_metrics_no_auth_returns_200() {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let req = Request::builder().uri("/metrics").body(()).unwrap();

        let resp = route_request(&req, &handle, None).await;

        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "metrics without auth configured should return 200"
        );
    }

    #[tokio::test]
    async fn route_healthz_returns_200() {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let req = Request::builder().uri("/healthz").body(()).unwrap();

        let resp = route_request(&req, &handle, None).await;

        assert_eq!(resp.status(), StatusCode::OK, "healthz should always return 200");
    }
}
