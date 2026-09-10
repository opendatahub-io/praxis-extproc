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

use std::{any::Any, collections::HashMap, fmt, mem, net::IpAddr, sync::Arc, time::Instant};

use http::{HeaderMap, Method, StatusCode, Uri};
use praxis_filter::{
    BodyMode, FilterPipeline, FilterResultSet, HttpFilterContext, Request, RequestExtensions, Response,
};
use praxis_proto::envoy::service::{
    common::v3::{HeaderValue, HeaderValueOption, HttpStatus, header_value_option::HeaderAppendAction},
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
            ":path" => val.clone_into(&mut path),
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
// Carried Context
// -----------------------------------------------------------------------------

/// Filter context state that outlives a single ExtProc phase.
///
/// [`HttpFilterContext`] owns these fields by value and is rebuilt for every
/// phase of a stream. The server keeps one `CarriedContext` per stream, moves
/// its contents into each context it builds and takes them back afterwards,
/// so filters see the same state across request headers, body, response
/// headers and response body — the same threading the Pingora protocol
/// layer does for a proxied request.
///
/// [`HttpFilterContext`]: praxis_filter::HttpFilterContext
pub struct CarriedContext {
    /// Per-filter body-done flags (`FilterAction::BodyDone`).
    pub body_done_indices: Vec<bool>,

    /// Re-entrance counters from request-phase branch chains.
    pub branch_iterations: HashMap<Arc<str>, u32>,

    /// Filter indices that ran during the request phase.
    pub executed_filter_indices: Vec<bool>,

    /// Typed request-scoped values, pre-populated by pipeline extensions.
    pub extensions: RequestExtensions,

    /// Durable string metadata written by filters.
    pub filter_metadata: HashMap<String, String>,

    /// Branch-condition results keyed by filter name.
    pub filter_results: HashMap<&'static str, FilterResultSet>,

    /// Typed per-filter state keyed by filter invocation ID.
    pub filter_state: HashMap<usize, Box<dyn Any + Send + Sync>>,

    /// Request body bytes seen so far.
    pub request_body_bytes: u64,

    /// When the stream opened; the request's start for duration metrics.
    pub request_start: Instant,

    /// Response body bytes seen so far.
    pub response_body_bytes: u64,

    /// Structured metadata keyed by namespace.
    pub structured_metadata: HashMap<String, serde_json::Value>,
}

impl CarriedContext {
    /// Fresh state for a new stream, with pipeline extensions prepared.
    pub fn new(pipeline: &FilterPipeline) -> Self {
        let mut extensions = RequestExtensions::new();
        pipeline.prepare_extensions(&mut extensions);

        Self {
            body_done_indices: Vec::new(),
            branch_iterations: HashMap::new(),
            executed_filter_indices: Vec::new(),
            extensions,
            filter_metadata: HashMap::new(),
            filter_results: HashMap::new(),
            filter_state: HashMap::new(),
            request_body_bytes: 0,
            request_start: Instant::now(),
            response_body_bytes: 0,
            structured_metadata: HashMap::new(),
        }
    }

    /// Take the carried state back out of a context once its phase is done.
    pub fn reclaim(&mut self, ctx: &mut HttpFilterContext<'_>) {
        self.body_done_indices = mem::take(&mut ctx.body_done_indices);
        self.branch_iterations = mem::take(&mut ctx.branch_iterations);
        self.executed_filter_indices = mem::take(&mut ctx.executed_filter_indices);
        self.extensions = mem::take(&mut ctx.extensions);
        self.filter_metadata = mem::take(&mut ctx.filter_metadata);
        self.filter_results = mem::take(&mut ctx.filter_results);
        self.filter_state = mem::take(&mut ctx.filter_state);
        self.request_body_bytes = ctx.request_body_bytes;
        self.response_body_bytes = ctx.response_body_bytes;
        self.structured_metadata = mem::take(&mut ctx.structured_metadata);
    }
}

impl fmt::Debug for CarriedContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CarriedContext")
            .field("executed_filter_indices", &self.executed_filter_indices)
            .field("filter_metadata", &self.filter_metadata)
            .field("filter_state_entries", &self.filter_state.len())
            .field("request_body_bytes", &self.request_body_bytes)
            .field("response_body_bytes", &self.response_body_bytes)
            .finish_non_exhaustive()
    }
}

