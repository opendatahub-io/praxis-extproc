// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

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
use praxis_filter::{
    BodyMode, FilterPipeline, HttpFilterContext, Request, RequestExtensions, Response, SubRequestResponseMode,
};
use praxis_proto::envoy::service::{
    common::v3::{HeaderValue, HeaderValueOption, HttpStatus, header_value_option::HeaderAppendAction},
    ext_proc::v3::{HeaderMutation, ImmediateResponse},
};
use tracing::debug;

// -----------------------------------------------------------------------------
// Header Conversion
// -----------------------------------------------------------------------------

/// Convert ExtProc [`HeaderValue`] list into a Praxis [`Request`].
///
/// Pseudo-headers are extracted: `:method` and `:path` into their fields,
/// `:scheme` and `:authority` into an absolute-form URI. Envoy carries the
/// host only as `:authority`, so a `host` header is synthesized from it
/// when the request did not send one; filters then find the host where
/// they would under either HTTP/1.1 (the header) or HTTP/2 (the URI).
/// Remaining headers populate the [`HeaderMap`] with their raw bytes.
///
/// [`HeaderValue`]: praxis_proto::envoy::service::common::v3::HeaderValue
/// [`Request`]: praxis_filter::Request
/// [`HeaderMap`]: http::HeaderMap
pub fn envoy_headers_to_request(headers: &[HeaderValue]) -> Request {
    let mut method = Method::GET;
    let mut path = "/".to_owned();
    let mut scheme = None;
    let mut authority = None;
    let mut header_map = HeaderMap::new();

    for hv in headers {
        match hv.key.as_str() {
            ":method" => method = header_value_str(hv).parse().unwrap_or(Method::GET),
            ":path" => header_value_str(hv).clone_into(&mut path),
            ":scheme" => scheme = Some(header_value_str(hv).to_owned()),
            ":authority" => authority = Some(header_value_str(hv).to_owned()),
            _ => append_header(&mut header_map, hv),
        }
    }

    if let Some(host) = authority.as_deref()
        && !header_map.contains_key(http::header::HOST)
        && let Ok(value) = http::header::HeaderValue::from_str(host)
    {
        header_map.insert(http::header::HOST, value);
    }

    Request {
        headers: header_map,
        method,
        uri: build_uri(scheme.as_deref(), authority.as_deref(), &path),
    }
}

/// Assemble the request URI: absolute-form when Envoy supplied both scheme
/// and authority and the path is origin-form, otherwise the path alone.
fn build_uri(scheme: Option<&str>, authority: Option<&str>, path: &str) -> Uri {
    let absolute = match (scheme, authority) {
        (Some(scheme), Some(authority)) if path.starts_with('/') => {
            format!("{scheme}://{authority}{path}").parse().ok()
        },
        _ => None,
    };

    absolute
        .or_else(|| path.parse().ok())
        .unwrap_or_else(|| Uri::from_static("/"))
}

