// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Builders for ExtProc [`ProcessingResponse`] messages.
//!
//! Constructs well-formed responses for each ExtProc phase (headers,
//! body, trailers) and handles body chunking at the 62 KiB boundary
//! required by Envoy.
//!
//! [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse

use praxis_proto::envoy::service::ext_proc::v3::{
    BodyMutation, BodyResponse, CommonResponse, HeaderMutation, HeadersResponse, ImmediateResponse, ProcessingResponse,
    StreamedBodyResponse, TrailersResponse, body_mutation, common_response::ResponseStatus,
    processing_response::Response,
};

use crate::server::BodyMode;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

/// Maximum body chunk size for streamed responses.
///
/// Envoy enforces a ~64 KiB limit per streamed body chunk. Using 62 KiB
/// provides a safety margin.
const BODY_CHUNK_LIMIT: usize = 63_488; // 62 KiB

// -----------------------------------------------------------------------------
// Header Responses
// -----------------------------------------------------------------------------

/// Build a [`ProcessingResponse`] for the request headers phase.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn request_headers(mutation: Option<HeaderMutation>) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(Response::RequestHeaders(HeadersResponse {
            response: Some(CommonResponse {
                status: ResponseStatus::Continue.into(),
                header_mutation: mutation,
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

/// Build a [`ProcessingResponse`] for the response headers phase.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn response_headers(mutation: Option<HeaderMutation>) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(Response::ResponseHeaders(HeadersResponse {
            response: Some(CommonResponse {
                status: ResponseStatus::Continue.into(),
                header_mutation: mutation,
                ..Default::default()
            }),
        })),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// Body Responses
// -----------------------------------------------------------------------------

/// Build [`ProcessingResponse`] messages for the request body phase.
///
/// When the body was mutated, sends chunked body responses at the
/// 62 KiB boundary. Otherwise sends a single continue.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn request_body(
    body: Option<&[u8]>,
    mutation: Option<HeaderMutation>,
    body_mode: BodyMode,
) -> Vec<ProcessingResponse> {
    body_responses(body, mutation, true, body_mode)
}

/// Build [`ProcessingResponse`] messages for the response body phase.
///
/// Same chunking logic as [`request_body`] but wraps in `ResponseBody`.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn response_body(
    body: Option<&[u8]>,
    mutation: Option<HeaderMutation>,
    body_mode: BodyMode,
) -> Vec<ProcessingResponse> {
    body_responses(body, mutation, false, body_mode)
}

// -----------------------------------------------------------------------------
// Trailer Responses
// -----------------------------------------------------------------------------

/// Build a passthrough [`ProcessingResponse`] for request trailers.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn request_trailers() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(Response::RequestTrailers(TrailersResponse { header_mutation: None })),
        ..Default::default()
    }
}

/// Build a passthrough [`ProcessingResponse`] for response trailers.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn response_trailers() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(Response::ResponseTrailers(TrailersResponse { header_mutation: None })),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// Immediate Response
// -----------------------------------------------------------------------------

/// Wrap an `ImmediateResponse` in a [`ProcessingResponse`].
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub(crate) fn immediate(imm: ImmediateResponse) -> ProcessingResponse {
    ProcessingResponse {
        response: Some(Response::ImmediateResponse(imm)),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// Body Chunking
// -----------------------------------------------------------------------------

/// Split body bytes into chunks of 62 KiB (the Envoy safety limit).
///
/// Returns a `Vec` of `(chunk, end_of_stream)` pairs. The last chunk
/// has `end_of_stream` set to `true`.
pub(crate) fn chunk_body(data: &[u8]) -> Vec<(&[u8], bool)> {
    if data.is_empty() {
        return vec![(data, true)];
    }

    let mut chunks = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let end = (offset + BODY_CHUNK_LIMIT).min(data.len());
        let eos = end == data.len();
        if let Some(slice) = data.get(offset..end) {
            chunks.push((slice, eos));
        }
        offset = end;
    }

    chunks
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Build body response(s) with optional header mutation and body data.
///
/// When body data is present, populates `body_mutation` so Envoy
/// applies the filter-modified body. Large bodies are split into
/// chunks at the [`BODY_CHUNK_LIMIT`] boundary.
fn body_responses(
    body: Option<&[u8]>,
    mutation: Option<HeaderMutation>,
    is_request: bool,
    body_mode: BodyMode,
) -> Vec<ProcessingResponse> {
    #[expect(
        clippy::single_match_else,
        reason = "Preparing for future implementation of different body modes from Envoy"
    )]
    match body_mode {
        BodyMode::FullDuplexStreamed => body_responses_streamed(body, mutation, is_request),
        _ => {
            // BUFFERED mode (and others): use BodyMutation::Body for full replacement
            let body_mutation = body.filter(|b| !b.is_empty()).map(make_body_mutation);

            let common = CommonResponse {
                status: ResponseStatus::Continue.into(),
                header_mutation: mutation,
                body_mutation,
                ..Default::default()
            };

            vec![wrap_body_response(common, is_request)]
        },
    }
}