// -----------------------------------------------------------------------------
// Phase Context
// -----------------------------------------------------------------------------

/// A phase's [`HttpFilterContext`] bound to the stream's [`CarriedContext`].
///
/// The carried state is moved into the context on construction and handed
/// back when this guard drops, so every exit from a phase — a rejection, an
/// error, an early return — leaves the stream's state intact for messages
/// that may still follow. Dereferences to the inner context.
///
/// [`HttpFilterContext`]: praxis_filter::HttpFilterContext
pub struct PhaseContext<'a> {
    /// Stream state to hand back on drop.
    carried: &'a mut CarriedContext,

    /// The context filters run against during this phase.
    ctx: HttpFilterContext<'a>,
}

impl<'a> PhaseContext<'a> {
    /// Build the context for one phase from the stream's carried state.
    pub fn new(pipeline: &'a FilterPipeline, request: &'a Request, carried: &'a mut CarriedContext) -> Self {
        let ctx = build_filter_context(pipeline, request, carried);
        Self { carried, ctx }
    }
}

impl<'a> std::ops::Deref for PhaseContext<'a> {
    type Target = HttpFilterContext<'a>;

    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl std::ops::DerefMut for PhaseContext<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}

impl Drop for PhaseContext<'_> {
    fn drop(&mut self) {
        self.carried.reclaim(&mut self.ctx);
    }
}

// -----------------------------------------------------------------------------
// Context Construction
// -----------------------------------------------------------------------------

