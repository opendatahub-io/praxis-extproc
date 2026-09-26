// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! FIPS behavior tests: what the ExtProc TLS listener and the shipped
//! binary do in approved mode, and that they do not do it outside one.
//!
//! Every test asserts both branches, so the suite is meaningful on any
//! host: on a developer machine the non-approved branch runs, and on a
//! FIPS host (`make test-fips-host`, which declares it with
//! `PRAXIS_FIPS_HOST=1`) the harness fails closed unless the process
//! really is in FIPS mode and every test insists on its approved branch.
//!
//! The listener under test is OpenSSL itself (`tokio-openssl`, the
//! platform's validated module in FIPS mode), so the `listener_` probes
//! attest the exact stack the product terminates TLS with. They normally
//! run against an in-process listener; `PRAXIS_FIPS_PROBE_ADDR` (with
//! `PRAXIS_FIPS_PROBE_CA` for the gRPC exchange) points them at a running
//! image instead, which is how `cargo xtask fips runtime-probe` drives
//! them against the shipped container. The approved branch of the TLS 1.2
//! probe encodes the module's Extended Master Secret requirement
//! (RFC 7627; SP 800-135 TLS KDF).
//!
//! The upstream (client) side of the FIPS story lives in praxis-ai's own
//! FIPS suite: extproc's outbound TLS is the same rustls-over-OpenSSL
//! provider praxis-ai attests, so only the listener and the binary are
//! extproc's to prove.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
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

mod probe;

use std::{net::SocketAddr, path::PathBuf, pin::Pin, process::Command, time::Duration};

use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use praxis_extproc::{config, server::PraxisExtProc, tls};
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        HeaderMap, HttpHeaders, ProcessingRequest, external_processor_client::ExternalProcessorClient,
        external_processor_server::ExternalProcessorServer, processing_request::Request as ReqVariant,
        processing_response::Response as RespVariant,
    },
};
use probe::{ClientHello, Reply, groups, suites};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint, Server};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

// -----------------------------------------------------------------------------
// Environment
// -----------------------------------------------------------------------------

/// Declares the host to be in FIPS mode; the harness then fails closed.
const FIPS_HOST_ENV: &str = "PRAXIS_FIPS_HOST";
/// Points the `listener_` probes at a running deployment's TLS address.
const PROBE_ADDR_ENV: &str = "PRAXIS_FIPS_PROBE_ADDR";
/// The CA (PEM path) the probed deployment's certificate chains to.
const PROBE_CA_ENV: &str = "PRAXIS_FIPS_PROBE_CA";

/// Whether the environment declares this a FIPS host. Only an empty value,
/// `0`, `false`, `no` or `off` leave it undeclared.
fn fips_host_declared() -> bool {
    std::env::var(FIPS_HOST_ENV).is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

/// Install the provider and read the FIPS status, failing closed on a
/// declared FIPS host that is not in FIPS mode.
fn checked_status() -> praxis_extproc::fips::Status {
    let status = praxis_extproc::fips::install().expect("install the crypto provider");
    if fips_host_declared() {
        let unmet = status.unmet();
        assert!(unmet.is_empty(), "{FIPS_HOST_ENV} declares a FIPS host but: {unmet:?}");
    }
    status
}

/// Whether the provider offers FIPS-approved algorithms only; on a declared
/// FIPS host this must be true, so a green run there cannot have quietly
/// taken the non-approved branches.
fn expect_approved_mode() -> bool {
    checked_status().provider_fips
}

/// Whether the process is in FIPS mode by both signals.
fn fips_host() -> bool {
    checked_status().unmet().is_empty()
}

/// Whether this build registers filters the FIPS build leaves out; mirrors
/// the binary the same features built (see `fips::blocker`).
fn carries_non_fips_filters() -> bool {
    cfg!(any(feature = "policy-engine", feature = "responses-store"))
}

// -----------------------------------------------------------------------------
// Listener Under Test
// -----------------------------------------------------------------------------

/// Keeps an in-process listener alive for the duration of a probe.
struct ListenerGuard {
    /// Shutdown sender; dropping it stops the server task.
    _shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

/// The TLS address the `listener_` probes target: a running deployment when
/// [`PROBE_ADDR_ENV`] names one, an in-process self-signed listener
/// otherwise.
async fn listener_target() -> (String, ListenerGuard) {
    if let Ok(addr) = std::env::var(PROBE_ADDR_ENV) {
        return (addr, ListenerGuard { _shutdown: None });
    }
    let cfg = tls::TlsConfig {
        mode: tls::TlsMode::SelfSigned,
        ..Default::default()
    };
    let (addr, shutdown) = serve_tls(cfg).await;
    (
        addr.to_string(),
        ListenerGuard {
            _shutdown: Some(shutdown),
        },
    )
}

/// Serve a headers-only ExtProc pipeline over the real TLS listener.
async fn serve_tls(cfg: tls::TlsConfig) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = tls::build_tls_config(&cfg).expect("tls config").expect("acceptor");
    let incoming = tls::build_tls_incoming(listener, acceptor, tls::HANDSHAKE_CONCURRENCY, tls::HANDSHAKE_TIMEOUT);
    let pipeline = headers_only_pipeline();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        drop(
            Server::builder()
                .add_service(ExternalProcessorServer::new(PraxisExtProc::new(pipeline)))
                .serve_with_incoming_shutdown(incoming, async { drop(shutdown_rx.await) })
                .await,
        );
    });
    (addr, shutdown_tx)
}

