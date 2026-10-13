// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Simulated native upstream phases over Envoy's ExtProc protocol.

#![expect(clippy::unwrap_used, clippy::too_many_lines, reason = "test helpers")]

use std::time::Duration;

use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, ProcessingResponse, ProtocolConfiguration,
        external_processor_client::ExternalProcessorClient, processing_request::Request, processing_response::Response,
    },
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

/// Emulate Envoy applying a header-time BUFFERED override to upstream chunks.
pub(crate) async fn native_exchange(
    client: &mut ExternalProcessorClient<Channel>,
    response_mode: i32,
    phases: Vec<Request>,
) -> Result<Vec<ProcessingResponse>, tonic::Status> {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let mut stream = client
        .process(ReceiverStream::new(receiver))
        .await
        .unwrap()
        .into_inner();
    let mut effective_mode = response_mode;
    let mut buffered_body = Vec::new();
    let mut responses = Vec::new();
    for (index, phase) in phases.into_iter().enumerate() {
        let phase = match phase {
            Request::ResponseBody(mut body) if effective_mode == 2 => {
                buffered_body.extend_from_slice(&body.body);
                if !body.end_of_stream {
                    continue;
                }
                body.body = std::mem::take(&mut buffered_body);
                Request::ResponseBody(body)
            },
            phase => phase,
        };
        sender
            .send(ProcessingRequest {
                request: Some(phase),
                protocol_config: (index == 0).then_some(ProtocolConfiguration {
                    request_body_mode: 2,
                    response_body_mode: response_mode,
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .unwrap()?
            .unwrap();
        assert!(
            !matches!(response.response, Some(Response::ImmediateResponse(_))),
            "native phase {index} must continue: {response:?}"
        );
        if let Some(mode) = response.mode_override.as_ref() {
            assert_eq!(index, 2, "mode overrides are valid only on header acknowledgements");
            assert_eq!(mode.request_body_mode, 2, "override preserves the request mode");
            effective_mode = mode.response_body_mode;
        }
        responses.push(response);
    }
    Ok(responses)
}

/// Separate data and empty EOS reproduce Envoy's streamed JSON delivery.
pub(crate) fn native_capture_phases(empty_eos: bool) -> Vec<Request> {
    let headers = [
        (":method", "POST"),
        (":path", "/v1/responses"),
        (":scheme", "https"),
        (":authority", "test.local"),
        ("x-test-user", "alice"),
        ("content-type", "application/json"),
    ];
    let mut phases = vec![
        Request::RequestHeaders(capture_headers(&headers)),
        Request::RequestBody(HttpBody { body: br#"{"model":"test","input":"hello","store":true}"#.to_vec(), end_of_stream: true }),
        Request::ResponseHeaders(capture_headers(&[(":status", "200"), ("content-type", "application/json")])),
        Request::ResponseBody(HttpBody { body: br#"{"id":"resp_native_test","object":"response","created_at":1,"model":"test","status":"completed","output":[],"store":true}"#.to_vec(), end_of_stream: !empty_eos }),
    ];
    if empty_eos {
        phases.push(Request::ResponseBody(HttpBody {
            body: Vec::new(),
            end_of_stream: true,
        }));
    }
    phases
}

/// Header messages retain the same fields Envoy passes to ExtProc.
pub(crate) fn capture_headers(headers: &[(&str, &str)]) -> HttpHeaders {
    HttpHeaders {
        headers: Some(HeaderMap {
            headers: headers
                .iter()
                .map(|(key, value)| HeaderValue {
                    key: (*key).to_owned(),
                    value: (*value).to_owned(),
                    raw_value: Vec::new(),
                })
                .collect(),
        }),
        end_of_stream: false,
    }
}
