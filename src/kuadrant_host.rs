// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Native host for the Kuadrant policy pipeline.
//!
//! [`PraxisResolver`] implements `kuadrant-filter`'s [`AttributeResolver`] over
//! request/response snapshots taken from Praxis's `HttpFilterContext`, and
//! [`drive_phase`] bridges the pipeline's *synchronous, deferred* gRPC
//! dispatch model to Praxis's *async* filter pipeline.
//!
//! The pipeline never makes network calls itself: a task calls
//! [`AttributeResolver::dispatch_grpc_call`], which here just **records** the
//! call and returns a token; the pipeline yields `InProgress`; the async driver
//! executes the call with a real transport, stashes the bytes for
//! [`AttributeResolver::get_grpc_response`], and resumes via `Pipeline::digest`.
//! No WASM, no `proxy-wasm` — native Rust.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use async_trait::async_trait;
use kuadrant_filter::{
    data::attribute::{AttributeError, Path},
    kuadrant::{
        Pipeline, PipelineState,
        resolver::{AttributeResolver, MapType},
    },
    services::ServiceError,
};

/// Named header maps (`request.headers` / `response.headers`) -> their pairs.
type NamedHeaders = HashMap<String, Vec<(String, String)>>;

/// Lock a mutex, treating poisoning (a thread panicked while holding it) as the
/// unrecoverable bug it is. Centralizes the one `expect` so call sites stay clean.
#[expect(clippy::expect_used, reason = "mutex poisoning is an unrecoverable bug")]
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().expect("mutex poisoned")
}

/// A gRPC call the pipeline deferred, captured for the async driver to execute.
#[derive(Debug, Clone)]
pub struct GrpcDispatch {
    /// Token the pipeline uses to correlate the response via `digest`.
    pub token: u32,
    /// Upstream cluster name (e.g. the Authorino/Limitador service).
    pub upstream: String,
    /// Fully-qualified gRPC service name.
    pub service: String,
    /// gRPC method name.
    pub method: String,
    /// gRPC metadata headers (raw byte values).
    pub headers: Vec<(String, Vec<u8>)>,
    /// Serialized request message.
    pub message: Vec<u8>,
    /// Call timeout.
    pub timeout: Duration,
}

/// An immediate HTTP reply the pipeline asked to send (e.g. a 401/429).
#[derive(Debug, Clone)]
pub struct HttpReply {
    /// HTTP status code to return (e.g. 401, 429).
    pub status: u32,
    /// Response headers.
    pub headers: Vec<(String, String)>,
    /// Optional response body.
    pub body: Option<Vec<u8>>,
}

/// Praxis's `AttributeResolver` implementation — snapshot-backed, with
/// interior mutability so the (synchronous) pipeline can record a deferred
/// gRPC dispatch and later read its response.
pub struct PraxisResolver {
    /// Envoy attribute paths -> raw bytes (also holds filter-state writes).
    properties: Mutex<HashMap<Path, Vec<u8>>>,
    /// Named header maps: `request.headers` / `response.headers`. Interior
    /// mutability lets the per-stream executor swap in response data on a later
    /// phase while the same pipeline instance still borrows this resolver.
    maps: Mutex<NamedHeaders>,
    /// Request body bytes, if seen.
    request_body: Mutex<Option<Vec<u8>>>,
    /// Current response body chunk (streamed) or full body (buffered).
    response_body: Mutex<Option<Vec<u8>>>,
    /// Monotonic token source for `dispatch_grpc_call`.
    next_token: Mutex<u32>,
    /// The call the pipeline last deferred (drained by the driver).
    pending: Mutex<Option<GrpcDispatch>>,
    /// The response bytes the driver stashed for `get_grpc_response`.
    grpc_response: Mutex<Option<Vec<u8>>>,
    /// An immediate reply the pipeline asked to send.
    http_reply: Mutex<Option<HttpReply>>,
    /// Whether the response phase has begun. Until then `get_response_headers`
    /// reports `NotAvailable` (→ `Pending`, uncached) so response-phase tasks
    /// (e.g. token-usage strategy selection) don't cache a premature empty map —
    /// matching the WASM host, where response headers are absent until the
    /// response phase.
    response_headers_ready: Mutex<bool>,
}