/// Build an [`HttpFilterContext`] for one phase of a stream.
///
/// Moves the stream's [`CarriedContext`] into the new context; the caller
/// hands it back with [`CarriedContext::reclaim`] when the phase is done.
/// Populates `client_addr` from the `x-forwarded-for` header if present.
/// All routing fields (`cluster`, `upstream`) default to `None`; they are
/// advisory in ExtProc mode since Envoy owns routing.
///
/// [`HttpFilterContext`]: praxis_filter::HttpFilterContext
#[expect(
    clippy::too_many_lines,
    reason = "HttpFilterContext field init mirrors the struct; splitting obscures defaults"
)]
pub fn build_filter_context<'a>(
    pipeline: &'a FilterPipeline,
    request: &'a Request,
    carried: &mut CarriedContext,
) -> HttpFilterContext<'a> {
    let client_addr = extract_client_addr(request);

    HttpFilterContext {
        buffered_request_body: None,
        body_done_indices: mem::take(&mut carried.body_done_indices),
        branch_iterations: mem::take(&mut carried.branch_iterations),
        client_addr,
        cluster: None,
        current_filter_id: None,
        downstream_tls: false,
        metrics_route: None,
        peer_identity: None,
        extensions: mem::take(&mut carried.extensions),
        executed_filter_indices: mem::take(&mut carried.executed_filter_indices),
        extra_request_headers: Vec::new(),
        request_headers_to_remove: Vec::new(),
        request_headers_to_set: Vec::new(),
        filter_metadata: mem::take(&mut carried.filter_metadata),
        pre_read_mutations: Vec::new(),
        structured_metadata: mem::take(&mut carried.structured_metadata),
        filter_results: mem::take(&mut carried.filter_results),
        filter_state: mem::take(&mut carried.filter_state),
        health_registry: pipeline.health_registry(),
        id_generator: pipeline.id_generator(),
        kv_stores: pipeline.kv_stores(),
        subrequest_client: pipeline.subrequest_client(),
        request,
        request_body_bytes: carried.request_body_bytes,
        request_body_mode: BodyMode::Stream,
        request_start: carried.request_start,
        response_body_bytes: carried.response_body_bytes,
        response_body_mode: BodyMode::Stream,
        response_header: None,
        response_headers_modified: false,
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
        let ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

        assert_eq!(
            ctx.client_addr,
            Some("10.0.0.1".parse().unwrap()),
            "should extract first IP from XFF"
        );
    }

    #[test]
    fn carried_context_prepares_pipeline_extensions() {
        struct Marker(u8);

        struct MarkerExtension;

        impl praxis_filter::PipelineExtension for MarkerExtension {
            fn prepare(&self, extensions: &mut RequestExtensions) {
                extensions.insert(Marker(7));
            }
        }

        let mut pipeline = FilterPipeline::build(&mut [], &FilterRegistry::with_builtins()).expect("empty pipeline");
        pipeline.add_pipeline_extension(Box::new(MarkerExtension));
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);

        let mut carried = CarriedContext::new(&pipeline);
        let ctx = build_filter_context(&pipeline, &req, &mut carried);

        assert_eq!(
            ctx.extensions.get::<Marker>().map(|m| m.0),
            Some(7),
            "pipeline extensions must reach the request context"
        );
    }

    #[test]
    fn carried_context_round_trips_filter_state_and_metadata() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut carried = CarriedContext::new(test_pipeline());

        let mut ctx = build_filter_context(test_pipeline(), &req, &mut carried);
        ctx.current_filter_id = Some(3);
        ctx.insert_filter_state(String::from("seen"));
        ctx.set_metadata("probe.key", "value");
        ctx.body_done_indices = vec![true];
        ctx.request_body_bytes = 42;
        carried.reclaim(&mut ctx);

        let mut ctx = build_filter_context(test_pipeline(), &req, &mut carried);
        ctx.current_filter_id = Some(3);
        assert_eq!(
            ctx.get_filter_state::<String>().map(String::as_str),
            Some("seen"),
            "filter_state must survive into the next phase"
        );
        assert_eq!(ctx.get_metadata("probe.key"), Some("value"), "metadata must survive");
        assert_eq!(ctx.body_done_indices, vec![true], "body-done flags must survive");
        assert_eq!(ctx.request_body_bytes, 42, "byte counters must survive");
    }

    #[test]
    fn phase_context_reclaims_state_on_drop() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut carried = CarriedContext::new(test_pipeline());

        {
            let mut ctx = PhaseContext::new(test_pipeline(), &req, &mut carried);
            ctx.current_filter_id = Some(1);
            ctx.insert_filter_state(String::from("kept"));
            ctx.request_body_bytes = 9;
        }

        assert!(
            carried.filter_state.contains_key(&1),
            "dropping the guard must hand filter state back to the stream"
        );
        assert_eq!(carried.request_body_bytes, 9, "counters are handed back too");
    }

    #[test]
    fn carried_context_keeps_request_start_stable() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut carried = CarriedContext::new(test_pipeline());

        let first = build_filter_context(test_pipeline(), &req, &mut carried).request_start;
        let second = build_filter_context(test_pipeline(), &req, &mut carried).request_start;

        assert_eq!(first, second, "every phase must measure from the same request start");
        assert_eq!(first, carried.request_start, "the start is owned by the stream");
    }

    #[test]
    fn collect_mutations_empty_when_no_extras() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

        assert!(
            collect_request_header_mutations(&ctx).is_none(),
            "no mutations when empty"
        );
    }

    #[test]
    fn collect_mutations_from_extra_headers() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));
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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));
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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));
        ctx.rewritten_path = Some("/rewritten".to_owned());

        let mutation = collect_request_header_mutations(&ctx).expect("should have mutations");

        assert_eq!(mutation.set_headers.len(), 1, "only :path mutation");
    }

    #[test]
    fn collect_mutations_from_set_and_remove_headers() {
        let req = envoy_headers_to_request(&[make_header(":method", "GET"), make_header(":path", "/")]);
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));
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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
        let mut ctx = build_filter_context(test_pipeline(), &req, &mut CarriedContext::new(test_pipeline()));

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
