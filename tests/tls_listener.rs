// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Protocol-level integration tests for the TLS listener.
//!
//! Drives a real gRPC call and mTLS handshakes over `build_tls_incoming`,
//! covering ALPN negotiation and the negotiated TLS version.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    clippy::missing_docs_in_private_items,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "tests"
)]
#![allow(missing_docs, reason = "test module")]

use std::{
    net::SocketAddr,
    pin::Pin,
    sync::atomic::{AtomicU32, Ordering},
};

use openssl::{
    pkey::PKey,
    ssl::{SslConnector, SslMethod, SslVerifyMode},
    x509::X509,
};
use praxis_extproc::{config, server::PraxisExtProc, tls};
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        HeaderMap, HttpHeaders, ProcessingRequest, external_processor_client::ExternalProcessorClient,
        external_processor_server::ExternalProcessorServer, processing_request::Request as ReqVariant,
        processing_response::Response as RespVariant,
    },
};
use tokio::net::{TcpListener, TcpStream};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint, Server};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type ExtProcClient = ExternalProcessorClient<Channel>;

const HEADERS_ONLY: &str = r#"
filter_chains:
  - name: main
    filters:
      - filter: request_id
"#;

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

/// A gRPC `Process` call succeeds over the real TLS listener (F5).
#[tokio::test]
async fn grpc_over_tls_returns_headers_response() {
    let (addr, _shutdown) = serve_tls(self_signed()).await;
    let mut client = tls_grpc_client(addr).await;

    let (tx, rx) = tokio::sync::mpsc::channel(16);
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
        "gRPC over TLS should yield a request-headers response, got: {msg:?}"
    );
}

/// The handshake selects h2 via ALPN and negotiates a modern TLS version (F3, and
/// the home for the cipher-preset version assertions).
#[tokio::test]
async fn handshake_negotiates_h2_and_modern_tls() {
    let (addr, _shutdown) = serve_tls(self_signed()).await;
    let tls_stream = raw_connect(addr, None).await.expect("handshake");
    let ssl = tls_stream.ssl();

    assert_eq!(ssl.selected_alpn_protocol(), Some(&b"h2"[..]), "ALPN must select h2");
    let version = ssl.version_str();
    assert!(
        version == "TLSv1.3" || version == "TLSv1.2",
        "must negotiate TLS 1.2 or 1.3, got {version}"
    );
}

/// A client offering only a non-h2 protocol is refused at the handshake (F3).
///
/// This fails under the old no-ack behavior, which completed with no ALPN.
#[tokio::test]
async fn handshake_rejects_non_h2_alpn() {
    let (addr, _shutdown) = serve_tls(self_signed()).await;
    let result = raw_connect_alpn(addr, None, b"\x08http/1.1").await;
    assert!(result.is_err(), "a non-h2 ALPN offer must fail the handshake");
}

/// mTLS accepts a client presenting a certificate signed by the trusted CA (F5).
#[tokio::test]
async fn mtls_accepts_valid_client_cert() {
    let pki = Pki::generate();
    let identity = (pki.client_cert_pem.clone(), pki.client_key_pem.clone());

    let accepted = server_accepts(pki.server_config(), Some(identity)).await;
    assert!(accepted, "server should accept a client cert signed by the trusted CA");
}

/// mTLS rejects a client that presents no certificate (F5).
///
/// The server side is authoritative: under TLS 1.3 the client considers the
/// handshake done before the server's client-auth rejection arrives.
#[tokio::test]
async fn mtls_rejects_missing_client_cert() {
    let pki = Pki::generate();

    let accepted = server_accepts(pki.server_config(), None).await;
    assert!(!accepted, "server must reject a client that presents no certificate");
}

// -----------------------------------------------------------------------------
// Server
// -----------------------------------------------------------------------------

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

    // No readiness wait needed: the listener is bound before the task spawns, so
    // a client connect lands in the backlog until the server accepts it.
    (addr, shutdown_tx)
}

/// Drive one client handshake through `build_tls_incoming` and report whether
/// the server accepted it. Authoritative for mTLS client-auth outcomes.
async fn server_accepts(cfg: tls::TlsConfig, identity: Option<(String, String)>) -> bool {
    use futures::StreamExt as _;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = tls::build_tls_config(&cfg).expect("tls config").expect("acceptor");
    let mut incoming = Box::pin(tls::build_tls_incoming(listener, acceptor, 1, tls::HANDSHAKE_TIMEOUT));

    let client = tokio::spawn(async move { drop(raw_connect(addr, identity).await) });
    let outcome = incoming.next().await;
    drop(client.await);
    matches!(outcome, Some(Ok(_)))
}

fn headers_only_pipeline() -> std::sync::Arc<praxis_filter::FilterPipeline> {
    let cfg: config::ExtProcConfig = serde_yaml::from_str(HEADERS_ONLY).expect("parse config");
    let registry = praxis_ai_filters::build_ai_registry();
    config::build_pipeline(&cfg, &registry).expect("build pipeline")
}

