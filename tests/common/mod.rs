// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Generic ExtProc transport toolbox shared by every test binary: client type,
//! port allocation, message and header builders, and `ProcessingResponse`
//! inspectors. Tier-specific harnesses (stub filters in `adapter`, the recording
//! `NeMo` peer in `guardrails_transport`) build on top of these and re-export
//! them, so a test can reach any of these as `support::<name>`.

use std::time::Duration;

use praxis_extproc::grpc_limits::DEFAULT_GRPC_MAX_RECV_MSG_SIZE;
use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        BodySendMode, HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, ProcessingResponse, ProtocolConfiguration,
        StreamedBodyResponse, body_mutation, external_processor_client::ExternalProcessorClient,
        processing_request::Request as ReqVariant, processing_response::Response as RespVariant,
    },
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

pub(crate) mod payloads;

/// Shared idle timeout (milliseconds) for single-message response waits.
pub(crate) const TIMEOUT_MILLIS: u64 = 500;

pub(crate) type ExtProcClient = ExternalProcessorClient<Channel>;

/// Bind an ephemeral loopback port and return it, then drop the listener.
pub(crate) fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .expect("find a free port")
}

/// Connect an ExtProc client to an already-bound address, with gRPC message
/// limits pinned to the raised default so the server cap under test is the only
/// one that can reject a body.
pub(crate) async fn extproc_client_for_addr(addr: std::net::SocketAddr) -> ExtProcClient {
    let channel = Channel::from_shared(format!("http://{addr}"))
        .expect("uri")
        .connect()
        .await
        .expect("connect");
    ExtProcClient::new(channel)
        .max_decoding_message_size(DEFAULT_GRPC_MAX_RECV_MSG_SIZE)
        .max_encoding_message_size(DEFAULT_GRPC_MAX_RECV_MSG_SIZE)
}

/// Open a bidirectional `process` stream, returning the request sender and the
/// inbound response stream.
pub(crate) async fn open_stream(
    client: &mut ExtProcClient,
) -> (
    tokio::sync::mpsc::Sender<ProcessingRequest>,
    tonic::Streaming<ProcessingResponse>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let stream = client
        .process(ReceiverStream::new(rx))
        .await
        .expect("process call failed")
        .into_inner();
    (tx, stream)
}

/// Wrap a request variant into a `ProcessingRequest`.
pub(crate) fn wrap(request: ReqVariant) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(request),
        ..Default::default()
    }
}

pub(crate) fn make_header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_owned(),
        raw_value: Vec::new(),
    }
}

pub(crate) fn make_request_headers(method: &str, path: &str, end_of_stream: bool) -> ProcessingRequest {
    make_request_headers_with(method, path, end_of_stream, [])
}

/// Build request headers with extra values appended in the supplied order.
pub(crate) fn make_request_headers_with(
    method: &str,
    path: &str,
    end_of_stream: bool,
    extra_headers: impl IntoIterator<Item = HeaderValue>,
) -> ProcessingRequest {
    let mut headers = vec![
        make_header(":method", method),
        make_header(":path", path),
        make_header(":authority", "localhost"),
        make_header(":scheme", "http"),
    ];
    headers.extend(extra_headers);

    wrap(ReqVariant::RequestHeaders(HttpHeaders {
        headers: Some(HeaderMap { headers }),
        end_of_stream,
    }))
}

pub(crate) fn make_response_headers(status: u32, end_of_stream: bool) -> ProcessingRequest {
    make_response_headers_with(status, end_of_stream, [])
}

/// Build response headers with extra values appended in the supplied order.
pub(crate) fn make_response_headers_with(
    status: u32,
    end_of_stream: bool,
    extra_headers: impl IntoIterator<Item = HeaderValue>,
) -> ProcessingRequest {
    let mut headers = vec![make_header(":status", &status.to_string())];
    headers.extend(extra_headers);
    wrap(ReqVariant::ResponseHeaders(HttpHeaders {
        headers: Some(HeaderMap { headers }),
        end_of_stream,
    }))
}

/// Build HTTP 200 response headers with a content type and a body to follow.
pub(crate) fn response_headers(content_type: &str) -> ProcessingRequest {
    response_headers_with_eos(content_type, false)
}

/// Build HTTP 200 response headers with a content type and explicit completion.
pub(crate) fn response_headers_with_eos(content_type: &str, end_of_stream: bool) -> ProcessingRequest {
    let header = |key: &str, value: &str| HeaderValue {
        key: key.to_owned(),
        raw_value: value.as_bytes().to_vec(),
        ..Default::default()
    };
    wrap(ReqVariant::ResponseHeaders(HttpHeaders {
        headers: Some(HeaderMap {
            headers: vec![header(":status", "200"), header("content-type", content_type)],
        }),
        end_of_stream,
    }))
}

pub(crate) fn make_response_body(body: &[u8], end_of_stream: bool) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(ReqVariant::ResponseBody(HttpBody {
            body: body.to_vec(),
            end_of_stream,
        })),
        ..Default::default()
    }
}

/// Attach a first-message `protocol_config` selecting `response_body_mode`.
pub(crate) fn with_response_body_mode(
    mut msg: ProcessingRequest,
    response_body_mode: BodySendMode,
) -> ProcessingRequest {
    msg.protocol_config = Some(ProtocolConfiguration {
        request_body_mode: BodySendMode::None as i32,
        response_body_mode: response_body_mode as i32,
        send_body_without_waiting_for_header_response: false,
    });
    msg
}

