// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Raw bytes-in/bytes-out `tonic` transport for the Kuadrant pipeline.
//!
//! Executes a [`GrpcDispatch`] as a real gRPC unary call. The request message is
//! already serialized protobuf built by `kuadrant-filter`, so the codec is a
//! pass-through, no shared proto types with the crate, hence no version coupling.
//! The upstream cluster name is resolved to a gRPC endpoint via a configured map.
//! TLS dials through system `OpenSSL`, not tonic's rustls, so the auth channel
//! honors the platform FIPS provider like the rest of this binary.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use http::{Uri, uri::PathAndQuery};
use hyper_util::rt::TokioIo;
use openssl::{
    ssl::{SslConnector, SslMethod, SslVerifyMode, SslVersion},
    x509::{X509, store::X509StoreBuilder},
};
use tokio::net::TcpStream;
use tonic::{
    Code, Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    metadata::{AsciiMetadataKey, AsciiMetadataValue, BinaryMetadataKey, BinaryMetadataValue, MetadataMap},
    transport::{Channel, Endpoint},
};
use tower_service::Service;
use tracing::warn;

use crate::kuadrant_host::{GrpcDispatch, GrpcTransport};

/// TLS settings for dialing one upstream: a CA to trust and the SNI / cert name
/// to verify against. Authorino serves TLS on its gRPC listener (signed by the
/// cluster service CA); Limitador is plaintext, so it needs no entry here.
#[derive(Debug, Clone)]
pub struct UpstreamTls {
    /// PEM-encoded CA certificate to trust for this upstream.
    pub ca_pem: Vec<u8>,
    /// Server name to send as SNI and verify the certificate against.
    pub sni: String,
}

/// A `tonic` codec that passes raw protobuf bytes through unchanged.
#[derive(Default, Clone, Copy)]
struct BytesCodec;

impl Codec for BytesCodec {
    type Decode = Vec<u8>;
    type Decoder = BytesDecoder;
    type Encode = Vec<u8>;
    type Encoder = BytesEncoder;

    fn encoder(&mut self) -> Self::Encoder {
        BytesEncoder
    }

    fn decoder(&mut self) -> Self::Decoder {
        BytesDecoder
    }
}

/// Encoder half of [`BytesCodec`]: writes the raw bytes through unchanged.
struct BytesEncoder;
impl Encoder for BytesEncoder {
    type Error = Status;
    type Item = Vec<u8>;

    fn encode(&mut self, item: Vec<u8>, dst: &mut EncodeBuf<'_>) -> Result<(), Status> {
        use bytes::BufMut as _;
        dst.put_slice(&item);
        Ok(())
    }
}

/// Decoder half of [`BytesCodec`]: reads the whole frame as raw bytes.
struct BytesDecoder;
impl Decoder for BytesDecoder {
    type Error = Status;
    type Item = Vec<u8>;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Vec<u8>>, Status> {
        use bytes::Buf as _;
        let n = src.remaining();
        Ok(Some(src.copy_to_bytes(n).to_vec()))
    }
}

/// Resolves Envoy upstream cluster names to gRPC endpoints and executes calls.
#[derive(Debug)]
pub struct TonicTransport {
    /// Cluster name (e.g. `authorino`, `limitador`) -> endpoint URI.
    upstreams: HashMap<String, String>,
    /// Per-upstream `OpenSSL` connector, built once at startup. Absent = plaintext.
    connectors: HashMap<String, OpenSslConnector>,
}

impl TonicTransport {
    /// Build a transport from a cluster-name -> URI map plus per-upstream TLS.
    /// Connectors are built once here so dispatch never re-parses a CA.
    ///
    /// # Errors
    /// Returns an error if any upstream CA cannot be built into a connector.
    pub fn new(upstreams: HashMap<String, String>, tls: HashMap<String, UpstreamTls>) -> Result<Self, String> {
        let connectors = tls
            .into_iter()
            .map(|(name, t)| openssl_connector(&t).map(|c| (name, c)))
            .collect::<Result<HashMap<_, _>, String>>()?;
        Ok(Self { upstreams, connectors })
    }

    /// Resolve the dispatch's endpoint (with `connect_timeout`) and its optional
    /// pre-built TLS connector. Kept out of the async `call` so the endpoint build
    /// stays off the dispatch future's frame.
    fn prepare(&self, pending: &GrpcDispatch) -> Result<(Endpoint, Option<OpenSslConnector>), String> {
        let uri = self
            .upstreams
            .get(&pending.upstream)
            .ok_or_else(|| format!("unknown upstream cluster: {}", pending.upstream))?;
        let endpoint = Channel::from_shared(uri.to_owned())
            .map_err(|e| e.to_string())?
            .connect_timeout(pending.timeout);
        Ok((endpoint, self.connectors.get(&pending.upstream).cloned()))
    }