/// Append a regular header to `map`, keeping opaque value bytes.
///
/// Envoy sends non-UTF-8 values in `raw_value`; they are kept byte for byte
/// rather than replaced by an empty value. A name or value that is not a
/// valid HTTP field is dropped and logged, so it never silently vanishes
/// from what filters inspect.
fn append_header(map: &mut HeaderMap, hv: &HeaderValue) {
    let value = if hv.raw_value.is_empty() {
        http::header::HeaderValue::from_str(&hv.value)
    } else {
        http::header::HeaderValue::from_bytes(&hv.raw_value)
    };

    if let (Ok(name), Ok(value)) = (hv.key.parse::<http::header::HeaderName>(), value) {
        map.append(name, value);
    } else {
        debug!(key = %hv.key, "dropping header that is not a valid HTTP field");
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
#[expect(
    clippy::too_many_lines,
    reason = "HttpFilterContext field init mirrors the struct; splitting obscures defaults"
)]
pub fn build_filter_context<'a>(pipeline: &'a FilterPipeline, request: &'a Request) -> HttpFilterContext<'a> {
    let client_addr = extract_client_addr(request);

    HttpFilterContext {
        buffered_request_body: None,
        body_done_indices: Vec::new(),
        branch_iterations: HashMap::new(),
        client_addr,
        cluster: None,
        current_filter_id: None,
        downstream_tls: false,
        metrics_route: None,
        peer_identity: None,
        extensions: RequestExtensions::default(),
        executed_filter_indices: Vec::new(),
        extra_request_headers: Vec::new(),
        request_headers_to_remove: Vec::new(),
        request_headers_to_set: Vec::new(),
        filter_metadata: HashMap::new(),
        // Envoy owns the upstream call, so gRPC completion is never observed
        // here, and ExtProc has no pre-read passes to carry mutations between.
        grpc_completion: None,
        prior_pre_read_mutations: Vec::new(),
        pre_read_mutations: Vec::new(),
        structured_metadata: HashMap::new(),
        filter_results: HashMap::new(),
        filter_state: HashMap::new(),
        health_registry: pipeline.health_registry(),
        id_generator: pipeline.id_generator(),
        kv_stores: pipeline.kv_stores(),
        session_stores: None,
        subrequest_client: pipeline.subrequest_client(),
        request,
        request_body_bytes: 0,
        request_body_mode: BodyMode::Stream,
        request_start: Instant::now(),
        response_body_bytes: 0,
        response_body_mode: BodyMode::Stream,
        response_header: None,
        response_headers_modified: false,
        // Flipped by the server for response-phase contexts: a response
        // exists only because Envoy reached the upstream.
        upstream_reached: false,
        subrequest_response_mode: SubRequestResponseMode::Buffered,
        attempted_endpoints: Vec::new(),
        retry_policy: None,
        route_retry_policy: None,
        cluster_retry_state: None,
        cluster_retry_state_released: false,
        endpoint_reselector: None,
        pinned_endpoint_address: None,
        selected_endpoint_index: None,
        time_source: pipeline.time_source(),
        rewritten_path: None,
        upstream: None,
    }
}

// -----------------------------------------------------------------------------
// Mutation Collection
// -----------------------------------------------------------------------------

/// Collect header mutations from request-phase context into a [`HeaderMutation`].
///
/// Emits ExtProc mutations for:
/// - `extra_request_headers` (append/inject)
/// - `request_headers_to_set` (overwrite)
/// - `request_headers_to_remove`
/// - `rewritten_path` as a `:path` mutation
///
/// Returns `None` when there are no mutations to apply.
///
/// [`HeaderMutation`]: praxis_proto::envoy::service::ext_proc::v3::HeaderMutation
pub fn collect_request_header_mutations(ctx: &HttpFilterContext<'_>) -> Option<HeaderMutation> {
    let has_extras = !ctx.extra_request_headers.is_empty();
    let has_sets = !ctx.request_headers_to_set.is_empty();
    let has_removes = !ctx.request_headers_to_remove.is_empty();
    let has_rewrite = ctx.rewritten_path.is_some();

    if !has_extras && !has_sets && !has_removes && !has_rewrite {
        return None;
    }

    let mut set_headers: Vec<HeaderValueOption> = ctx
        .extra_request_headers
        .iter()
        .map(|(name, value)| header_value_option_append(name, value))
        .collect();

    set_headers.extend(
        ctx.request_headers_to_set
            .iter()
            .map(|(name, value)| header_value_option(name.as_str(), value.to_str().unwrap_or_default())),
    );

    if let Some(path) = &ctx.rewritten_path {
        set_headers.push(header_value_option(":path", path));
    }

    let remove_headers: Vec<String> = ctx
        .request_headers_to_remove
        .iter()
        .map(|name| name.as_str().to_owned())
        .collect();

    Some(HeaderMutation {
        set_headers,
        remove_headers,
    })
}