impl PraxisResolver {
    /// Build a resolver from request/response snapshots (populated in
    /// `server.rs` from `HttpFilterContext` via `adapter.rs`).
    #[must_use]
    pub fn new(
        request_headers: Vec<(String, String)>,
        response_headers: Vec<(String, String)>,
        request_body: Option<Vec<u8>>,
        response_body: Option<Vec<u8>>,
    ) -> Self {
        let response_ready = !response_headers.is_empty();
        let mut properties = HashMap::new();
        seed_request_properties(&mut properties, &request_headers);
        let mut maps = HashMap::new();
        // Move (no clone) now that seeding has borrowed it.
        maps.insert("request.headers".to_owned(), request_headers);
        maps.insert("response.headers".to_owned(), response_headers);
        Self {
            properties: Mutex::new(properties),
            maps: Mutex::new(maps),
            request_body: Mutex::new(request_body),
            response_body: Mutex::new(response_body),
            next_token: Mutex::new(1),
            pending: Mutex::new(None),
            grpc_response: Mutex::new(None),
            http_reply: Mutex::new(None),
            response_headers_ready: Mutex::new(response_ready),
        }
    }

    /// Take the gRPC call the pipeline just deferred, if any.
    #[must_use]
    pub fn take_pending(&self) -> Option<GrpcDispatch> {
        lock(&self.pending).take()
    }

    /// Stash a gRPC response for the next `get_grpc_response`.
    pub fn set_grpc_response(&self, bytes: Vec<u8>) {
        *lock(&self.grpc_response) = Some(bytes);
    }

    /// Take any immediate reply the pipeline asked to send.
    #[must_use]
    pub fn take_http_reply(&self) -> Option<HttpReply> {
        lock(&self.http_reply).take()
    }

    /// Install the request headers (and derive the `request.*` properties that
    /// blueprint selection reads). Called once, on the request-headers phase.
    pub fn update_request_headers(&self, headers: Vec<(String, String)>) {
        seed_request_properties(&mut lock(&self.properties), &headers);
        lock(&self.maps).insert("request.headers".to_owned(), headers);
    }

    /// Install the response headers, visible to the persisted pipeline when it
    /// resumes on the response phase, and mark the response phase as begun.
    pub fn update_response_headers(&self, headers: Vec<(String, String)>) {
        lock(&self.maps).insert("response.headers".to_owned(), headers);
        *lock(&self.response_headers_ready) = true;
    }

    /// Install the current response-body chunk and return its length. This
    /// mirrors the WASM host's per-chunk buffer: Envoy drains the response body
    /// each callback, so `get_http_response_body(start, n)` reads the *current*
    /// chunk and the token-usage parser accumulates frames itself. The returned
    /// length is the chunk size the executor hands the ctx as its buffer size.
    #[must_use]
    pub fn set_response_body_chunk(&self, chunk: Vec<u8>) -> usize {
        let len = chunk.len();
        *lock(&self.response_body) = Some(chunk);
        len
    }

    /// Clone a named header map (`request.headers` / `response.headers`).
    fn get_map(&self, name: &str) -> Result<Vec<(String, String)>, AttributeError> {
        lock(&self.maps)
            .get(name)
            .cloned()
            .ok_or_else(|| AttributeError::Retrieval(format!("no map: {name}")))
    }
}

/// Split a dotted attribute path into its `Path` tokens.
fn path_tokens(path: &str) -> Vec<String> {
    path.split('.').map(str::to_owned).collect()
}

/// Store `request.<attr>` (or any dotted path) as a string property.
fn put_str(properties: &mut HashMap<Path, Vec<u8>>, path: &str, value: &str) {
    properties.insert(Path::new(path_tokens(path)), value.as_bytes().to_vec());
}

/// Store a dotted path as an i64 little-endian property (the encoding the CEL
/// layer decodes `Int` attributes with).
fn put_i64(properties: &mut HashMap<Path, Vec<u8>>, path: &str, value: i64) {
    properties.insert(Path::new(path_tokens(path)), value.to_le_bytes().to_vec());
}