/// Build a [`BodyMutation`] replacing the body with the given bytes.
fn make_body_mutation(data: &[u8]) -> BodyMutation {
    BodyMutation {
        mutation: Some(body_mutation::Mutation::Body(data.to_vec())),
    }
}

/// Build streamed body responses using `StreamedBodyResponse` wire format.
///
/// Chunks the body at 62 KiB boundaries and returns multiple responses,
/// each with `end_of_stream` set on the final chunk only.
fn body_responses_streamed(
    body: Option<&[u8]>,
    mutation: Option<HeaderMutation>,
    is_request: bool,
) -> Vec<ProcessingResponse> {
    let Some(data) = body.filter(|b| !b.is_empty()) else {
        // No body or empty: single continue with header mutation only
        return vec![wrap_body_response(
            CommonResponse {
                status: ResponseStatus::Continue.into(),
                header_mutation: mutation,
                body_mutation: None,
                ..Default::default()
            },
            is_request,
        )];
    };

    let chunks = chunk_body(data);
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, (chunk, eos))| {
            make_streamed_response(chunk, eos, if i == 0 { mutation.clone() } else { None }, is_request)
        })
        .collect()
}

/// Build a single streamed body response with chunk data.
fn make_streamed_response(
    chunk: &[u8],
    end_of_stream: bool,
    header_mutation: Option<HeaderMutation>,
    is_request: bool,
) -> ProcessingResponse {
    let streamed = StreamedBodyResponse {
        body: chunk.to_vec(),
        end_of_stream,
    };

    let body_mutation = Some(BodyMutation {
        mutation: Some(body_mutation::Mutation::StreamedResponse(streamed)),
    });

    wrap_body_response(
        CommonResponse {
            status: ResponseStatus::Continue.into(),
            header_mutation,
            body_mutation,
            ..Default::default()
        },
        is_request,
    )
}