/// Extract `end_of_stream` from a streamed request-body response, if present.
pub(crate) fn streamed_eos(msg: &ProcessingResponse) -> Option<bool> {
    match msg.response.as_ref()? {
        RespVariant::RequestBody(b) => match b.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref()? {
            body_mutation::Mutation::StreamedResponse(s) => Some(s.end_of_stream),
            _ => None,
        },
        _ => None,
    }
}

/// Pull the next response from a `FULL_DUPLEX` stream with the shared timeout.
pub(crate) async fn next_full_duplex_msg(stream: &mut tonic::Streaming<ProcessingResponse>) -> ProcessingResponse {
    tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), stream.message())
        .await
        .expect("timed out waiting for response")
        .expect("response stream error")
        .expect("stream closed before response")
}

pub(crate) async fn collect_responses(inbound: &mut tonic::Streaming<ProcessingResponse>) -> Vec<ProcessingResponse> {
    collect_responses_result(inbound).await.expect("collect responses")
}

pub(crate) async fn collect_responses_result(
    inbound: &mut tonic::Streaming<ProcessingResponse>,
) -> Result<Vec<ProcessingResponse>, tonic::Status> {
    let mut responses = Vec::new();
    let timeout = Duration::from_secs(2);

    loop {
        match tokio::time::timeout(timeout, inbound.message()).await {
            Ok(Ok(Some(resp))) => responses.push(resp),
            // Idle timeout (`Err`) means the stream went quiet.
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(status)) => return Err(status),
        }
    }

    Ok(responses)
}

/// Collect streamed body chunks from a response stream.
///
/// Returns `(chunks, reassembled_body)` where chunks is the vector of
/// `StreamedBodyResponse` messages and `reassembled_body` is all chunk
/// bodies concatenated.
///
/// Panics if stream errors, times out, or exceeds `max_chunks` without EOS.
pub(crate) async fn collect_streamed_chunks<F>(
    response_stream: &mut tonic::Streaming<ProcessingResponse>,
    extract_fn: F,
    max_chunks: usize,
) -> (Vec<StreamedBodyResponse>, Vec<u8>)
where
    F: Fn(&ProcessingResponse) -> Option<&StreamedBodyResponse>,
{
    let mut chunks = Vec::new();
    let mut received_body = Vec::new();

    loop {
        assert!(
            chunks.len() < max_chunks,
            "received {max_chunks} chunks without end_of_stream"
        );

        let outcome = tokio::time::timeout(Duration::from_millis(TIMEOUT_MILLIS), response_stream.message()).await;

        match outcome {
            Ok(Ok(Some(msg))) => {
                if let Some(s) = extract_fn(&msg) {
                    received_body.extend_from_slice(&s.body);
                    let is_eos = s.end_of_stream;
                    chunks.push(s.clone());
                    if is_eos {
                        break;
                    }
                } else {
                    panic!("expected StreamedResponse, got: {msg:?}");
                }
            },
            Ok(Ok(None)) => panic!("stream closed before EOS"),
            Ok(Err(err)) => panic!("stream error: {err}"),
            Err(_) => panic!("timed out waiting for chunk"),
        }
    }

    (chunks, received_body)
}

pub(crate) fn has_request_headers_response(responses: &[ProcessingResponse]) -> bool {
    responses
        .iter()
        .any(|r| matches!(&r.response, Some(RespVariant::RequestHeaders(_))))
}

pub(crate) fn extract_all_set_headers(responses: &[ProcessingResponse]) -> Vec<HeaderValue> {
    let mut headers = Vec::new();
    for r in responses {
        let mutation = match &r.response {
            Some(RespVariant::RequestHeaders(h)) => h.response.as_ref().and_then(|c| c.header_mutation.as_ref()),
            Some(RespVariant::RequestBody(b)) => b.response.as_ref().and_then(|c| c.header_mutation.as_ref()),
            _ => None,
        };
        if let Some(m) = mutation {
            for hvo in &m.set_headers {
                if let Some(hv) = &hvo.header {
                    headers.push(hv.clone());
                }
            }
        }
    }
    headers
}

pub(crate) fn immediate_status(responses: &[ProcessingResponse]) -> Option<i32> {
    responses.iter().find_map(|response| match &response.response {
        Some(RespVariant::ImmediateResponse(immediate)) => immediate.status.as_ref().map(|status| status.code),
        _ => None,
    })
}

pub(crate) fn immediate_body(response: &ProcessingResponse) -> String {
    match &response.response {
        Some(RespVariant::ImmediateResponse(immediate)) => immediate.body.clone(),
        _ => String::new(),
    }
}

pub(crate) fn body_bytes(responses: &[ProcessingResponse]) -> Vec<u8> {
    responses
        .iter()
        .filter_map(|response| match &response.response {
            Some(RespVariant::ResponseBody(body)) => body.response.as_ref()?.body_mutation.as_ref()?.mutation.as_ref(),
            _ => None,
        })
        .flat_map(|mutation| match mutation {
            body_mutation::Mutation::Body(body) => body.as_slice(),
            body_mutation::Mutation::StreamedResponse(body) => body.body.as_slice(),
            body_mutation::Mutation::ClearBody(_) => &[],
        })
        .copied()
        .collect()
}
