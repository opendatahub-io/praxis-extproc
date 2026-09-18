// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Synthetic ext-proc client for the Kuadrant native-enforcement smoke test.
//!
//! Drives one processing stream at a running praxis-extproc: request headers
//! with a credential, then response headers and a body carrying token usage.
//! Prints what the server decides at each phase, so auth and token reporting
//! are visible. Behaviour is tuned by env vars read in `main`.

#![allow(
    clippy::print_stdout,
    clippy::missing_docs_in_private_items,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::large_futures,
    clippy::large_stack_frames,
    clippy::collapsible_if,
    clippy::missing_errors_doc,
    reason = "throwaway smoke-test example binary"
)]

use std::env;

use praxis_proto::envoy::service::{
    common::v3::HeaderValue,
    ext_proc::v3::{
        HeaderMap, HttpBody, HttpHeaders, ProcessingRequest, ProcessingResponse,
        external_processor_client::ExternalProcessorClient, processing_request::Request as Req,
        processing_response::Response as Resp,
    },
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target = env::var("TARGET").unwrap_or_else(|_| "http://127.0.0.1:9004".to_owned());
    let authz = env::var("AUTHZ").unwrap_or_else(|_| "APIKEY bogus-token".to_owned());
    let path = env::var("REQPATH").unwrap_or_else(|_| "/v1/chat/completions".to_owned());
    let tokens = env::var("TOKENS").unwrap_or_else(|_| "42".to_owned());
    let host = env::var("HOSTHDR").unwrap_or_else(|_| "example.com".to_owned());
    let model = env::var("MODEL").unwrap_or_else(|_| "Qwen/Qwen3-0.6B".to_owned());

    println!("target={target} host={host} path={path} model={model} authz={authz:?} tokens={tokens}");
    let channel = Channel::from_shared(target)?.connect().await?;
    let mut client = ExternalProcessorClient::new(channel);

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let inbound = client.process(ReceiverStream::new(rx)).await?;
    let mut inbound = inbound.into_inner();

    // Phase 1: request headers with the credential Authorino keys off.
    tx.send(request_headers(&path, &authz, &host, &model)).await?;
    let first = inbound.message().await?;
    println!("request-headers  -> {}", describe(first.as_ref()));
    if let Some(r) = &first {
        if matches!(r.response, Some(Resp::ImmediateResponse(_))) {
            println!("DENIED at request phase (auth). Smoke proved the auth path.");
            return Ok(());
        }
    }

    // Phase 2: response headers (JSON, as a chat completion).
    tx.send(response_headers()).await?;
    drop(inbound.message().await?);

    // Phase 3: response body carrying usage, which triggers the token report.
    let body = format!("{{\"usage\":{{\"total_tokens\":{tokens}}}}}");
    tx.send(response_body(body.as_bytes())).await?;
    let last = inbound.message().await?;
    println!("response-body    -> {}", describe(last.as_ref()));

    drop(tx);
    println!("stream complete.");
    Ok(())
}

fn describe(resp: Option<&ProcessingResponse>) -> String {
    match resp.and_then(|r| r.response.as_ref()) {
        Some(Resp::RequestHeaders(_)) => "CONTINUE (request headers)".to_owned(),
        Some(Resp::ResponseHeaders(_)) => "CONTINUE (response headers)".to_owned(),
        Some(Resp::ResponseBody(_)) => "CONTINUE (response body)".to_owned(),
        Some(Resp::ImmediateResponse(ir)) => format!("IMMEDIATE {:?}", ir.status),
        other => format!("{other:?}"),
    }
}

fn header(key: &str, value: &str) -> HeaderValue {
    HeaderValue {
        key: key.to_owned(),
        value: value.to_owned(),
        raw_value: Vec::new(),
    }
}

fn request_headers(path: &str, authz: &str, host: &str, model: &str) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(Req::RequestHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![
                    header(":method", "POST"),
                    header(":path", path),
                    header(":authority", host),
                    header(":scheme", "https"),
                    header("authorization", authz),
                    header("content-type", "application/json"),
                    header("x-gateway-model-name", model),
                ],
            }),
            end_of_stream: false,
        })),
        ..Default::default()
    }
}

fn response_headers() -> ProcessingRequest {
    ProcessingRequest {
        request: Some(Req::ResponseHeaders(HttpHeaders {
            headers: Some(HeaderMap {
                headers: vec![header(":status", "200"), header("content-type", "application/json")],
            }),
            end_of_stream: false,
        })),
        ..Default::default()
    }
}

fn response_body(body: &[u8]) -> ProcessingRequest {
    ProcessingRequest {
        request: Some(Req::ResponseBody(HttpBody {
            body: body.to_vec(),
            end_of_stream: true,
        })),
        ..Default::default()
    }
}