/// Wrap a [`CommonResponse`] as either request or response body.
fn wrap_body_response(common: CommonResponse, is_request: bool) -> ProcessingResponse {
    let response = if is_request {
        Response::RequestBody(BodyResponse { response: Some(common) })
    } else {
        Response::ResponseBody(BodyResponse { response: Some(common) })
    };

    ProcessingResponse {
        response: Some(response),
        ..Default::default()
    }
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn chunk_body_empty() {
        let chunks = chunk_body(&[]);

        assert_eq!(chunks.len(), 1, "empty body should produce one chunk");
        assert!(chunks[0].0.is_empty(), "chunk should be empty");
        assert!(chunks[0].1, "single chunk should be EOS");
    }

    #[test]
    fn chunk_body_small() {
        let data = vec![0_u8; 100];
        let chunks = chunk_body(&data);

        assert_eq!(chunks.len(), 1, "small body should produce one chunk");
        assert_eq!(chunks[0].0.len(), 100, "chunk should contain all data");
        assert!(chunks[0].1, "single chunk should be EOS");
    }

    #[test]
    fn chunk_body_exact_boundary() {
        let data = vec![0_u8; BODY_CHUNK_LIMIT];
        let chunks = chunk_body(&data);

        assert_eq!(chunks.len(), 1, "exact boundary should produce one chunk");
        assert!(chunks[0].1, "single chunk should be EOS");
    }

    #[test]
    fn chunk_body_exceeds_boundary() {
        let data = vec![0_u8; BODY_CHUNK_LIMIT + 1];
        let chunks = chunk_body(&data);

        assert_eq!(chunks.len(), 2, "should split into two chunks");
        assert_eq!(chunks[0].0.len(), BODY_CHUNK_LIMIT, "first chunk at limit");
        assert!(!chunks[0].1, "first chunk is not EOS");
        assert_eq!(chunks[1].0.len(), 1, "second chunk has remainder");
        assert!(chunks[1].1, "second chunk is EOS");
    }

    #[test]
    fn chunk_body_multiple_chunks() {
        let size = BODY_CHUNK_LIMIT * 3 + 42;
        let data = vec![0_u8; size];
        let chunks = chunk_body(&data);

        assert_eq!(chunks.len(), 4, "should split into four chunks");

        for (i, (chunk, eos)) in chunks.iter().enumerate() {
            if i < 3 {
                assert_eq!(chunk.len(), BODY_CHUNK_LIMIT, "full chunk at index {i}");
                assert!(!eos, "non-final chunk at index {i} should not be EOS");
            } else {
                assert_eq!(chunk.len(), 42, "last chunk has remainder");
                assert!(eos, "last chunk should be EOS");
            }
        }
    }

    #[test]
    fn request_headers_response_with_mutation() {
        let mutation = HeaderMutation {
            set_headers: vec![],
            remove_headers: vec!["x-remove".to_owned()],
        };
        let resp = request_headers(Some(mutation));

        assert!(resp.response.is_some(), "response should be present");
    }

    #[test]
    fn request_headers_response_without_mutation() {
        let resp = request_headers(None);

        assert!(resp.response.is_some(), "response should be present");
    }

    #[test]
    fn immediate_wraps_correctly() {
        use praxis_proto::envoy::service::common::v3::HttpStatus;

        let imm = ImmediateResponse {
            status: Some(HttpStatus { code: 403 }),
            body: "forbidden".to_owned(),
            ..Default::default()
        };
        let resp = immediate(imm);

        assert!(
            matches!(resp.response, Some(Response::ImmediateResponse(_))),
            "should wrap as ImmediateResponse"
        );
    }

    #[test]
    fn request_body_no_mutation() {
        let responses = request_body(None, None, BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "no body should produce one response");
    }

    #[test]
    fn request_body_with_data() {
        let data = vec![0_u8; 100];
        let responses = request_body(Some(&data), None, BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "should produce single body response");
    }

    #[test]
    fn response_body_no_mutation() {
        let responses = response_body(None, None, BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "no body should produce one response");
        assert!(
            matches!(responses[0].response, Some(Response::ResponseBody(_))),
            "should be ResponseBody variant"
        );
    }

    #[test]
    fn response_body_with_data() {
        let data = vec![0_u8; 200];
        let responses = response_body(Some(&data), None, BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "should produce single body response");
        assert!(
            matches!(responses[0].response, Some(Response::ResponseBody(_))),
            "should be ResponseBody"
        );
    }

    #[test]
    fn response_headers_with_mutation() {
        let mutation = HeaderMutation {
            set_headers: vec![],
            remove_headers: vec!["x-internal".to_owned()],
        };
        let resp = response_headers(Some(mutation));

        assert!(
            matches!(resp.response, Some(Response::ResponseHeaders(_))),
            "should be ResponseHeaders variant"
        );
    }

    #[test]
    fn response_headers_without_mutation() {
        let resp = response_headers(None);

        assert!(
            matches!(resp.response, Some(Response::ResponseHeaders(_))),
            "should be ResponseHeaders variant"
        );
    }

    #[test]
    fn request_trailers_response() {
        let resp = request_trailers();

        assert!(
            matches!(resp.response, Some(Response::RequestTrailers(_))),
            "should be RequestTrailers variant"
        );
    }

    #[test]
    fn response_trailers_response() {
        let resp = response_trailers();

        assert!(
            matches!(resp.response, Some(Response::ResponseTrailers(_))),
            "should be ResponseTrailers variant"
        );
    }

    #[test]
    fn request_body_with_mutation_and_data() {
        let mutation = HeaderMutation {
            set_headers: vec![],
            remove_headers: vec!["x-strip".to_owned()],
        };
        let data = vec![0_u8; 50];
        let responses = request_body(Some(&data), Some(mutation), BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "should produce single body response with mutation");
    }

    #[test]
    fn large_body_single_response() {
        let data = vec![0_u8; BODY_CHUNK_LIMIT * 2 + 100];
        let responses = request_body(Some(&data), None, BodyMode::Buffered);

        assert_eq!(
            responses.len(),
            1,
            "large body should produce single response with body replacement"
        );
    }

    #[test]
    fn request_body_includes_body_mutation() {
        let data = b"mutated body content";
        let responses = request_body(Some(data), None, BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "should produce one response");

        let body_mut = extract_body_mutation(&responses[0]);
        assert!(body_mut.is_some(), "body_mutation should be populated");

        match body_mut.unwrap() {
            body_mutation::Mutation::Body(bytes) => {
                assert_eq!(bytes, data, "body_mutation should contain the provided body data");
            },
            other => panic!("expected Body variant, got {other:?}"),
        }
    }

    #[test]
    fn response_body_includes_body_mutation() {
        let data = b"response body data";
        let responses = response_body(Some(data), None, BodyMode::Buffered);

        let body_mut = extract_body_mutation(&responses[0]);
        assert!(body_mut.is_some(), "response body_mutation should be populated");
    }

    #[test]
    fn empty_body_has_no_body_mutation() {
        let responses = request_body(Some(&[]), None, BodyMode::Buffered);

        let body_mut = extract_body_mutation(&responses[0]);
        assert!(body_mut.is_none(), "empty body should not produce body_mutation");
    }

    #[test]
    fn none_body_has_no_body_mutation() {
        let responses = request_body(None, None, BodyMode::Buffered);

        let body_mut = extract_body_mutation(&responses[0]);
        assert!(body_mut.is_none(), "None body should not produce body_mutation");
    }

    #[test]
    fn streamed_mode_single_chunk() {
        let data = vec![0_u8; 100];
        let responses = request_body(Some(&data), None, BodyMode::FullDuplexStreamed);

        assert_eq!(responses.len(), 1, "small body should produce one streamed response");

        let body_mut = extract_body_mutation(&responses[0]);
        assert!(body_mut.is_some(), "streamed response should have body_mutation");

        match body_mut.unwrap() {
            body_mutation::Mutation::StreamedResponse(s) => {
                assert_eq!(s.body.len(), 100, "chunk should contain all data");
                assert!(s.end_of_stream, "single chunk should be EOS");
            },
            other => panic!("expected StreamedResponse variant, got {other:?}"),
        }
    }

    #[test]
    fn streamed_mode_multiple_chunks() {
        let size = BODY_CHUNK_LIMIT * 2 + 50;
        let data = vec![0_u8; size];
        let responses = request_body(Some(&data), None, BodyMode::FullDuplexStreamed);

        assert_eq!(responses.len(), 3, "large body should produce three streamed responses");

        for (i, resp) in responses.iter().enumerate() {
            let body_mut = extract_body_mutation(resp);
            assert!(body_mut.is_some(), "chunk {i} should have body_mutation");

            match body_mut.unwrap() {
                body_mutation::Mutation::StreamedResponse(s) => {
                    if i < 2 {
                        assert_eq!(s.body.len(), BODY_CHUNK_LIMIT, "chunk {i} should be full size");
                        assert!(!s.end_of_stream, "chunk {i} should not be EOS");
                    } else {
                        assert_eq!(s.body.len(), 50, "last chunk should have remainder");
                        assert!(s.end_of_stream, "last chunk should be EOS");
                    }
                },
                other => panic!("expected StreamedResponse variant, got {other:?}"),
            }
        }
    }

    #[test]
    fn streamed_mode_with_header_mutation() {
        let mutation = HeaderMutation {
            set_headers: vec![],
            remove_headers: vec!["x-internal".to_owned()],
        };
        let data = vec![0_u8; BODY_CHUNK_LIMIT + 100];
        let responses = request_body(Some(&data), Some(mutation), BodyMode::FullDuplexStreamed);

        assert_eq!(responses.len(), 2, "should produce two streamed responses");

        // First chunk should have header mutation
        let first = &responses[0];
        assert!(
            matches!(&first.response, Some(Response::RequestBody(b)) if b.response.as_ref()
                .and_then(|c| c.header_mutation.as_ref())
                .is_some()),
            "first chunk should include header mutation"
        );

        // Second chunk should not
        let second = &responses[1];
        assert!(
            matches!(&second.response, Some(Response::RequestBody(b)) if b.response.as_ref()
                .and_then(|c| c.header_mutation.as_ref())
                .is_none()),
            "second chunk should not have header mutation"
        );
    }

    #[test]
    fn streamed_mode_empty_body() {
        let responses = request_body(Some(&[]), None, BodyMode::FullDuplexStreamed);

        assert_eq!(responses.len(), 1, "empty body should produce one response");

        let body_mut = extract_body_mutation(&responses[0]);
        assert!(body_mut.is_none(), "empty body should not have body_mutation");
    }

    #[test]
    fn buffered_mode_preserves_behavior() {
        let data = vec![0_u8; BODY_CHUNK_LIMIT + 100];
        let responses = request_body(Some(&data), None, BodyMode::Buffered);

        assert_eq!(responses.len(), 1, "BUFFERED mode should produce single response");

        let body_mut = extract_body_mutation(&responses[0]);
        match body_mut.unwrap() {
            body_mutation::Mutation::Body(bytes) => {
                assert_eq!(bytes.len(), data.len(), "BUFFERED should send full body");
            },
            other => panic!("BUFFERED should use Body variant, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    fn extract_body_mutation(resp: &ProcessingResponse) -> Option<&body_mutation::Mutation> {
        match &resp.response {
            Some(Response::RequestBody(b)) => b
                .response
                .as_ref()
                .and_then(|c| c.body_mutation.as_ref())
                .and_then(|bm| bm.mutation.as_ref()),
            Some(Response::ResponseBody(b)) => b
                .response
                .as_ref()
                .and_then(|c| c.body_mutation.as_ref())
                .and_then(|bm| bm.mutation.as_ref()),
            _ => None,
        }
    }
}
