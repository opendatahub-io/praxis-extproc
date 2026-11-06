// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Prometheus metrics endpoint for the ExtProc server.
//!
//! Serves metrics in Prometheus text exposition format on a dedicated
//! HTTP port, optionally gated by Kubernetes `TokenReview` +
//! `SubjectAccessReview` authentication. Health probes at `/healthz`
//! are always unauthenticated.

use std::{future::Future, net::SocketAddr, sync::OnceLock, time::Duration};

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

/// Outcome of a `TokenReview` authentication attempt.
enum AuthnOutcome {
    /// Token is valid; contains the authenticated identity.
    Authenticated(TokenReviewResult),
    /// Token is invalid or expired (authentication denied by the API server).
    NotAuthenticated,
    /// The Kubernetes API call itself failed (network error, missing RBAC,
    /// malformed response, or timeout). The caller should return 500, not 401,
    /// because the server cannot determine whether the token is valid.
    ApiError(String),
}

/// Bound for one `TokenReview` or `SubjectAccessReview` round trip.
///
/// kube 5 leaves [`kube::Config::read_timeout`] unset (connect is 30s, write
/// is 295s). A peer that accepts and stays silent would otherwise keep the
/// scrape task alive until the HTTP client disconnects.
const KUBERNETES_API_TIMEOUT: Duration = Duration::from_secs(10);

/// Post one review object and fail when the API server stays silent.
///
/// [`kube::Client::try_default`] does not set a read timeout, so this bound
/// is what stops a stalled control plane from pinning the scrape task.
async fn kubernetes_create<K>(api: &kube::Api<K>, object: &K, call: &str) -> Result<K, String>
where
    K: Clone + std::fmt::Debug + serde::Serialize + serde::de::DeserializeOwned + Sync,
{
    let created = tokio::time::timeout(
        KUBERNETES_API_TIMEOUT,
        api.create(&kube::api::PostParams::default(), object),
    )
    .await;
    match created {
        Ok(Ok(created)) => Ok(created),
        Ok(Err(error)) => Err(format!("{call} API call failed: {error}")),
        Err(_elapsed) => Err(format!(
            "{call} API call timed out after {}s",
            KUBERNETES_API_TIMEOUT.as_secs()
        )),
    }
}