/// Collect response header mutations by diffing against original state.
///
/// Compares each header name's complete value list, so multi-valued headers
/// such as `set-cookie` are only touched when a filter actually changed
/// them. Detects three kinds of mutations:
/// - **Added**: names present after but not before filters ran.
/// - **Modified**: names whose value list changed.
/// - **Removed**: names present before but absent after filters ran.
///
/// A changed value list is re-emitted in full: the first value overwrites
/// whatever Envoy holds, the remaining values append to it.
///
/// [`HeaderMutation`]: praxis_proto::envoy::service::ext_proc::v3::HeaderMutation
pub fn collect_response_header_mutations_diff(
    ctx: &HttpFilterContext<'_>,
    original_headers: &HeaderMap,
) -> Option<HeaderMutation> {
    let current = &ctx.response_header.as_ref()?.headers;

    let set_headers: Vec<HeaderValueOption> = current
        .keys()
        .filter(|name| !current.get_all(*name).iter().eq(original_headers.get_all(*name).iter()))
        .flat_map(|name| replace_header_values(name.as_str(), current.get_all(name).iter()))
        .collect();

    let remove_headers: Vec<String> = original_headers
        .keys()
        .filter(|name| !current.contains_key(*name))
        .map(|name| name.as_str().to_owned())
        .collect();

    if set_headers.is_empty() && remove_headers.is_empty() {
        return None;
    }

    Some(HeaderMutation {
        set_headers,
        remove_headers,
    })
}

/// Mutations that make `name` carry exactly `values`, in order.
///
/// The first value overwrites any existing header of that name; the rest
/// append, which is the only way ExtProc can express a multi-valued header.
/// Values travel as raw bytes so opaque (non-UTF-8) header values survive.
fn replace_header_values<'a>(
    name: &'a str,
    values: impl Iterator<Item = &'a http::header::HeaderValue> + 'a,
) -> impl Iterator<Item = HeaderValueOption> + 'a {
    values.enumerate().map(move |(index, value)| {
        let action = if index == 0 {
            HeaderAppendAction::OverwriteIfExistsOrAdd
        } else {
            HeaderAppendAction::AppendIfExistsOrAdd
        };
        header_option(name, value.as_bytes(), action)
    })
}

// -----------------------------------------------------------------------------
// Rejection Conversion
// -----------------------------------------------------------------------------

/// Convert a [`Rejection`] into an ExtProc [`ImmediateResponse`].
///
/// Maps status code, headers (both the string pairs and the
/// byte-preserving header map), and body from the Praxis rejection to
/// the ExtProc immediate response format.
///
/// [`Rejection`]: praxis_filter::Rejection
/// [`ImmediateResponse`]: praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse
pub fn rejection_to_immediate(rejection: &praxis_filter::Rejection) -> ImmediateResponse {
    let headers = rejection
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .chain(rejection.header_map.iter().flat_map(|map| header_map_pairs(map)));

    immediate_response(rejection.status, headers, rejection.body.as_deref())
}

/// Convert a [`TerminalResponse`] into an ExtProc [`ImmediateResponse`].
///
/// A terminal response is a complete reply produced by a request-phase filter
/// (e.g. the iterative router returning an upstream response). Envoy can only
/// deliver it as a local reply, which ends the stream.
///
/// [`TerminalResponse`]: praxis_filter::TerminalResponse
/// [`ImmediateResponse`]: praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse
pub fn terminal_response_to_immediate(terminal: &praxis_filter::TerminalResponse) -> ImmediateResponse {
    immediate_response(
        terminal.status,
        header_map_pairs(&terminal.headers),
        terminal.body.as_deref(),
    )
}

/// Build an [`ImmediateResponse`] from its parts.
///
/// [`ImmediateResponse`]: praxis_proto::envoy::service::ext_proc::v3::ImmediateResponse
fn immediate_response<'a>(
    status: u16,
    headers: impl Iterator<Item = (&'a str, &'a [u8])>,
    body: Option<&[u8]>,
) -> ImmediateResponse {
    let set_headers: Vec<HeaderValueOption> = headers
        .map(|(name, value)| header_option(name, value, HeaderAppendAction::OverwriteIfExistsOrAdd))
        .collect();

    ImmediateResponse {
        status: Some(HttpStatus {
            code: i32::from(status),
        }),
        headers: (!set_headers.is_empty()).then(|| HeaderMutation {
            set_headers,
            remove_headers: Vec::new(),
        }),
        body: body
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default(),
        grpc_status: None,
        details: String::new(),
    }
}