fn headers_only_pipeline() -> std::sync::Arc<praxis_filter::FilterPipeline> {
    let cfg: config::ExtProcConfig =
        serde_yaml::from_str("filter_chains:\n  - name: main\n    filters:\n      - filter: request_id\n")
            .expect("parse config");
    let registry = praxis_ai_filters::build_ai_registry();
    config::build_pipeline(&cfg, &registry).expect("build pipeline")
}

/// Run a raw probe against `addr` without blocking the async runtime.
async fn probe_at(addr: String, hello: ClientHello) -> Reply {
    tokio::task::spawn_blocking(move || probe::probe(&addr, &hello))
        .await
        .expect("probe task")
}

// -----------------------------------------------------------------------------
// Listener Probes
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn listener_answers_an_approved_client_hello_in_every_mode() {
    let _mode = expect_approved_mode();
    let (addr, _guard) = listener_target().await;
    let reply = probe_at(addr, ClientHello::approved("localhost")).await;
    assert!(
        !reply.refused(),
        "an approved offer must be acceptable in every mode, got {reply:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_negotiates_only_approved_algorithms_in_approved_mode() {
    let approved = expect_approved_mode();
    let (addr, _guard) = listener_target().await;
    let reply = probe_at(addr, ClientHello::approved("localhost")).await;
    let Reply::ServerHello { version, cipher_suite } = reply else {
        panic!("an approved offer must draw a ServerHello, got {reply:?}");
    };
    assert!(
        version == probe::TLS13 || version == probe::TLS12,
        "a modern version is selected, got {version:#06x}"
    );
    if approved {
        assert!(
            suites::AES_GCM.contains(&cipher_suite),
            "approved mode must select an AES-GCM suite, got {cipher_suite:#06x}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_refuses_chacha20_only_clients_in_approved_mode() {
    let approved = expect_approved_mode();
    let (addr, _guard) = listener_target().await;
    let mut hello = ClientHello::approved("localhost");
    hello.cipher_suites = suites::CHACHA20.to_vec();
    let reply = probe_at(addr, hello).await;
    if approved {
        assert!(
            reply.refused(),
            "approved mode must refuse a ChaCha20-only client, got {reply:?}"
        );
    } else {
        let Reply::ServerHello { cipher_suite, .. } = reply else {
            panic!("outside approved mode a ChaCha20-only client is served, got {reply:?}");
        };
        assert!(
            suites::is_chacha20(cipher_suite),
            "the only offered family is selected, got {cipher_suite:#06x}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_refuses_x25519_only_clients_in_approved_mode() {
    let approved = expect_approved_mode();
    let (addr, _guard) = listener_target().await;
    let mut hello = ClientHello::approved("localhost");
    hello.versions = vec![probe::TLS13];
    hello.groups = vec![groups::X25519];
    let reply = probe_at(addr, hello).await;
    if approved {
        assert!(
            reply.refused(),
            "approved mode must refuse an X25519-only key exchange, got {reply:?}"
        );
    } else {
        assert!(
            !reply.refused(),
            "outside approved mode an X25519-only client is served, got {reply:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_refuses_tls12_without_extended_master_secret_in_approved_mode() {
    let approved = expect_approved_mode();
    let (addr, guard) = listener_target().await;
    let reply = probe_at(addr.clone(), ClientHello::tls12("localhost", false)).await;
    if approved {
        assert!(
            reply.refused(),
            "the module's TLS 1.2 KDF requires EMS; a non-EMS client must be refused, got {reply:?}"
        );
    } else {
        assert!(
            !reply.refused(),
            "outside approved mode a non-EMS TLS 1.2 client is served, got {reply:?}"
        );
    }
    // Control: the same hello with EMS is acceptable in every mode.
    let control = probe_at(addr, ClientHello::tls12("localhost", true)).await;
    assert!(
        !control.refused(),
        "a TLS 1.2 client offering EMS must be acceptable in every mode, got {control:?}"
    );
    drop(guard);
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_completes_a_grpc_exchange_over_tls() {
    let _mode = expect_approved_mode();
    let (addr, _guard) = listener_target().await;
    let mut client = tls_grpc_client(&addr).await;

    let (tx, rx) = tokio::sync::mpsc::channel(4);
    let response = client.process(ReceiverStream::new(rx)).await.expect("process call");
    let mut inbound = response.into_inner();

    tx.send(request_headers("GET", "/")).await.expect("send headers");
    drop(tx);

    let msg = inbound
        .message()
        .await
        .expect("stream error")
        .expect("stream closed before a response");
    assert!(
        matches!(msg.response, Some(RespVariant::RequestHeaders(_))),
        "a real ExtProc exchange must complete over the TLS listener, got {msg:?}"
    );
}

// -----------------------------------------------------------------------------
// Listener Key Strength
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_listener_with_a_short_rsa_key_cannot_serve() {
    // Both branches refuse: in approved mode the validated module enforces
    // the FIPS 140-3 floor, and outside it every platform this product
    // supports (Red Hat's and Ubuntu's OpenSSL at security level 2) already
    // refuses RSA below 2048 bits ("ee key too small"), at key load or at
    // the first handshake. The control for "the listener can serve at all"
    // is a_self_signed_listener_serves_in_every_mode.
    let _mode = expect_approved_mode();
    let cfg = tls::TlsConfig {
        mode: tls::TlsMode::Provided,
        cert_path: Some(fixture("rsa1536-cert.pem")),
        key_path: Some(fixture("rsa1536-key.pem")),
        ..Default::default()
    };
    match tls::build_tls_config(&cfg) {
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.contains("too small") || message.contains("key"),
                "the refusal names the key strength: {message}"
            );
        },
        Ok(acceptor) => {
            let acceptor = acceptor.expect("provided mode builds an acceptor");
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr").to_string();
            let incoming = tls::build_tls_incoming(listener, acceptor, 1, tls::HANDSHAKE_TIMEOUT);
            let server = tokio::spawn(async move {
                use futures::StreamExt as _;
                drop(Box::pin(incoming).next().await);
            });
            let reply = probe_at(addr, ClientHello::approved("localhost")).await;
            assert!(reply.refused(), "a 1536-bit RSA key must never serve, got {reply:?}");
            server.abort();
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_self_signed_listener_serves_in_every_mode() {
    let _mode = expect_approved_mode();
    let cfg = tls::TlsConfig {
        mode: tls::TlsMode::SelfSigned,
        ..Default::default()
    };
    let (addr, _shutdown) = serve_tls(cfg).await;
    let reply = probe_at(addr.to_string(), ClientHello::approved("localhost")).await;
    assert!(
        !reply.refused(),
        "self-signed generation goes through the active provider and must serve, got {reply:?}"
    );
}

// -----------------------------------------------------------------------------
// The Binary
// -----------------------------------------------------------------------------

/// Run `praxis-extproc --validate` on the example config with the given
/// environment, returning (success, combined output). The tracing
/// subscriber writes to stdout, so both streams are captured.
fn validate_with(env: &[(&str, &str)]) -> (bool, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_praxis-extproc"));
    command.arg("-c").arg(example_config()).arg("--validate");
    command.env_remove("PRAXIS_REQUIRE_FIPS");
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().expect("the praxis-extproc binary must run");
    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.success(), combined)
}

#[test]
fn require_fips_validate_fails_closed_unless_the_host_is_in_fips_mode() {
    let (control_ok, control_out) = validate_with(&[]);
    assert!(control_ok, "without the variable, validate must succeed: {control_out}");

    let (ok, output) = validate_with(&[("PRAXIS_REQUIRE_FIPS", "1")]);
    if fips_host() {
        if carries_non_fips_filters() {
            assert!(!ok, "a binary carrying non-FIPS filters must refuse on a FIPS host");
            assert!(
                output.contains("run the FIPS build"),
                "the refusal names the filters and the fix, got: {output}"
            );
        } else {
            assert!(ok, "on a FIPS host the FIPS build must validate: {output}");
        }
    } else {
        assert!(
            !ok,
            "on a non-FIPS host the requirement is unmet and validate must fail"
        );
        assert!(
            output.contains("PRAXIS_REQUIRE_FIPS is set but FIPS mode is not in effect"),
            "the refusal names the variable and the state, got: {output}"
        );
    }
}

#[test]
fn a_false_value_does_not_require_fips() {
    let (ok, output) = validate_with(&[("PRAXIS_REQUIRE_FIPS", "false")]);
    assert!(ok, "PRAXIS_REQUIRE_FIPS=false must not require FIPS: {output}");
}

#[tokio::test(flavor = "multi_thread")]
async fn under_require_fips_the_server_serves_exactly_on_a_fips_host() {
    let serving_expected = fips_host() && !carries_non_fips_filters();
    let (grpc, health, metrics) = (free_port(), free_port(), free_port());
    let config_path = write_server_config(grpc, health, metrics);

    let mut child = Command::new(env!("CARGO_BIN_EXE_praxis-extproc"))
        .arg("-c")
        .arg(&config_path)
        .env_remove("PRAXIS_REQUIRE_FIPS")
        .env("PRAXIS_REQUIRE_FIPS", "1")
        .spawn()
        .expect("spawn praxis-extproc");

    let status = extproc_health_status(health).await;
    let serving = i32::from(tonic_health::pb::health_check_response::ServingStatus::Serving);
    let not_serving = i32::from(tonic_health::pb::health_check_response::ServingStatus::NotServing);
    let expected = if serving_expected { serving } else { not_serving };

    let kill = child.kill();
    drop(std::fs::remove_file(&config_path));
    kill.expect("kill praxis-extproc");
    child.wait().expect("reap praxis-extproc");

    assert_eq!(
        status, expected,
        "under PRAXIS_REQUIRE_FIPS the server serves exactly on a FIPS host with the FIPS build \
         (it stays up and inspectable either way)"
    );
}

/// Poll the health sidecar until the ExtProc service reports a status.
///
/// The refusal path binds the sidecars just like serving does, so the
/// health answer, whatever it is, proves the process stayed up.
async fn extproc_health_status(port: u16) -> i32 {
    let service = <ExternalProcessorServer<PraxisExtProc> as tonic::server::NamedService>::NAME.to_owned();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(channel) = Channel::from_shared(format!("http://127.0.0.1:{port}"))
            .expect("valid uri")
            .connect()
            .await
        {
            let mut client = tonic_health::pb::health_client::HealthClient::new(channel);
            if let Ok(resp) = client
                .check(tonic_health::pb::HealthCheckRequest {
                    service: service.clone(),
                })
                .await
            {
                return resp.into_inner().status;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the health sidecar never answered on 127.0.0.1:{port}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Write a minimal serving config on free ports; the caller removes it.
fn write_server_config(grpc: u16, health: u16, metrics: u16) -> PathBuf {
    let yaml = format!(
        "server:\n  grpc_address: \"127.0.0.1:{grpc}\"\n  health_address: \"127.0.0.1:{health}\"\n  \
         metrics_address: \"127.0.0.1:{metrics}\"\n  tls:\n    mode: none\nfilter_chains:\n  - name: main\n    \
         filters:\n      - filter: request_id\n"
    );
    let path = std::env::temp_dir().join(format!("praxis-extproc-fips-{}-{grpc}.yaml", std::process::id()));
    std::fs::write(&path, yaml).expect("write server config");
    path
}

/// A free loopback port.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("find a free port")
}

// -----------------------------------------------------------------------------
// gRPC Client Over OpenSSL
// -----------------------------------------------------------------------------

/// A tonic client whose transport is a TLS stream over OpenSSL, trusting
/// [`PROBE_CA_ENV`] when set (the image probe) and nothing otherwise (the
/// in-process listener's ephemeral certificate).
async fn tls_grpc_client(addr: &str) -> ExternalProcessorClient<Channel> {
    let addr = addr.to_owned();
    let connector = tower::service_fn(move |_uri: http::Uri| {
        let addr = addr.clone();
        async move {
            let tcp = TcpStream::connect(&addr).await?;
            let mut builder = SslConnector::builder(SslMethod::tls())?;
            if let Ok(ca) = std::env::var(PROBE_CA_ENV) {
                builder.set_ca_file(&ca)?;
                builder.set_verify(SslVerifyMode::PEER);
            } else {
                builder.set_verify(SslVerifyMode::NONE);
            }
            builder.set_alpn_protos(b"\x02h2")?;
            let ssl = builder.build().configure()?.into_ssl("localhost")?;
            let mut tls_stream = tokio_openssl::SslStream::new(ssl, tcp)?;
            Pin::new(&mut tls_stream).connect().await?;
            Ok::<_, BoxError>(hyper_util::rt::TokioIo::new(tls_stream))
        }
    });
    let channel = Endpoint::from_static("https://localhost")
        .connect_with_connector(connector)
        .await
        .expect("connect over TLS");
    ExternalProcessorClient::new(channel)
}

fn request_headers(method: &str, path: &str) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(ReqVariant::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![
                    HeaderValue {
                        key: ":method".into(),
                        raw_value: method.as_bytes().to_vec(),
                        ..Default::default()
                    },
                    HeaderValue {
                        key: ":path".into(),
                        raw_value: path.as_bytes().to_vec(),
                        ..Default::default()
                    },
                ],
            }),
            ..Default::default()
        })),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// Paths
// -----------------------------------------------------------------------------

/// A fixture under `tests/fixtures/fips/`, as a path string for `TlsConfig`.
fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/fips")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// The example configuration the `--validate` tests run against.
fn example_config() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/praxis-extproc.yaml")
        .to_string_lossy()
        .into_owned()
}