/// Seed the `request.*` (and peer) attributes the pipeline reads, from the Envoy
/// pseudo-headers. Blueprint route predicates need `request.host` / `method` /
/// `url_path`; the Authorino `CheckRequest` builder additionally needs
/// `request.path` (with query), `scheme`, `protocol`, `time`, and the
/// `source`/`destination` peer address and port. Missing any of those made the
/// message builder resolve `null` and auth fail closed. Peer addresses and the
/// protocol are not carried on the `ext_proc` header phase, so they default here;
/// a later pass can lift them from the `ext_proc` `attributes` Envoy sends.
fn seed_request_properties(properties: &mut HashMap<Path, Vec<u8>>, headers: &[(String, String)]) {
    for (key, value) in headers {
        match key.as_str() {
            ":authority" | "host" => put_str(properties, "request.host", value),
            ":method" => put_str(properties, "request.method", value),
            ":scheme" => put_str(properties, "request.scheme", value),
            ":path" => {
                // `path` keeps the query; `url_path` is the path without it.
                put_str(properties, "request.path", value);
                let (url_path, query) = value
                    .split_once('?')
                    .map_or((value.as_str(), None), |(p, q)| (p, Some(q)));
                put_str(properties, "request.url_path", url_path);
                if let Some(query) = query {
                    put_str(properties, "request.query", query);
                }
            },
            _ => {},
        }
    }

    // Attributes the ext_proc header phase does not carry. Defaults keep the
    // CheckRequest well-formed; auth (API key / identity) does not key off them.
    if !properties.contains_key(&Path::new(path_tokens("request.scheme"))) {
        put_str(properties, "request.scheme", "https");
    }
    put_str(properties, "request.protocol", "HTTP/1.1");
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    put_i64(properties, "request.time", now_nanos);
    put_str(properties, "source.address", "0.0.0.0");
    put_i64(properties, "source.port", 0);
    put_str(properties, "destination.address", "0.0.0.0");
    put_i64(properties, "destination.port", 0);
}

/// Slice `[start, start+max_size)` out of an optional body, clamped to its length.
fn slice_body(body: Option<&[u8]>, start: usize, max_size: usize) -> Option<Vec<u8>> {
    body.map(|b| {
        let end = start.saturating_add(max_size).min(b.len());
        b.get(start..end).unwrap_or(&[]).to_vec()
    })
}

impl AttributeResolver for PraxisResolver {
    fn get_attribute(&self, path: &Path) -> Result<Option<Vec<u8>>, AttributeError> {
        Ok(lock(&self.properties).get(path).cloned())
    }

    fn get_attribute_map(&self, map_type: MapType) -> Result<Vec<(String, String)>, AttributeError> {
        match map_type {
            MapType::HttpRequestHeaders => self.get_map("request.headers"),
            MapType::HttpResponseHeaders => {
                // Absent until the response phase, so tasks don't cache an empty map.
                if !*lock(&self.response_headers_ready) {
                    return Err(AttributeError::NotAvailable("response.headers".to_owned()));
                }
                self.get_map("response.headers")
            },
        }
    }

    fn get_attribute_map_value(&self, map_type: MapType, key: &str) -> Result<Option<String>, AttributeError> {
        Ok(self
            .get_attribute_map(map_type)?
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v))
    }

    fn set_attribute(&self, path: &Path, value: &[u8]) -> Result<(), AttributeError> {
        // Mirror the shim: single-token writes land under filter_state/wasm.<token>.
        let tokens = path.tokens();
        let storage = match tokens.as_slice() {
            [single] => Path::new(vec!["filter_state".to_owned(), format!("wasm.{single}")]),
            _ => Path::new(tokens),
        };
        lock(&self.properties).insert(storage, value.to_vec());
        Ok(())
    }

    fn set_attribute_map(&self, map_type: MapType, value: Vec<(&str, &str)>) -> Result<(), AttributeError> {
        let name = match map_type {
            MapType::HttpRequestHeaders => "request.headers",
            MapType::HttpResponseHeaders => "response.headers",
        };
        let owned = value.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect();
        lock(&self.maps).insert(name.to_owned(), owned);
        Ok(())
    }

    fn get_http_request_body(&self, start: usize, max_size: usize) -> Result<Option<Vec<u8>>, AttributeError> {
        let body = lock(&self.request_body);
        Ok(slice_body(body.as_deref(), start, max_size))
    }

    fn get_http_response_body(&self, start: usize, max_size: usize) -> Result<Option<Vec<u8>>, AttributeError> {
        let body = lock(&self.response_body);
        Ok(slice_body(body.as_deref(), start, max_size))
    }

    fn dispatch_grpc_call(
        &self,
        upstream_name: &str,
        service_name: &str,
        method: &str,
        headers: Vec<(&str, &[u8])>,
        message: Vec<u8>,
        timeout: Duration,
    ) -> Result<u32, ServiceError> {
        let token = {
            let mut tok = lock(&self.next_token);
            let token = *tok;
            *tok = tok.wrapping_add(1);
            token
        };
        let dispatch = GrpcDispatch {
            token,
            upstream: upstream_name.to_owned(),
            service: service_name.to_owned(),
            method: method.to_owned(),
            headers: headers.into_iter().map(|(k, v)| (k.to_owned(), v.to_vec())).collect(),
            message,
            timeout,
        };
        // Single deferred slot: the driver drains `pending` between dispatches. A
        // second call while one is still pending means the pipeline dispatched two
        // calls in one eval step, which this bridge cannot correlate. Overwriting
        // would feed one backend's response to the other's token, an undefined
        // decision, so fail closed and let the task run its service's failureMode
        // (auth denies) instead of silently allowing.
        let mut slot = lock(&self.pending);
        if slot.is_some() {
            return Err(ServiceError::Dispatch(
                "kuadrant: a gRPC dispatch is already pending (concurrent dispatch unsupported)".to_owned(),
            ));
        }
        *slot = Some(dispatch);
        drop(slot);
        Ok(token)
    }

    fn get_grpc_response(&self, _response_size: usize) -> Result<Vec<u8>, ServiceError> {
        lock(&self.grpc_response)
            .clone()
            .ok_or_else(|| ServiceError::Retrieval("no gRPC response stashed".to_owned()))
    }

    fn send_http_reply(
        &self,
        status_code: u32,
        headers: Vec<(&str, &str)>,
        body: Option<&[u8]>,
    ) -> Result<(), ServiceError> {
        *lock(&self.http_reply) = Some(HttpReply {
            status: status_code,
            headers: headers.into_iter().map(|(k, v)| (k.to_owned(), v.to_owned())).collect(),
            body: body.map(<[u8]>::to_vec),
        });
        Ok(())
    }
}

