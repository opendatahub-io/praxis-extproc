// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Shane Utt

//! Translation between Envoy ExtProc types and Praxis filter types.
//!
//! Converts Envoy `HttpHeaders` into [`Request`], builds
//! [`HttpFilterContext`], and extracts header mutations from context
//! after pipeline execution.
//!
//! [`Request`]: praxis_filter::Request
//! [`HttpFilterContext`]: praxis_filter::HttpFilterContext

use std::{collections::HashMap, net::IpAddr, time::Instant};

use http::{HeaderMap, Method, StatusCode, Uri};
use praxis_filter::{BodyMode, HttpFilterContext, Request, Response};
use praxis_proto::envoy::service::{
    common::v3::{HeaderValue, HeaderValueOption, HttpStatus},
    ext_proc::v3::{HeaderMutation, ImmediateResponse},
};

// -----------------------------------------------------------------------------
// Header Conversion
// -----------------------------------------------------------------------------

/// Convert ExtProc [`HeaderValue`] list into a Praxis [`Request`].
///
/// Pseudo-headers (`:method`, `:path`, `:authority`, `:scheme`) are
/// extracted into their respective fields. Remaining headers populate
/// the [`HeaderMap`].
///
/// [`HeaderValue`]: praxis_proto::envoy::service::common::v3::HeaderValue
/// [`Request`]: praxis_filter::Request
/// [`HeaderMap`]: http::HeaderMap
pub fn envoy_headers_to_request(headers: &[HeaderValue]) -> Request {
    let mut method = Method::GET;
    let mut path = "/".to_owned();
    let mut header_map = HeaderMap::new();

    for hv in headers {
        let val = header_value_str(hv);
        match hv.key.as_str() {
            ":method" => method = val.parse().unwrap_or(Method::GET),
            ":path" => path = val.to_owned(),
            ":authority" | ":scheme" => {},
            key => {
                if let (Ok(name), Ok(value)) = (
                    key.parse::<http::header::HeaderName>(),
                    val.parse::<http::header::HeaderValue>(),
                ) {
                    header_map.append(name, value);
                }
            },
        }
    }

    let uri = path.parse().unwrap_or_else(|_| Uri::from_static("/"));

    Request {
        headers: header_map,
        method,
        uri,
    }
}

// -----------------------------------------------------------------------------
// Context Construction
// -----------------------------------------------------------------------------

/// Build a minimal [`HttpFilterContext`] from a converted [`Request`].
///
/// Populates `client_addr` from the `x-forwarded-for` header if present.
/// All routing fields (`cluster`, `upstream`) default to `None`; they are
/// advisory in ExtProc mode since Envoy owns routing.
///
/// [`HttpFilterContext`]: praxis_filter::HttpFilterContext
/// [`Request`]: praxis_filter::Request
pub fn build_filter_context<'a>(request: &'a Request) -> HttpFilterContext<'a> {
    let client_addr = extract_client_addr(request);

    HttpFilterContext {
        body_done_indices: Vec::new(),
        branch_iterations: HashMap::new(),
        client_addr,
        cluster: None,
        downstream_tls: false,
        executed_filter_indices: Vec::new(),
        extra_request_headers: Vec::new(),
        filter_metadata: HashMap::new(),
        filter_results: HashMap::new(),
        health_registry: None,
        kv_stores: None,
        request,
        request_body_bytes: 0,
        request_body_mode: BodyMode::Stream,
        request_start: Instant::now(),
        response_body_bytes: 0,
        response_body_mode: BodyMode::Stream,
        response_header: None,
        response_headers_modified: false,
        rewritten_path: None,
        selected_endpoint_index: None,
        upstream: None,
    }
}

// -----------------------------------------------------------------------------
// Mutation Collection
// -----------------------------------------------------------------------------

/// Collect header mutations from `ctx.extra_request_headers` and
/// `ctx.rewritten_path` into a [`HeaderMutation`].
///
/// When `rewritten_path` is set, emits a `:path` mutation so Envoy
/// forwards the rewritten URI to the upstream.
///
/// Returns `None` when there are no mutations to apply.
///
/// [`HeaderMutation`]: praxis_proto::envoy::service::ext_proc::v3::HeaderMutation
pub fn collect_request_header_mutations(ctx: &HttpFilterContext<'_>) -> Option<HeaderMutation> {
    let has_extras = !ctx.extra_request_headers.is_empty();
    let has_rewrite = ctx.rewritten_path.is_some();

    if !has_extras && !has_rewrite {
        return None;
    }

    let mut set_headers: Vec<HeaderValueOption> = ctx
        .extra_request_headers
        .iter()
        .map(|(name, value)| header_value_option(name, value))
        .collect();

    if let Some(path) = &ctx.rewritten_path {
        set_headers.push(header_value_option(":path", path));
    }

    Some(HeaderMutation {
        set_headers,
        remove_headers: Vec::new(),
    })
}