/// Name and raw value bytes of every entry in a [`HeaderMap`].
///
/// [`HeaderMap`]: http::HeaderMap
fn header_map_pairs(map: &HeaderMap) -> impl Iterator<Item = (&str, &[u8])> {
    map.iter().map(|(name, value)| (name.as_str(), value.as_bytes()))
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
        if hv.key == ":status" {
            status = header_value_str(hv)
                .parse::<u16>()
                .ok()
                .and_then(|c| StatusCode::from_u16(c).ok())
                .unwrap_or(StatusCode::OK);
        } else {
            append_header(&mut header_map, hv);
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

/// Extract the client IP for `HttpFilterContext::client_addr`.
///
/// Prefers `x-envoy-external-address`, which Envoy derives from its own
/// trusted-hop configuration and sanitizes on external requests, over the
/// first `x-forwarded-for` entry, which any client can set. The latter
/// remains the fallback for deployments that do not use `use_remote_address`.
fn extract_client_addr(request: &Request) -> Option<IpAddr> {
    first_ip_in_header(request, "x-envoy-external-address").or_else(|| first_ip_in_header(request, "x-forwarded-for"))
}

/// Parse the first comma-separated IP address in a header.
fn first_ip_in_header(request: &Request, name: &str) -> Option<IpAddr> {
    request
        .headers
        .get(name)?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Build a [`HeaderValueOption`] from raw value bytes with the given action.
///
/// Valid UTF-8 is sent in both `value` and `raw_value` for compatibility
/// across Envoy versions; any other bytes go in `raw_value` alone, since
/// `value` is a protobuf string and must not carry them.
fn header_option(key: &str, value: &[u8], append_action: HeaderAppendAction) -> HeaderValueOption {
    HeaderValueOption {
        header: Some(HeaderValue {
            key: key.to_owned(),
            value: std::str::from_utf8(value).map(str::to_owned).unwrap_or_default(),
            raw_value: value.to_vec(),
        }),
        append_action: append_action.into(),
        append: None,
    }
}

/// Build a [`HeaderValueOption`] that overwrites any existing header of the
/// same key (or adds it if absent).
///
/// Correct for single-valued headers (`content-length`, `:path`, `:authority`)
/// and explicit set/replace mutations: without `OverwriteIfExistsOrAdd`, Envoy
/// would append the new value alongside an original the client already sent,
/// producing an invalid multi-valued header.
fn header_value_option(key: &str, value: &str) -> HeaderValueOption {
    header_option(key, value.as_bytes(), HeaderAppendAction::OverwriteIfExistsOrAdd)
}

/// Build a [`HeaderValueOption`] that appends to any existing header of the
/// same key (protobuf default `APPEND_IF_EXISTS_OR_ADD`).
///
/// Used for injected extra headers, where a filter may legitimately add a
/// value alongside one the client already sent.
fn header_value_option_append(key: &str, value: &str) -> HeaderValueOption {
    header_option(key, value.as_bytes(), HeaderAppendAction::AppendIfExistsOrAdd)
}

/// Overwrite `content-length` on a header mutation to `len` bytes.
///
/// Creates the mutation if absent and drops any prior `content-length`
/// entry so the declared size matches the body actually emitted.
pub(crate) fn set_content_length(mutation: Option<HeaderMutation>, len: usize) -> HeaderMutation {
    let mut mutation = mutation.unwrap_or_default();
    mutation.set_headers.retain(|h| {
        h.header
            .as_ref()
            .is_none_or(|hv| !hv.key.eq_ignore_ascii_case("content-length"))
    });
    mutation
        .set_headers
        .push(header_value_option("content-length", &len.to_string()));
    mutation
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, reason = "tests")]
mod tests {
    use std::sync::LazyLock;

    use bytes::Bytes;
    use praxis_filter::FilterRegistry;

    use super::*;

    static TEST_PIPELINE: LazyLock<FilterPipeline> =
        LazyLock::new(|| FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).expect("empty pipeline"));

    fn test_pipeline() -> &'static FilterPipeline {
        &TEST_PIPELINE
    }

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
    fn malformed_path_falls_back_to_root() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "not a valid uri !!!"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(
            req.uri.path(),
            "/",
            "malformed path should fall back to / instead of panicking"
        );
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
    fn authority_and_scheme_populate_host_and_uri() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/api?x=1"),
            make_header(":authority", "example.com:8443"),
            make_header(":scheme", "https"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(
            req.headers.get("host").and_then(|v| v.to_str().ok()),
            Some("example.com:8443"),
            "host header is synthesized from :authority"
        );
        assert_eq!(req.uri.scheme_str(), Some("https"), "scheme carried into the URI");
        assert_eq!(
            req.uri.authority().map(http::uri::Authority::as_str),
            Some("example.com:8443"),
            "authority carried into the URI"
        );
        assert_eq!(req.uri.path(), "/api", "path unchanged");
        assert_eq!(req.uri.query(), Some("x=1"), "query unchanged");
    }

    #[test]
    fn explicit_host_header_wins_over_authority() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header(":authority", "proxy.internal"),
            make_header("host", "client-sent.example"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(
            req.headers.get("host").and_then(|v| v.to_str().ok()),
            Some("client-sent.example"),
            "a host header the client sent is not overwritten"
        );
        assert_eq!(req.headers.get_all("host").iter().count(), 1, "host is not duplicated");
    }

    #[test]
    fn asterisk_form_path_stays_parseable() {
        let headers = vec![
            make_header(":method", "OPTIONS"),
            make_header(":path", "*"),
            make_header(":authority", "example.com"),
            make_header(":scheme", "http"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(req.uri.path(), "*", "asterisk-form request target is kept");
    }

    #[test]
    fn opaque_header_bytes_are_preserved() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            HeaderValue {
                key: "x-raw".to_owned(),
                value: String::new(),
                raw_value: b"caf\xe9".to_vec(),
            },
        ];

        let req = envoy_headers_to_request(&headers);

        assert_eq!(
            req.headers.get("x-raw").map(http::header::HeaderValue::as_bytes),
            Some(&b"caf\xe9"[..]),
            "non-UTF-8 bytes must reach filters unchanged, not as an empty value"
        );
    }

    #[test]
    fn invalid_header_field_is_dropped() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header("x-bad", "line\nbreak"),
            make_header("x-good", "ok"),
        ];

        let req = envoy_headers_to_request(&headers);

        assert!(
            req.headers.get("x-bad").is_none(),
            "control characters are not a valid field"
        );
        assert!(req.headers.get("x-good").is_some(), "valid headers are unaffected");
    }

    #[test]
    fn build_context_defaults() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let ctx = build_filter_context(test_pipeline(), &req);

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
        let ctx = build_filter_context(test_pipeline(), &req);

        assert_eq!(
            ctx.client_addr,
            Some("10.0.0.1".parse().unwrap()),
            "should extract first IP from XFF"
        );
    }

    #[test]
    fn build_context_invalid_xff_returns_none() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header("x-forwarded-for", "not-an-ip-address"),
        ];
        let req = envoy_headers_to_request(&headers);
        let ctx = build_filter_context(test_pipeline(), &req);

        assert!(
            ctx.client_addr.is_none(),
            "unparseable XFF should return None instead of panicking"
        );
    }

    #[test]
    fn build_context_prefers_envoy_external_address() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header("x-forwarded-for", "10.0.0.1, 172.16.0.1"),
            make_header("x-envoy-external-address", "203.0.113.9"),
        ];
        let req = envoy_headers_to_request(&headers);
        let ctx = build_filter_context(test_pipeline(), &req);

        assert_eq!(
            ctx.client_addr,
            Some("203.0.113.9".parse().unwrap()),
            "Envoy's trusted client address must win over the client-controlled XFF entry"
        );
    }

    #[test]
    fn build_context_ignores_unparseable_external_address() {
        let headers = vec![
            make_header(":method", "GET"),
            make_header(":path", "/"),
            make_header("x-forwarded-for", "10.0.0.1"),
            make_header("x-envoy-external-address", "not-an-ip"),
        ];
        let req = envoy_headers_to_request(&headers);
        let ctx = build_filter_context(test_pipeline(), &req);

        assert_eq!(
            ctx.client_addr,
            Some("10.0.0.1".parse().unwrap()),
            "falls back to XFF when the external address is unusable"
        );
    }

    #[test]
    fn collect_mutations_empty_when_no_extras() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let ctx = build_filter_context(test_pipeline(), &req);

        assert!(
            collect_request_header_mutations(&ctx).is_none(),
            "no mutations when empty"
        );
    }

    #[test]
    fn collect_mutations_from_extra_headers() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);
        ctx.extra_request_headers.push(("x-added".into(), "value".to_owned()));

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "should have one set header");
        assert_eq!(
            mutation.set_headers[0].header.as_ref().unwrap().key,
            "x-added",
            "key should match"
        );
        assert_eq!(
            mutation.set_headers[0].append_action,
            i32::from(HeaderAppendAction::AppendIfExistsOrAdd),
            "injected extra headers should append, not overwrite an existing value"
        );
    }

    #[test]
    fn collect_mutations_includes_rewritten_path() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/old")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);
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
        assert_eq!(
            path_header.unwrap().append_action,
            i32::from(HeaderAppendAction::OverwriteIfExistsOrAdd),
            ":path is single-valued and must overwrite the original"
        );
    }

    #[test]
    fn collect_mutations_rewritten_path_only() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);
        ctx.rewritten_path = Some("/rewritten".to_owned());

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "only :path mutation");
    }

    #[test]
    fn collect_mutations_from_set_and_remove_headers() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);
        ctx.request_headers_to_set.push((
            http::header::HeaderName::from_static("x-set"),
            http::header::HeaderValue::from_static("one"),
        ));
        ctx.request_headers_to_remove
            .push(http::header::HeaderName::from_static("x-remove"));

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "should have one set header");
        assert_eq!(
            mutation.set_headers[0].header.as_ref().unwrap().key,
            "x-set",
            "set header key should match"
        );
        assert_eq!(mutation.remove_headers, vec!["x-remove".to_owned()], "remove headers");
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
    fn terminal_response_to_immediate_basic() {
        let terminal = praxis_filter::TerminalResponse::new(200);
        let imm = terminal_response_to_immediate(&terminal);

        assert_eq!(imm.status.unwrap().code, 200, "status should be 200");
        assert!(imm.headers.is_none(), "no headers on bare terminal response");
        assert!(imm.body.is_empty(), "no body on bare terminal response");
    }

    #[test]
    fn terminal_response_to_immediate_with_body_and_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let terminal = praxis_filter::TerminalResponse::new(200)
            .with_headers(headers)
            .with_body(Bytes::from_static(b"{\"ok\":true}"));
        let imm = terminal_response_to_immediate(&terminal);

        assert_eq!(imm.status.unwrap().code, 200, "status should be 200");
        assert_eq!(imm.body, "{\"ok\":true}", "body should match");

        let hdrs = imm.headers.unwrap();
        assert_eq!(hdrs.set_headers.len(), 1, "should have one header");
        assert_eq!(
            hdrs.set_headers[0].header.as_ref().unwrap().key,
            "content-type",
            "header key should match"
        );
    }

    #[test]
    fn rejection_to_immediate_includes_byte_preserving_headers() {
        let mut map = HeaderMap::new();
        map.insert("x-upstream", "kept".parse().unwrap());
        let rejection = praxis_filter::Rejection {
            header_map: Some(Box::new(map)),
            ..praxis_filter::Rejection::status(502).with_header("x-plain", "also")
        };

        let imm = rejection_to_immediate(&rejection);

        let mut keys: Vec<String> = imm
            .headers
            .expect("headers present")
            .set_headers
            .iter()
            .map(|h| h.header.as_ref().unwrap().key.clone())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["x-plain".to_owned(), "x-upstream".to_owned()],
            "headers from both the pair list and the header map must be emitted"
        );
    }

    #[test]
    fn rejection_to_immediate_keeps_opaque_header_bytes() {
        let mut map = HeaderMap::new();
        map.insert(
            "content-disposition",
            http::header::HeaderValue::from_bytes(b"attachment; filename=\"caf\xe9\"").unwrap(),
        );
        let rejection = praxis_filter::Rejection {
            header_map: Some(Box::new(map)),
            ..praxis_filter::Rejection::status(502)
        };

        let imm = rejection_to_immediate(&rejection);

        let hv = imm.headers.expect("headers present").set_headers[0]
            .header
            .clone()
            .expect("header value");
        assert_eq!(
            hv.raw_value,
            b"attachment; filename=\"caf\xe9\"".to_vec(),
            "byte-preserving headers must be carried byte for byte, not blanked"
        );
    }

    #[test]
    fn terminal_response_to_immediate_maps_status_headers_and_body() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        let terminal = praxis_filter::TerminalResponse::new(201)
            .with_headers(headers)
            .with_body(Bytes::from_static(b"{\"ok\":true}"));

        let imm = terminal_response_to_immediate(&terminal);

        assert_eq!(imm.status.unwrap().code, 201, "status should be carried");
        assert_eq!(imm.body, "{\"ok\":true}", "body should be carried");
        let hdrs = imm.headers.expect("headers present");
        assert_eq!(hdrs.set_headers.len(), 1, "one header");
        assert_eq!(
            hdrs.set_headers[0].header.as_ref().unwrap().key,
            "content-type",
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

    #[test]
    fn response_diff_detects_added_header() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        let original = HeaderMap::new();

        resp.headers.insert("x-added", "new".parse().unwrap());
        ctx.response_header = Some(&mut resp);

        let mutation = collect_response_header_mutations_diff(&ctx, &original).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "should detect one added header");
        assert_eq!(
            mutation.set_headers[0].header.as_ref().unwrap().key,
            "x-added",
            "added header key should match"
        );
    }

    #[test]
    fn response_diff_detects_modified_value() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.insert("x-existing", "changed".parse().unwrap());

        let mut original = HeaderMap::new();
        original.insert("x-existing", "original".parse().unwrap());

        ctx.response_header = Some(&mut resp);

        let mutation = collect_response_header_mutations_diff(&ctx, &original).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "should detect value change");
        assert_eq!(
            mutation.set_headers[0].header.as_ref().unwrap().value,
            "changed",
            "should contain new value"
        );
    }

    #[test]
    fn response_diff_detects_removed_header() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };

        let mut original = HeaderMap::new();
        original.insert("x-removed", "gone".parse().unwrap());

        ctx.response_header = Some(&mut resp);

        let mutation = collect_response_header_mutations_diff(&ctx, &original).expect("should have mutations");

        assert!(mutation.set_headers.is_empty(), "no headers to set");
        assert_eq!(mutation.remove_headers.len(), 1, "should detect one removal");
        assert_eq!(
            mutation.remove_headers[0], "x-removed",
            "removed header name should match"
        );
    }

    #[test]
    fn response_diff_unchanged_returns_none() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.insert("x-keep", "same".parse().unwrap());

        let mut original = HeaderMap::new();
        original.insert("x-keep", "same".parse().unwrap());

        ctx.response_header = Some(&mut resp);

        assert!(
            collect_response_header_mutations_diff(&ctx, &original).is_none(),
            "unchanged headers should return None"
        );
    }

    #[test]
    fn response_diff_leaves_unchanged_multi_valued_header_alone() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.append("set-cookie", "a=1".parse().unwrap());
        resp.headers.append("set-cookie", "b=2".parse().unwrap());
        let original = resp.headers.clone();

        ctx.response_header = Some(&mut resp);

        assert!(
            collect_response_header_mutations_diff(&ctx, &original).is_none(),
            "an untouched multi-valued header must not be rewritten (that would collapse it)"
        );
    }

    #[test]
    fn response_diff_reemits_changed_multi_valued_header_in_order() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut original = HeaderMap::new();
        original.append("set-cookie", "a=1".parse().unwrap());

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.append("set-cookie", "a=1".parse().unwrap());
        resp.headers.append("set-cookie", "b=2".parse().unwrap());
        ctx.response_header = Some(&mut resp);

        let mutation = collect_response_header_mutations_diff(&ctx, &original).expect("should have mutations");

        let entries: Vec<(String, i32)> = mutation
            .set_headers
            .iter()
            .map(|h| (h.header.as_ref().unwrap().value.clone(), h.append_action))
            .collect();
        assert_eq!(
            entries,
            vec![
                ("a=1".to_owned(), i32::from(HeaderAppendAction::OverwriteIfExistsOrAdd)),
                ("b=2".to_owned(), i32::from(HeaderAppendAction::AppendIfExistsOrAdd)),
            ],
            "first value overwrites, later values append, preserving order"
        );
        assert!(mutation.remove_headers.is_empty(), "nothing to remove");
    }

    #[test]
    fn response_diff_dropping_one_of_several_values_overwrites_with_the_rest() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut original = HeaderMap::new();
        original.append("vary", "accept".parse().unwrap());
        original.append("vary", "origin".parse().unwrap());

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.append("vary", "origin".parse().unwrap());
        ctx.response_header = Some(&mut resp);

        let mutation = collect_response_header_mutations_diff(&ctx, &original).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "single remaining value");
        let only = &mutation.set_headers[0];
        assert_eq!(only.header.as_ref().unwrap().value, "origin");
        assert_eq!(
            only.append_action,
            i32::from(HeaderAppendAction::OverwriteIfExistsOrAdd),
            "the surviving value must replace the whole list"
        );
        assert!(mutation.remove_headers.is_empty(), "the name still exists");
    }

    #[test]
    fn response_diff_keeps_opaque_value_bytes() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req);

        let mut resp = Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        resp.headers.insert(
            "content-disposition",
            http::header::HeaderValue::from_bytes(b"attachment; filename=\"caf\xe9\"").unwrap(),
        );
        ctx.response_header = Some(&mut resp);

        let mutation = collect_response_header_mutations_diff(&ctx, &HeaderMap::new()).expect("added header");

        let hv = mutation.set_headers[0].header.as_ref().unwrap();
        assert_eq!(
            hv.raw_value,
            b"attachment; filename=\"caf\xe9\"".to_vec(),
            "opaque bytes are sent in raw_value instead of being blanked"
        );
        assert!(hv.value.is_empty(), "a protobuf string cannot carry non-UTF-8 bytes");
    }

    #[test]
    fn header_value_str_prefers_raw_value() {
        let hv = HeaderValue {
            key: "x-test".to_owned(),
            value: "fallback".to_owned(),
            raw_value: b"raw".to_vec(),
        };

        assert_eq!(header_value_str(&hv), "raw", "should prefer raw_value");
    }

    #[test]
    fn header_value_str_falls_back_to_value() {
        let hv = HeaderValue {
            key: "x-test".to_owned(),
            value: "text".to_owned(),
            raw_value: Vec::new(),
        };

        assert_eq!(
            header_value_str(&hv),
            "text",
            "should use value when raw_value is empty"
        );
    }

    #[test]
    fn set_content_length_creates_mutation_when_absent() {
        let mutation = set_content_length(None, 42);

        let cl = mutation
            .set_headers
            .iter()
            .find(|h| h.header.as_ref().unwrap().key == "content-length")
            .expect("content-length should be set");
        assert_eq!(cl.header.as_ref().unwrap().value, "42", "should carry the byte length");
        assert_eq!(
            cl.append_action,
            i32::from(HeaderAppendAction::OverwriteIfExistsOrAdd),
            "content-length must overwrite an original header, not append a second value"
        );
    }

    #[test]
    fn set_content_length_overwrites_stale_value() {
        let existing = HeaderMutation {
            set_headers: vec![header_value_option("content-length", "999")],
            remove_headers: vec![],
        };

        let mutation = set_content_length(Some(existing), 7);

        let entries: Vec<_> = mutation
            .set_headers
            .iter()
            .filter(|h| h.header.as_ref().unwrap().key.eq_ignore_ascii_case("content-length"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "stale content-length should be replaced, not duplicated"
        );
        assert_eq!(
            entries[0].header.as_ref().unwrap().value,
            "7",
            "value should reflect new length"
        );
    }

    #[test]
    fn set_content_length_preserves_other_headers() {
        let existing = HeaderMutation {
            set_headers: vec![header_value_option("x-keep", "yes")],
            remove_headers: vec!["x-drop".to_owned()],
        };

        let mutation = set_content_length(Some(existing), 3);

        assert!(
            mutation
                .set_headers
                .iter()
                .any(|h| h.header.as_ref().unwrap().key == "x-keep"),
            "unrelated set header should be preserved"
        );
        assert_eq!(
            mutation.remove_headers,
            vec!["x-drop".to_owned()],
            "remove list untouched"
        );
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