/// Validate a bearer token via the Kubernetes `TokenReview` API.
///
/// Sends the raw bearer token to the API server, which verifies
/// the JWT signature and expiry. Returns [`AuthnOutcome::Authenticated`]
/// with the identity on success, [`AuthnOutcome::NotAuthenticated`] when
/// the token is invalid or expired, and [`AuthnOutcome::ApiError`] when
/// the API call itself fails (network, RBAC misconfiguration, malformed
/// response, or no response within [`KUBERNETES_API_TIMEOUT`]) — callers
/// must return 500 for that case, not 401.
async fn authenticate_token(client: &kube::Client, token: &str) -> AuthnOutcome {
    use k8s_openapi::api::authentication::v1::TokenReview;

    let review = TokenReview {
        spec: k8s_openapi::api::authentication::v1::TokenReviewSpec {
            token: token.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    };

    let api: kube::Api<TokenReview> = kube::Api::all(client.clone());
    let result = match kubernetes_create(&api, &review, "TokenReview").await {
        Ok(created) => created,
        Err(error) => return AuthnOutcome::ApiError(error),
    };

    let Some(status) = result.status else {
        return AuthnOutcome::ApiError("TokenReview response missing status".to_owned());
    };

    match status.authenticated {
        Some(true) => {
            let user = status.user.unwrap_or_default();
            AuthnOutcome::Authenticated(TokenReviewResult {
                username: user.username.unwrap_or_default(),
                groups: user.groups.unwrap_or_default(),
            })
        },
        Some(false) => AuthnOutcome::NotAuthenticated,
        None => AuthnOutcome::ApiError("TokenReview response missing authenticated".to_owned()),
    }
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
/// Returns an error if the API call fails (network, RBAC, timeout, etc.).
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
    let result = kubernetes_create(&api, &sar, "SubjectAccessReview").await?;

    let status = result.status.ok_or("SubjectAccessReview response missing status")?;
    Ok(status.allowed)
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
                match result {
                    Ok((stream, _)) => serve_connection(stream, handle.clone(), kube_client.clone()),
                    Err(e) => warn!(error = %e, "metrics accept error"),
                }
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
/// - `/metrics` — `GET` only; Prometheus render, gated by `TokenReview` + SAR when configured.
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
        "/metrics" if req.method() == hyper::Method::GET => serve_metrics(req, handle, kube_client).await,
        "/metrics" => response_with_status(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
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
#[expect(
    clippy::too_many_lines,
    reason = "sequential authn/authz/render steps; extraction would split tightly coupled logic"
)]
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
        AuthnOutcome::Authenticated(id) => id,
        AuthnOutcome::NotAuthenticated => {
            warn!("metrics token not authenticated");
            return response_with_status(StatusCode::UNAUTHORIZED, "unauthorized");
        },
        AuthnOutcome::ApiError(e) => {
            error!(error = %e, "metrics TokenReview API call failed");
            return response_with_status(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        },
    };

    match authorize_metrics_access(client, &identity.username, &identity.groups).await {
        Ok(true) => {},
        Ok(false) => {
            warn_denied(&identity.username);
            return response_with_status(StatusCode::FORBIDDEN, "forbidden");
        },
        Err(e) => {
            error!(error = %e, "metrics SubjectAccessReview failed");
            return response_with_status(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        },
    }

    Response::new(Full::new(Bytes::from(handle.render())))
}

/// Log a denial without writing a human `TokenReview` username.
///
/// Service accounts stay identifiable as `namespace:name`. Any other subject
/// is omitted; log aggregation for this process is not a place for a person's
/// name.
fn warn_denied(username: &str) {
    if let Some(service_account) = service_account_subject(username) {
        warn!(service_account = %service_account, "metrics access denied by SubjectAccessReview");
    } else {
        warn!("metrics access denied by SubjectAccessReview");
    }
}

/// `namespace:name` when `username` is a service account, otherwise nothing.
fn service_account_subject(username: &str) -> Option<&str> {
    username
        .strip_prefix("system:serviceaccount:")
        .filter(|account| !account.is_empty())
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

    #[test]
    fn denial_log_keeps_service_accounts_and_drops_users() {
        assert_eq!(
            service_account_subject("system:serviceaccount:redhat-ods-monitoring:prometheus-k8s"),
            Some("redhat-ods-monitoring:prometheus-k8s"),
            "a service account is safe to log"
        );
        assert_eq!(service_account_subject("alice"), None, "a user name is not logged");
        assert_eq!(
            service_account_subject("system:admin"),
            None,
            "a system user is not a service account"
        );
        assert_eq!(
            service_account_subject("system:serviceaccount:"),
            None,
            "an empty account is dropped"
        );
        assert_eq!(service_account_subject(""), None, "a missing username is dropped");
    }

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
    async fn route_metrics_non_get_returns_405_before_auth() {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let req = Request::builder()
            .method("POST")
            .uri("/metrics")
            .header("Authorization", "Bearer secret")
            .body(())
            .unwrap();

        let resp = route_request(&req, &handle, None).await;

        assert_eq!(
            resp.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "non-GET /metrics should return 405 without checking auth"
        );
    }

    #[tokio::test]
    async fn route_healthz_returns_200() {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let req = Request::builder().uri("/healthz").body(()).unwrap();

        let resp = route_request(&req, &handle, None).await;

        assert_eq!(resp.status(), StatusCode::OK, "healthz should always return 200");
    }

    // -------------------------------------------------------------------------
    // TokenReview + SubjectAccessReview
    //
    // The coverage job has no cluster. `serve_metrics` only reaches Kubernetes
    // when it is given a client, so these tests point one at a loopback API.
    // -------------------------------------------------------------------------

    #[derive(Clone, Copy)]
    struct Reply {
        status: StatusCode,
        body: &'static str,
    }

    fn created(body: &'static str) -> Reply {
        Reply {
            status: StatusCode::CREATED,
            body,
        }
    }

    fn api_failure() -> Reply {
        Reply {
            status: StatusCode::BAD_REQUEST,
            body: r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"denied","code":400}"#,
        }
    }

    #[derive(Clone, Default)]
    struct Seen(std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>);

    impl Seen {
        fn record(&self, path: &str, body: &str) {
            self.0.lock().unwrap().push((path.to_owned(), body.to_owned()));
        }

        fn contains(&self, path_suffix: &str, needle: &str) -> bool {
            self.0
                .lock()
                .unwrap()
                .iter()
                .any(|(path, body)| path.ends_with(path_suffix) && body.contains(needle))
        }
    }

    async fn kube_client(token_review: Reply, subject_access: Reply) -> (kube::Client, Seen) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Seen::default();
        tokio::spawn(accept_api(listener, seen.clone(), token_review, subject_access));
        let mut config = kube::Config::new(format!("http://{address}").parse().unwrap());
        config.connect_timeout = Some(Duration::from_secs(2));
        config.default_retry = false;
        let client = kube::Client::try_from(config).unwrap();
        (client, seen)
    }

    async fn accept_api(listener: tokio::net::TcpListener, seen: Seen, token_review: Reply, subject_access: Reply) {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request| {
                    let seen = seen.clone();
                    async move {
                        Ok::<_, std::convert::Infallible>(
                            api_response(request, seen, token_review, subject_access).await,
                        )
                    }
                });
                drop(
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await,
                );
            });
        }
    }

    async fn api_response(
        request: Request<hyper::body::Incoming>,
        seen: Seen,
        token_review: Reply,
        subject_access: Reply,
    ) -> Response<Full<Bytes>> {
        let path = request.uri().path().to_owned();
        let collected = http_body_util::BodyExt::collect(request.into_body()).await.ok();
        let body = collected.map(|collected| String::from_utf8_lossy(&collected.to_bytes()).into_owned());
        seen.record(&path, body.as_deref().unwrap_or(""));
        json_reply(reply_for(&path, token_review, subject_access))
    }

    fn reply_for(path: &str, token_review: Reply, subject_access: Reply) -> Reply {
        if path.ends_with("/tokenreviews") {
            token_review
        } else if path.ends_with("/subjectaccessreviews") {
            subject_access
        } else {
            Reply {
                status: StatusCode::NOT_FOUND,
                body: "{}",
            }
        }
    }

    fn json_reply(reply: Reply) -> Response<Full<Bytes>> {
        let mut response = Response::new(Full::new(Bytes::from(reply.body)));
        *response.status_mut() = reply.status;
        response
            .headers_mut()
            .insert(hyper::header::CONTENT_TYPE, "application/json".parse().unwrap());
        response
    }

    async fn scrape(client: &kube::Client, authorization: Option<&str>) -> (StatusCode, String) {
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let mut builder = Request::builder().uri("/metrics");
        if let Some(value) = authorization {
            builder = builder.header(hyper::header::AUTHORIZATION, value);
        }
        let response = serve_metrics(&builder.body(()).unwrap(), &handle, Some(client)).await;
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn assert_outcome(
        token_review: Reply,
        subject_access: Reply,
        authorization: Option<&str>,
        status: StatusCode,
        body: &str,
    ) {
        let (client, _) = kube_client(token_review, subject_access).await;
        let (got_status, got_body) = scrape(&client, authorization).await;
        assert_eq!(got_status, status, "metrics status");
        assert_eq!(got_body, body, "metrics body");
    }

    #[tokio::test]
    async fn metrics_without_bearer_is_unauthorized() {
        assert_outcome(
            created(TOKEN_REJECTED),
            created(SAR_DENIED),
            None,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
    }

    #[tokio::test]
    async fn metrics_with_non_bearer_scheme_is_unauthorized() {
        assert_outcome(
            created(TOKEN_REJECTED),
            created(SAR_DENIED),
            Some("Basic dXNlcjpwYXNz"),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
    }

    #[tokio::test]
    async fn rejected_token_is_unauthorized() {
        assert_outcome(
            created(TOKEN_REJECTED),
            created(SAR_DENIED),
            Some("Bearer secret-token"),
            StatusCode::UNAUTHORIZED,
            "unauthorized",
        )
        .await;
    }

    #[tokio::test]
    async fn token_review_without_status_is_an_internal_error() {
        assert_outcome(
            created(TOKEN_NO_STATUS),
            created(SAR_DENIED),
            Some("Bearer secret-token"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )
        .await;
    }

    #[tokio::test]
    async fn token_review_without_authenticated_is_an_internal_error() {
        assert_outcome(
            created(TOKEN_NO_FLAG),
            created(SAR_DENIED),
            Some("Bearer secret-token"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )
        .await;
    }

    #[tokio::test]
    async fn stalled_token_review_is_an_internal_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(hold_accepted_connections(listener));
        let mut config = kube::Config::new(format!("http://{address}").parse().unwrap());
        config.connect_timeout = Some(Duration::from_secs(2));
        config.default_retry = false;
        let client = kube::Client::try_from(config).unwrap();
        let finished =
            tokio::time::timeout(Duration::from_secs(15), scrape(&client, Some("Bearer secret-token"))).await;
        assert!(finished.is_ok(), "a silent API server must not hold the scrape open");
        let (status, body) = finished.unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "stalled TokenReview is 500");
        assert_eq!(body, "internal error", "stalled TokenReview body");
    }

    async fn hold_accepted_connections(listener: tokio::net::TcpListener) {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _held = stream;
                std::future::pending::<()>().await;
            });
        }
    }

    #[tokio::test]
    async fn token_review_api_error_is_an_internal_error() {
        assert_outcome(
            api_failure(),
            created(SAR_DENIED),
            Some("Bearer secret-token"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )
        .await;
    }

    #[tokio::test]
    async fn denied_subject_access_review_is_forbidden() {
        let (client, seen) = kube_client(created(TOKEN_ALICE), created(SAR_DENIED)).await;
        let (status, body) = scrape(&client, Some("Bearer secret-token")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "SAR allowed=false is 403");
        assert_eq!(body, "forbidden", "forbidden body");
        assert!(
            seen.contains("/tokenreviews", "secret-token"),
            "TokenReview receives the bearer"
        );
        assert!(
            seen.contains("/subjectaccessreviews", "alice"),
            "SAR receives the TokenReview username"
        );
        assert!(
            seen.contains("/subjectaccessreviews", "readers"),
            "SAR receives the TokenReview groups"
        );
    }

    #[tokio::test]
    async fn allowed_token_renders_metrics() {
        let (client, seen) = kube_client(created(TOKEN_ALICE), created(SAR_ALLOWED)).await;
        let handle = PrometheusBuilder::new().build_recorder().handle();
        let request = Request::builder()
            .uri("/metrics")
            .header(hyper::header::AUTHORIZATION, "Bearer secret-token")
            .body(())
            .unwrap();
        let response = serve_metrics(&request, &handle, Some(&client)).await;
        assert_eq!(response.status(), StatusCode::OK, "allowed GET /metrics is 200");
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(
            bytes.as_ref(),
            handle.render().as_bytes(),
            "body is the Prometheus render"
        );
        assert!(
            seen.contains("/subjectaccessreviews", "alice"),
            "SAR receives the TokenReview username"
        );
    }

    #[tokio::test]
    async fn token_without_user_identity_still_renders_when_allowed() {
        assert_outcome(
            created(TOKEN_NO_USER),
            created(SAR_ALLOWED),
            Some("Bearer secret-token"),
            StatusCode::OK,
            "",
        )
        .await;
    }

    #[tokio::test]
    async fn subject_access_review_without_status_is_an_internal_error() {
        assert_outcome(
            created(TOKEN_ALICE),
            created(SAR_NO_STATUS),
            Some("Bearer secret-token"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )
        .await;
    }

    #[tokio::test]
    async fn subject_access_review_api_error_is_an_internal_error() {
        assert_outcome(
            created(TOKEN_ALICE),
            api_failure(),
            Some("Bearer secret-token"),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal error",
        )
        .await;
    }

    const TOKEN_ALICE: &str = r#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","status":{"authenticated":true,"user":{"username":"alice","groups":["readers"]}}}"#;
    const TOKEN_NO_USER: &str =
        r#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","status":{"authenticated":true}}"#;
    const TOKEN_REJECTED: &str =
        r#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","status":{"authenticated":false}}"#;
    const TOKEN_NO_STATUS: &str = r#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview"}"#;
    const TOKEN_NO_FLAG: &str = r#"{"apiVersion":"authentication.k8s.io/v1","kind":"TokenReview","status":{}}"#;
    const SAR_ALLOWED: &str =
        r#"{"apiVersion":"authorization.k8s.io/v1","kind":"SubjectAccessReview","status":{"allowed":true}}"#;
    const SAR_DENIED: &str =
        r#"{"apiVersion":"authorization.k8s.io/v1","kind":"SubjectAccessReview","status":{"allowed":false}}"#;
    const SAR_NO_STATUS: &str = r#"{"apiVersion":"authorization.k8s.io/v1","kind":"SubjectAccessReview"}"#;
}