/// Collect response header mutations by diffing against original headers.
///
/// `original_keys` is the set of header names present before filters ran.
/// Headers present after but not before are mutations to send to Envoy.
///
/// [`HeaderMutation`]: praxis_proto::envoy::service::ext_proc::v3::HeaderMutation
pub fn collect_response_header_mutations_diff(
    ctx: &HttpFilterContext<'_>,
    original_keys: &std::collections::HashSet<String>,
) -> Option<HeaderMutation> {
    let resp = ctx.response_header.as_ref()?;

    let set_headers: Vec<HeaderValueOption> = resp
        .headers
        .iter()
        .filter(|(name, _)| !original_keys.contains(name.as_str()))
        .map(|(name, value)| header_value_option(name.as_str(), value.to_str().unwrap_or_default()))
        .collect();

    if set_headers.is_empty() {
        return None;
    }

    Some(HeaderMutation {
        set_headers,
        remove_headers: Vec::new(),
    })
}

// -----------------------------------------------------------------------------
// Rejection Conversion
// -----------------------------------------------------------------------------

/// Convert a [`Rejection`] into an ExtProc [`ImmediateResponse`].
///
/// Maps status code, headers, and body from the Praxis rejection
/// to the ExtProc immediate response format.
///
/// [`Rejection`]: praxis_filter::Rejection
/// [`ImmediateResponse`]: praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse
pub fn rejection_to_immediate(rejection: &praxis_filter::Rejection) -> ImmediateResponse {
    let headers = if rejection.headers.is_empty() {
        None
    } else {
        Some(rejection_headers_to_mutation(&rejection.headers))
    };

    let body = rejection
        .body
        .as_ref()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default();

    ImmediateResponse {
        status: Some(HttpStatus {
            code: i32::from(rejection.status),
        }),
        headers,
        body,
        grpc_status: None,
        details: String::new(),
    }
}

/// Build a [`Response`] from ExtProc response headers.
///
/// Extracts `:status` pseudo-header for the status code; remaining
/// headers populate the [`HeaderMap`].
///
/// [`Response`]: praxis_filter::Response
/// [`HeaderMap`]: http::HeaderMap
pub fn envoy_headers_to_response(headers: &[HeaderValue]) -> Response {
    let mut status = StatusCode::OK;
    let mut header_map = HeaderMap::new();

    for hv in headers {
        let val = header_value_str(hv);
        if hv.key == ":status" {
            status = val
                .parse::<u16>()
                .ok()
                .and_then(|c| StatusCode::from_u16(c).ok())
                .unwrap_or(StatusCode::OK);
        } else if let (Ok(name), Ok(value)) = (
            hv.key.parse::<http::header::HeaderName>(),
            val.parse::<http::header::HeaderValue>(),
        ) {
            header_map.append(name, value);
        }
    }

    Response {
        headers: header_map,
        status,
    }
}

// -----------------------------------------------------------------------------
// Utilities
// -----------------------------------------------------------------------------

/// Extract string value from a [`HeaderValue`], preferring `raw_value`.
fn header_value_str(hv: &HeaderValue) -> &str {
    if hv.raw_value.is_empty() {
        &hv.value
    } else {
        std::str::from_utf8(&hv.raw_value).unwrap_or(&hv.value)
    }
}

/// Extract client IP from the `x-forwarded-for` header.
fn extract_client_addr(request: &Request) -> Option<IpAddr> {
    request
        .headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
}

/// Build a single [`HeaderValueOption`] from key and value strings.
///
/// Sets both `value` and `raw_value` for maximum compatibility
/// across Envoy versions.
fn header_value_option(key: &str, value: &str) -> HeaderValueOption {
    HeaderValueOption {
        header: Some(HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: value.as_bytes().to_vec(),
        }),
        ..Default::default()
    }
}

