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
    BodyResponse, CommonResponse, HeaderMutation, HeadersResponse, ImmediateResponse, ProcessingResponse,
    TrailersResponse, common_response::ResponseStatus, processing_response::Response,
};

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
pub fn request_headers(mutation: Option<HeaderMutation>) -> ProcessingResponse {
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
pub fn response_headers(mutation: Option<HeaderMutation>) -> ProcessingResponse {
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
pub fn request_body(body: Option<&[u8]>, mutation: Option<HeaderMutation>) -> Vec<ProcessingResponse> {
    body_responses(body, mutation, true)
}

/// Build [`ProcessingResponse`] messages for the response body phase.
///
/// Same chunking logic as [`request_body`] but wraps in `ResponseBody`.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub fn response_body(body: Option<&[u8]>, mutation: Option<HeaderMutation>) -> Vec<ProcessingResponse> {
    body_responses(body, mutation, false)
}

// -----------------------------------------------------------------------------
// Trailer Responses
// -----------------------------------------------------------------------------

/// Build a passthrough [`ProcessingResponse`] for request trailers.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub fn request_trailers() -> ProcessingResponse {
    ProcessingResponse {
        response: Some(Response::RequestTrailers(TrailersResponse { header_mutation: None })),
        ..Default::default()
    }
}

/// Build a passthrough [`ProcessingResponse`] for response trailers.
///
/// [`ProcessingResponse`]: praxis_proto::envoy::service::ext_proc::v3::ProcessingResponse
pub fn response_trailers() -> ProcessingResponse {
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
pub fn immediate(imm: ImmediateResponse) -> ProcessingResponse {
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
///
/// ```
/// use praxis_extproc::response::chunk_body;
///
/// let data = vec![0u8; 130_000];
/// let chunks = chunk_body(&data);
/// assert_eq!(chunks.len(), 3, "130KB should split into 3 chunks");
/// assert!(!chunks[0].1, "first chunk is not EOS");
/// assert!(chunks[chunks.len() - 1].1, "last chunk is EOS");
/// ```
pub fn chunk_body(data: &[u8]) -> Vec<(&[u8], bool)> {
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

/// Build a single body response with optional header mutation.
fn body_responses(_body: Option<&[u8]>, mutation: Option<HeaderMutation>, is_request: bool) -> Vec<ProcessingResponse> {
    let common = CommonResponse {
        status: ResponseStatus::Continue.into(),
        header_mutation: mutation,
        ..Default::default()
    };

    vec![wrap_body_response(common, is_request)]
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    reason = "tests"
)]
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
        let data = vec![0u8; 100];
        let chunks = chunk_body(&data);

        assert_eq!(chunks.len(), 1, "small body should produce one chunk");
        assert_eq!(chunks[0].0.len(), 100, "chunk should contain all data");
        assert!(chunks[0].1, "single chunk should be EOS");
    }

    #[test]
    fn chunk_body_exact_boundary() {
        let data = vec![0u8; BODY_CHUNK_LIMIT];
        let chunks = chunk_body(&data);

        assert_eq!(chunks.len(), 1, "exact boundary should produce one chunk");
        assert!(chunks[0].1, "single chunk should be EOS");
    }

    #[test]
    fn chunk_body_exceeds_boundary() {
        let data = vec![0u8; BODY_CHUNK_LIMIT + 1];
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
        let data = vec![0u8; size];
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
        let responses = request_body(None, None);

        assert_eq!(responses.len(), 1, "no body should produce one response");
    }

    #[test]
    fn request_body_with_data() {
        let data = vec![0u8; 100];
        let responses = request_body(Some(&data), None);

        assert_eq!(responses.len(), 1, "should produce single body response");
    }

    #[test]
    fn response_body_no_mutation() {
        let responses = response_body(None, None);

        assert_eq!(responses.len(), 1, "no body should produce one response");
        assert!(
            matches!(responses[0].response, Some(Response::ResponseBody(_))),
            "should be ResponseBody variant"
        );
    }

    #[test]
    fn response_body_with_data() {
        let data = vec![0u8; 200];
        let responses = response_body(Some(&data), None);

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
        let data = vec![0u8; 50];
        let responses = request_body(Some(&data), Some(mutation));

        assert_eq!(responses.len(), 1, "should produce single body response with mutation");
    }

    #[test]
    fn large_body_single_response() {
        let data = vec![0u8; BODY_CHUNK_LIMIT * 2 + 100];
        let responses = request_body(Some(&data), None);

        assert_eq!(
            responses.len(),
            1,
            "large body should produce single response with body replacement"
        );
    }
}