/// Executes a deferred gRPC call for the driver. The real impl (next step) is a
/// raw bytes-in/bytes-out `tonic` client keyed on `service`/`method`; kept as a
/// trait so the driver compiles and unit-tests can inject a fake.
#[async_trait]
pub trait GrpcTransport: Send + Sync {
    /// Returns `(status_code, response_bytes)`.
    async fn call(&self, pending: &GrpcDispatch) -> Result<(u32, Vec<u8>), String>;
}

/// Errors from driving the pipeline.
#[derive(Debug)]
pub enum DriverError {
    /// The transport failed.
    Transport(String),
}

/// The result of driving a persisted pipeline for one request/response phase.
pub enum PhaseOutcome {
    /// The pipeline finished. `None` = allow; `Some(reply)` = reject/immediate.
    Done(Option<HttpReply>),
    /// The pipeline paused: it has run everything it can with the data seen so
    /// far and is waiting on a later phase. The executor holds this instance and
    /// resumes it (via `Pipeline::eval`) once the next phase's data is installed.
    /// Boxed — `Pipeline` is large, and it arrives already boxed from
    /// `PipelineState::InProgress`, so this avoids copying it.
    Paused(Box<Pipeline>),
}

/// Drive a persisted pipeline forward for one phase, executing each deferred
/// gRPC call inline against `transport`, and yield the pipeline back when it
/// pauses so the same instance spans request→response (the rate-limit *check*
/// on the request, the token *report* on the response).
///
/// Pass the [`PipelineState`] from `pipeline.eval()` (first phase) or a resumed
/// `held.eval()` (later phases) after installing that phase's data on the resolver.
///
/// # Errors
///
/// Returns [`DriverError::Transport`] if a gRPC call fails.
#[expect(
    clippy::future_not_send,
    reason = "runs on the per-stream current-thread runtime; the Kuadrant pipeline is !Send by design"
)]
pub async fn drive_phase<T: GrpcTransport>(
    mut state: PipelineState,
    resolver: &PraxisResolver,
    transport: &T,
) -> Result<PhaseOutcome, DriverError> {
    loop {
        match state {
            PipelineState::Completed { .. } => {
                return Ok(PhaseOutcome::Done(resolver.take_http_reply()));
            },
            PipelineState::InProgress(pl) => match resolver.take_pending() {
                // A task deferred a gRPC call: make it, feed the bytes back, resume.
                Some(dispatch) => {
                    let (status, resp) = transport.call(&dispatch).await.map_err(DriverError::Transport)?;
                    let resp_size = resp.len();
                    resolver.set_grpc_response(resp);
                    state = (*pl).digest(dispatch.token, status, resp_size);
                },
                // In progress but nothing to dispatch: the remaining tasks are
                // waiting on a later phase's data. Hand the pipeline back to pause.
                None => return Ok(PhaseOutcome::Paused(pl)),
            },
        }
    }
}

#[cfg(test)]
#[expect(clippy::expect_used, clippy::clone_on_ref_ptr, reason = "tests")]
mod tests {
    use std::sync::Arc;

    use kuadrant_filter::kuadrant::{Pipeline, ReqRespCtx};

    use super::*;