/// Convert rejection header pairs to a [`HeaderMutation`].
fn rejection_headers_to_mutation(headers: &[(String, String)]) -> HeaderMutation {
    let set_headers = headers
        .iter()
        .map(|(name, value)| header_value_option(name, value))
        .collect();

    HeaderMutation {
        set_headers,
        remove_headers: Vec::new(),
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
    use bytes::Bytes;

    use super::*;

    #[test]
    fn convert_basic_get_request() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/api/users"),
            make_header(":authority", "example.com"),
            make_header("accept", "application/json"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(req.method, Method::GET, "method should be GET");
        assert_eq!(req.uri.path(), "/api/users", "path should match");
        assert_eq!(
            req.headers.get("accept").and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "accept header should be preserved"
        );
    }

    #[test]
    fn convert_post_request() {
        let headers = vec![make_header(":method", "POST"), make_header(":path", "/submit")];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(req.method, Method::POST, "method should be POST");
    }

    #[test]
    fn missing_method_defaults_to_get() {
        let headers = vec![make_header(":path", "/")];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(req.method, Method::GET, "should default to GET");
    }

    #[test]
    fn missing_path_defaults_to_root() {
        let headers = vec![make_header(":method", "GET")];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(req.uri.path(), "/", "should default to /");
    }

    #[test]
    fn pseudo_headers_excluded_from_header_map() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header(":authority", "example.com"),
            make_header(":scheme", "https"),
            make_header("x-custom", "value"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert!(req.headers.get(":method").is_none(), ":method should not be in headers");
        assert!(req.headers.get(":path").is_none(), ":path should not be in headers");
        assert!(
            req.headers.get("x-custom").is_some(),
            "regular headers should be preserved"
        );
    }

    #[test]
    fn build_context_defaults() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let ctx = build_filter_context(&req);

        assert!(ctx.client_addr.is_none(), "client_addr should be None without XFF");
        assert!(ctx.cluster.is_none(), "cluster should be None");
        assert!(ctx.upstream.is_none(), "upstream should be None");
    }

    #[test]
    fn build_context_extracts_client_ip_from_xff() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header("x-forwarded-for", "10.0.0.1, 172.16.0.1"),
        ];
        let req = envoy_headers_to_request(&headers);
        let ctx = build_filter_context(&req);

        assert_eq!(
            ctx.client_addr,
            Some("10.0.0.1".parse().unwrap()),
            "should extract first IP from XFF"
        );
    }

    #[test]
    fn collect_mutations_empty_when_no_extras() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let ctx = build_filter_context(&req);

        assert!(
            collect_request_header_mutations(&ctx).is_none(),
            "no mutations when empty"
        );
    }

    #[test]
    fn collect_mutations_from_extra_headers() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(&req);
        ctx.extra_request_headers.push(("x-added".into(), "value".to_owned()));

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "should have one set header");
        assert_eq!(
            mutation.set_headers[0].header.as_ref().unwrap().key,
            "x-added",
            "key should match"
        );
    }

    #[test]
    fn collect_mutations_includes_rewritten_path() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/old")]);
        let mut ctx = build_filter_context(&req);
        ctx.rewritten_path = Some("/new/path".to_owned());

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        let path_header = mutation
            .set_headers
            .iter()
            .find(|h| h.header.as_ref().is_some_and(|hv| hv.key == ":path"));
        assert!(path_header.is_some(), ":path mutation should be present");
        assert_eq!(
            path_header.unwrap().header.as_ref().unwrap().value,
            "/new/path",
            ":path value should match rewritten path"
        );
    }

    #[test]
    fn collect_mutations_rewritten_path_only() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(&req);
        ctx.rewritten_path = Some("/rewritten".to_owned());

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "only :path mutation");
    }

    #[test]
    fn rejection_to_immediate_basic() {
        let rejection = praxis_filter::Rejection::status(403);
        let imm = rejection_to_immediate(&rejection);

        assert_eq!(imm.status.unwrap().code, 403, "status should be 403");
        assert!(imm.headers.is_none(), "no headers on basic rejection");
        assert!(imm.body.is_empty(), "no body on basic rejection");
    }

    #[test]
    fn rejection_to_immediate_with_body_and_headers() {
        let rejection = praxis_filter::Rejection::status(429)
            .with_header("Retry-After", "60")
            .with_body(Bytes::from_static(b"rate limited"));
        let imm = rejection_to_immediate(&rejection);

        assert_eq!(imm.status.unwrap().code, 429, "status should be 429");
        assert_eq!(imm.body, "rate limited", "body should match");

        let hdrs = imm.headers.unwrap();
        assert_eq!(hdrs.set_headers.len(), 1, "should have one header");
        assert_eq!(
            hdrs.set_headers[0].header.as_ref().unwrap().key,
            "Retry-After",
            "header key should match"
        );
    }

    #[test]
    fn convert_response_headers() {
        let headers = vec![
            make_header(":status", "201"),
            make_header("content-type", "application/json"),
        ];

        let resp = envoy_headers_to_response(&headers);

        assert_eq!(resp.status, StatusCode::CREATED, "status should be 201");
        assert_eq!(
            resp.headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/json"),
            "content-type should be preserved"
        );
    }

    #[test]
    fn convert_response_missing_status_defaults_ok() {
        let headers = vec![make_header("x-custom", "value")];

        let resp = envoy_headers_to_response(&headers);

        assert_eq!(resp.status, StatusCode::OK, "should default to 200");
    }

    // -----------------------------------------------------------------------------
    // Test Utilities
    // -----------------------------------------------------------------------------

    fn make_header(key: &str, value: &str) -> HeaderValue {
        HeaderValue {
            key: key.to_owned(),
            value: value.to_owned(),
            raw_value: Vec::new(),
        }
    }
}