    /// Execute the dispatch as a real gRPC call. Returns the completed call's
    /// `(status, body)` (status 0 = OK, non-zero = a real gRPC status the
    /// pipeline maps through failureMode), or `Err(reason)` for a transport-level
    /// failure (unknown upstream, connect/DNS/TLS failure, unready channel, or an
    /// elapsed deadline) that `call` collapses to UNAVAILABLE.
    async fn try_call(&self, pending: &GrpcDispatch) -> Result<(u32, Vec<u8>), String> {
        let (endpoint, connector) = self.prepare(pending)?;
        // Fresh channel per call: a tonic `Channel` binds its connection-driver
        // task to the runtime that built it, and this transport is shared across
        // the per-stream current-thread runtimes. `connect_timeout` bounds the dial
        // (including the OpenSSL handshake).
        // Boxed so the large connect future stays off `try_call`'s stack frame.
        let channel = match connector {
            Some(c) => Box::pin(endpoint.connect_with_connector(c)).await,
            None => Box::pin(endpoint.connect()).await,
        }
        .map_err(|e| format!("connect: {}", error_chain(&e)))?;
        let path = PathAndQuery::from_maybe_shared(format!("/{}/{}", pending.service, pending.method))
            .map_err(|e| format!("path: {e}"))?;
        let mut request = tonic::Request::new(pending.message.clone());
        attach_metadata(request.metadata_mut(), &pending.headers);
        let mut grpc = tonic::client::Grpc::new(channel);
        // Bound the RPC (ready + unary) so a backend that connects then hangs cannot
        // stall the request. With `connect_timeout` above, worst-case wall time is
        // up to ~2x `pending.timeout` (dial then RPC). `Err(None)` = unready channel,
        // `Err(Some)` = a completed non-OK status (a real result, not a failure).
        // Boxed to keep the large tonic client future off `try_call`'s frame.
        let rpc = Box::pin(async move {
            if grpc.ready().await.is_err() {
                return Err(None);
            }
            grpc.unary(request, path, BytesCodec).await.map_err(Some)
        });
        match tokio::time::timeout(pending.timeout, rpc).await {
            Ok(Ok(response)) => Ok((0, response.into_inner())),
            Ok(Err(Some(status))) => Ok((status.code() as u32, Vec::new())),
            Ok(Err(None)) => Err("channel not ready".to_owned()),
            Err(_) => Err("rpc deadline elapsed".to_owned()),
        }
    }
}

/// The `(status, body)` for a backend we could not reach. Reported instead of an
/// error so the pipeline runs each service's own failureMode.
fn unavailable() -> (u32, Vec<u8>) {
    (Code::Unavailable as u32, Vec::new())
}

/// Client TLS through system `OpenSSL` rather than tonic's rustls
/// `ClientTlsConfig`, so the dial honors the platform FIPS provider.
#[derive(Clone)]
struct OpenSslConnector {
    /// CA-pinned client context advertising h2 ALPN.
    connector: Arc<SslConnector>,
    /// SNI and certificate hostname to verify against.
    sni: Arc<str>,
}

impl std::fmt::Debug for OpenSslConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenSslConnector")
            .field("sni", &self.sni)
            .finish_non_exhaustive()
    }
}

impl Service<Uri> for OpenSslConnector {
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;
    type Response = TokioIo<tokio_openssl::SslStream<TcpStream>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Uri) -> Self::Future {
        let connector = Arc::clone(&self.connector);
        let sni = Arc::clone(&self.sni);
        Box::pin(async move {
            // `http::Uri::host` keeps the brackets on an IPv6 literal (`[::1]`).
            // Strip them so `TcpStream::connect` can parse the address.
            let host = strip_brackets(req.host().ok_or("gRPC endpoint URI has no host")?);
            let port = req.port_u16().unwrap_or(443);
            let tcp = TcpStream::connect((host, port)).await?;
            // into_ssl sets SNI and the cert hostname to verify against.
            let ssl = connector.configure()?.into_ssl(sni.as_ref())?;
            let mut tls = tokio_openssl::SslStream::new(ssl, tcp)?;
            Pin::new(&mut tls).connect().await?;
            Ok(TokioIo::new(tls))
        })
    }
}

