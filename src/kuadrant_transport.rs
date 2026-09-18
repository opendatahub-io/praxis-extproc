// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Raw bytes-in/bytes-out `tonic` transport for the Kuadrant pipeline.
//!
//! Executes a [`GrpcDispatch`] as a real gRPC unary call. The request message is
//! already serialized protobuf built by `kuadrant-filter`, so the codec is a
//! pass-through — no shared proto types with the crate, hence no version
//! coupling. The upstream cluster name is resolved to a gRPC endpoint via a
//! configured map.

use std::collections::HashMap;

use async_trait::async_trait;
use http::uri::PathAndQuery;
use tonic::{
    Code, Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    metadata::{AsciiMetadataKey, AsciiMetadataValue, BinaryMetadataKey, BinaryMetadataValue, MetadataMap},
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
};

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
        let mut out = vec![0_u8; n];
        src.copy_to_slice(&mut out);
        Ok(Some(out))
    }
}

/// Resolves Envoy upstream cluster names to gRPC endpoints and executes calls.
#[derive(Debug)]
pub struct TonicTransport {
    /// Cluster name (e.g. `authorino`, `limitador`) -> endpoint URI.
    upstreams: HashMap<String, String>,
    /// Per-upstream TLS settings. Absent = plaintext.
    tls: HashMap<String, UpstreamTls>,
}

impl TonicTransport {
    /// Build a transport from a cluster-name -> URI map plus per-upstream TLS.
    #[must_use]
    pub fn new(upstreams: HashMap<String, String>, tls: HashMap<String, UpstreamTls>) -> Self {
        Self { upstreams, tls }
    }

    /// Build the (optionally TLS-wrapped) endpoint for a dispatch. Kept separate
    /// from the async `call` so the large `ClientTlsConfig`/`Certificate` locals
    /// live in this synchronous frame, not the dispatch future's state machine.
    fn endpoint(&self, uri: &str, pending: &GrpcDispatch) -> Result<Endpoint, String> {
        let mut endpoint = Channel::from_shared(uri.to_owned())
            .map_err(|e| e.to_string())?
            .connect_timeout(pending.timeout);
        if let Some(t) = self.tls.get(&pending.upstream) {
            let tls = ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(&t.ca_pem))
                .domain_name(t.sni.clone());
            endpoint = endpoint.tls_config(tls).map_err(|e| e.to_string())?;
        }
        Ok(endpoint)
    }
}

#[async_trait]
impl GrpcTransport for TonicTransport {
    async fn call(&self, pending: &GrpcDispatch) -> Result<(u32, Vec<u8>), String> {
        let uri = self
            .upstreams
            .get(&pending.upstream)
            .ok_or_else(|| format!("unknown upstream cluster: {}", pending.upstream))?;
        let channel = self
            .endpoint(uri, pending)?
            .connect()
            .await
            .map_err(|e| e.to_string())?;
        let mut grpc = tonic::client::Grpc::new(channel);
        // A backend we cannot reach is not a transport panic: report it as
        // UNAVAILABLE so the pipeline applies the service's failure mode
        // (Authorino auth fails closed, Limitador rate-limit fails open).
        if grpc.ready().await.is_err() {
            return Ok((Code::Unavailable as u32, Vec::new()));
        }
        let path = PathAndQuery::from_maybe_shared(format!("/{}/{}", pending.service, pending.method))
            .map_err(|e| e.to_string())?;
        let mut request = tonic::Request::new(pending.message.clone());
        attach_metadata(request.metadata_mut(), &pending.headers);
        match grpc.unary(request, path, BytesCodec).await {
            // gRPC status 0: the policy decision (allow / OVER_LIMIT / denied)
            // lives in-band, inside the response message body.
            Ok(response) => Ok((0, response.into_inner())),
            // A non-OK gRPC status is a call-level failure. Hand its code back so
            // the pipeline runs the service's failure mode instead of aborting the
            // whole stream; there is no message body to parse on an error status.
            Err(status) => Ok((status.code() as u32, Vec::new())),
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