// -----------------------------------------------------------------------------
// Client
// -----------------------------------------------------------------------------

/// Build a tonic client whose transport is a TLS stream over `OpenSSL`.
async fn tls_grpc_client(addr: SocketAddr) -> ExtProcClient {
    let connector = tower::service_fn(move |_uri: http::Uri| async move {
        let tls_stream = raw_connect(addr, None).await?;
        Ok::<_, BoxError>(hyper_util::rt::TokioIo::new(tls_stream))
    });
    // The connector brings its own OpenSSL TLS, so use an http scheme: with the
    // tonic `tls-ring` feature on, an https endpoint would make tonic try to own
    // the TLS handshake itself and reject the custom connector.
    let channel = Endpoint::from_static("http://localhost")
        .connect_with_connector(connector)
        .await
        .expect("connect over TLS");
    ExternalProcessorClient::new(channel)
}

/// Complete a TLS handshake as a client, offering h2 and optionally a client cert.
async fn raw_connect(
    addr: SocketAddr,
    identity: Option<(String, String)>,
) -> Result<tokio_openssl::SslStream<TcpStream>, BoxError> {
    raw_connect_alpn(addr, identity, b"\x02h2").await
}

/// As [`raw_connect`], with the client's advertised ALPN protocol list.
async fn raw_connect_alpn(
    addr: SocketAddr,
    identity: Option<(String, String)>,
    alpn: &[u8],
) -> Result<tokio_openssl::SslStream<TcpStream>, BoxError> {
    let tcp = TcpStream::connect(addr).await?;
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_verify(SslVerifyMode::NONE);
    builder.set_alpn_protos(alpn)?;
    if let Some((cert_pem, key_pem)) = identity {
        let cert = X509::from_pem(cert_pem.as_bytes())?;
        let key = PKey::private_key_from_pem(key_pem.as_bytes())?;
        builder.set_certificate(&cert)?;
        builder.set_private_key(&key)?;
    }
    let ssl = builder.build().configure()?.into_ssl("localhost")?;
    let mut tls_stream = tokio_openssl::SslStream::new(ssl, tcp)?;
    Pin::new(&mut tls_stream).connect().await?;
    Ok(tls_stream)
}

fn request_headers(method: &str, path: &str) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(ReqVariant::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![
                    header(":method", method),
                    header(":path", path),
                    header(":authority", "localhost"),
                    header(":scheme", "https"),
                ],
            }),
            end_of_stream: true,
        })),
        ..Default::default()
    }
}

fn header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_owned(),
        raw_value: Vec::new(),
    }
}

// -----------------------------------------------------------------------------
// PKI
// -----------------------------------------------------------------------------

/// A test CA with a CA-signed server and client certificate.
struct Pki {
    ca_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

impl Pki {
    fn generate() -> Self {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair};

        let ca_key = KeyPair::generate().expect("ca key");
        let mut ca = CertificateParams::new(Vec::new()).expect("ca params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name.push(DnType::CommonName, "praxis-extproc-test-ca");
        let ca_cert = ca.self_signed(&ca_key).expect("ca cert");

        let server_key = KeyPair::generate().expect("server key");
        let server = CertificateParams::new(vec!["localhost".to_owned()]).expect("server params");
        let server_cert = server.signed_by(&server_key, &ca_cert, &ca_key).expect("server cert");

        let client_key = KeyPair::generate().expect("client key");
        let mut client = CertificateParams::new(vec!["praxis-client".to_owned()]).expect("client params");
        client.distinguished_name.push(DnType::CommonName, "praxis-client");
        let client_cert = client.signed_by(&client_key, &ca_cert, &ca_key).expect("client cert");

        Self {
            ca_pem: ca_cert.pem(),
            server_cert_pem: server_cert.pem(),
            server_key_pem: server_key.serialize_pem(),
            client_cert_pem: client_cert.pem(),
            client_key_pem: client_key.serialize_pem(),
        }
    }

    /// Write the server identity and CA to disk and return a provided-mode config.
    fn server_config(&self) -> tls::TlsConfig {
        tls::TlsConfig {
            mode: tls::TlsMode::Provided,
            cert_path: Some(write_temp("server-cert", &self.server_cert_pem)),
            key_path: Some(write_temp("server-key", &self.server_key_pem)),
            ca_cert_path: Some(write_temp("ca-cert", &self.ca_pem)),
            ..tls::TlsConfig::default()
        }
    }
}

fn self_signed() -> tls::TlsConfig {
    tls::TlsConfig {
        mode: tls::TlsMode::SelfSigned,
        ..tls::TlsConfig::default()
    }
}

fn write_temp(kind: &str, pem: &str) -> String {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("praxis_tls_listener_{kind}_{}_{n}.pem", std::process::id()));
    std::fs::write(&path, pem).expect("write pem");
    path.to_string_lossy().into_owned()
}