/// Build the connector: trust only the pinned CA, PEER verify, h2 ALPN.
fn openssl_connector(tls: &UpstreamTls) -> Result<OpenSslConnector, String> {
    // An empty sni makes `into_ssl` clear OpenSSL's host-verify list, silently
    // disabling SAN/hostname verification while chain verify still holds. Fail loud.
    if tls.sni.trim().is_empty() {
        return Err("upstream TLS sni must not be empty".to_owned());
    }
    let ca = X509::from_pem(&tls.ca_pem).map_err(|e| format!("upstream CA cert: {e}"))?;
    let mut store = X509StoreBuilder::new().map_err(|e| format!("cert store: {e}"))?;
    store.add_cert(ca).map_err(|e| format!("trust CA cert: {e}"))?;
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(|e| format!("SSL connector: {e}"))?;
    builder.set_cert_store(store.build());
    builder.set_verify(SslVerifyMode::PEER);
    // Floor at TLS 1.2 so it holds without relying on the system crypto policy.
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(|e| format!("min TLS version: {e}"))?;
    builder
        .set_alpn_protos(b"\x02h2")
        .map_err(|e| format!("ALPN protos: {e}"))?;
    Ok(OpenSslConnector {
        connector: Arc::new(builder.build()),
        sni: Arc::from(tls.sni.as_str()),
    })
}

/// Strip the surrounding brackets from an IPv6-literal host (`[::1]` -> `::1`).
/// Leaves DNS names and IPv4 untouched.
fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host)
}

/// Flatten an error and its `source()` chain: tonic's Display drops the TLS
/// handshake / cert-verify cause that lives in `source()`.
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut msg = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    msg
}

#[async_trait]
impl GrpcTransport for TonicTransport {
    async fn call(&self, pending: &GrpcDispatch) -> Result<(u32, Vec<u8>), String> {
        // Never surface a transport failure as `Err`: that escapes the pipeline
        // and hands the allow/deny decision to Envoy's single `failure_mode_allow`,
        // which cannot be both fail-closed for auth and fail-open for rate-limit.
        // Mapping to UNAVAILABLE lets each service's own failureMode govern
        // (Authorino fails closed, Limitador fails open).
        match self.try_call(pending).await {
            Ok(outcome) => Ok(outcome),
            Err(reason) => {
                warn!(upstream = %pending.upstream, reason = %reason, "kuadrant transport: backend unreachable, applying failureMode");
                Ok(unavailable())
            },
        }
    }
}

/// Copy the pipeline's deferred headers onto the outgoing gRPC request as
/// metadata. Authorino keys its `Check` decision off these request attributes,
/// so dropping them (the old behaviour) silently defeated auth. `-bin` keys carry
/// raw binary values; everything else is ASCII. Pseudo-headers (`:path`, ...) and
/// values that are not valid metadata are skipped rather than failing the call.
fn attach_metadata(md: &mut MetadataMap, headers: &[(String, Vec<u8>)]) {
    for (name, value) in headers {
        if name.starts_with(':') {
            continue;
        }
        let key = name.to_ascii_lowercase();
        if key.ends_with("-bin") {
            if let Ok(k) = BinaryMetadataKey::from_bytes(key.as_bytes()) {
                md.insert_bin(k, BinaryMetadataValue::from_bytes(value));
            }
        } else if let (Ok(k), Ok(v)) = (
            key.parse::<AsciiMetadataKey>(),
            AsciiMetadataValue::try_from(value.as_slice()),
        ) {
            md.insert(k, v);
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "tests")]
mod tests {
    use super::{UpstreamTls, openssl_connector, strip_brackets};

    #[test]
    fn openssl_connector_rejects_empty_sni() {
        // Empty/blank sni is refused before the CA is even parsed, so a misconfig
        // fails loud instead of dialing with hostname verification disabled.
        for sni in ["", "   "] {
            let tls = UpstreamTls {
                ca_pem: b"unused".to_vec(),
                sni: sni.to_owned(),
            };
            let err = openssl_connector(&tls).expect_err("empty sni must be rejected");
            assert!(err.contains("sni"), "unexpected error: {err}");
        }
    }

    #[test]
    fn strip_brackets_only_unwraps_ipv6_literals() {
        assert_eq!(strip_brackets("[::1]"), "::1");
        assert_eq!(strip_brackets("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(strip_brackets("authorino.svc"), "authorino.svc");
        assert_eq!(strip_brackets("10.0.0.1"), "10.0.0.1");
    }
}