    // NOTE: kuadrant-filter does not export `Task`/`TaskOutcome`, so we can't
    // hand-build a deferring pipeline here. These tests cover the resolver
    // bridge + the driver's allow/completion path; the full deferred->async
    // loop is exercised in `server.rs` against a real PipelineFactory config
    // (and would benefit from the crate exporting Task for isolated testing).

    #[test]
    fn dispatch_records_pending_and_increments_token() {
        let r = PraxisResolver::new(vec![], vec![], None, None);
        let t1 = r
            .dispatch_grpc_call("up", "svc.A", "M", vec![], b"m1".to_vec(), Duration::from_secs(1))
            .expect("dispatch");
        let p1 = r.take_pending().expect("pending recorded");
        assert_eq!(p1.token, t1);
        assert_eq!(p1.service, "svc.A");
        assert_eq!(p1.message, b"m1".to_vec());
        assert!(r.take_pending().is_none(), "pending is drained once");
        let t2 = r
            .dispatch_grpc_call("up", "svc.B", "M", vec![], b"m2".to_vec(), Duration::from_secs(1))
            .expect("dispatch");
        assert_ne!(t1, t2, "tokens increment");
    }

    #[test]
    fn second_dispatch_before_drain_fails_closed() {
        // A second dispatch while one is still pending (undrained) must be
        // refused, not silently overwrite the first: the driver never sees two
        // un-correlated calls, so an auth call cannot be dropped for a later one.
        let r = PraxisResolver::new(vec![], vec![], None, None);
        r.dispatch_grpc_call("up", "svc.A", "M", vec![], b"m1".to_vec(), Duration::from_secs(1))
            .expect("first dispatch");
        let second = r.dispatch_grpc_call("up", "svc.B", "M", vec![], b"m2".to_vec(), Duration::from_secs(1));
        assert!(second.is_err(), "second dispatch before drain is rejected");
        let pending = r.take_pending().expect("first dispatch still pending");
        assert_eq!(pending.service, "svc.A", "the first call was not overwritten");
    }

    #[test]
    fn grpc_response_round_trips() {
        let r = PraxisResolver::new(vec![], vec![], None, None);
        assert!(r.get_grpc_response(0).is_err(), "no response stashed yet");
        r.set_grpc_response(b"resp".to_vec());
        assert_eq!(r.get_grpc_response(0).expect("response"), b"resp".to_vec());
    }

    #[test]
    fn send_http_reply_is_captured() {
        let r = PraxisResolver::new(vec![], vec![], None, None);
        r.send_http_reply(429, vec![("retry-after", "5")], Some(b"limited".as_slice()))
            .expect("reply");
        let reply = r.take_http_reply().expect("reply captured");
        assert_eq!(reply.status, 429);
        assert_eq!(reply.headers, vec![("retry-after".to_owned(), "5".to_owned())]);
        assert_eq!(reply.body, Some(b"limited".to_vec()));
    }

    #[test]
    fn header_and_body_accessors() {
        let r = PraxisResolver::new(
            vec![("x-api-key".to_owned(), "abc".to_owned())],
            vec![],
            Some(b"hello world".to_vec()),
            None,
        );
        assert_eq!(
            r.get_attribute_map_value(MapType::HttpRequestHeaders, "x-api-key")
                .expect("hdr"),
            Some("abc".to_owned())
        );
        assert_eq!(
            r.get_attribute_map_value(MapType::HttpRequestHeaders, "missing")
                .expect("hdr"),
            None
        );
        assert_eq!(r.get_http_request_body(0, 5).expect("body"), Some(b"hello".to_vec()));
        assert_eq!(r.get_http_request_body(6, 100).expect("body"), Some(b"world".to_vec()));
    }

    #[derive(Default)]
    struct FakeTransport {
        calls: Mutex<u32>,
    }
    #[async_trait]
    impl GrpcTransport for FakeTransport {
        async fn call(&self, _pending: &GrpcDispatch) -> Result<(u32, Vec<u8>), String> {
            *lock(&self.calls) += 1;
            Ok((0, b"resp".to_vec()))
        }
    }

    #[tokio::test]
    async fn empty_pipeline_completes_without_transport() {
        let resolver = Arc::new(PraxisResolver::new(vec![], vec![], None, None));
        let pipeline = Pipeline::new(ReqRespCtx::new(resolver.clone()));
        let transport = FakeTransport::default();
        let outcome = drive_phase(pipeline.eval(), resolver.as_ref(), &transport)
            .await
            .expect("driver");
        assert!(
            matches!(outcome, PhaseOutcome::Done(None)),
            "no tasks -> allow, no immediate reply"
        );
        assert_eq!(*transport.calls.lock().expect("calls mutex"), 0, "no deferred calls");
    }
}
