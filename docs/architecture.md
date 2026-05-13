# Architecture

The Praxis ExtProc server is a standalone gRPC service
that translates [Envoy ExtProc] messages into [Praxis]
filter pipeline invocations.

[Envoy ExtProc]: https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter
[Praxis]: https://github.com/praxis-proxy/praxis

## Overview

Envoy's External Processing filter opens a
bidirectional gRPC stream per HTTP request. The
ExtProc server receives header and body messages on
each stream, runs the Praxis filter pipeline, and
replies with mutations or rejections.

```text
                         ┌──────────────────────────────┐
  Client ──► Envoy ──────►  praxis-extproc (gRPC)       │
               ▲         │                              │
               │         │  ┌──────────────────────┐    │
               │         │  │   FilterPipeline      │    │
               │         │  │                      │    │
               │         │  │  request_id          │    │
               │         │  │  guardrails          │    │
               │         │  │  headers             │    │
               │         │  │  ...                 │    │
               │         │  └──────────────────────┘    │
               │         │                              │
               ◄─────────┤  ProcessingResponse          │
                         └──────────────────────────────┘
```

Envoy owns networking: listeners, routing, upstream
connections, TLS termination, and load balancing. The
ExtProc server contributes policy (header mutation,
body inspection, request rejection) without replacing
the data plane.

## Module Structure

```text
src/
  lib.rs           # Crate root, module declarations
  bin/
    praxis_extproc.rs  # Binary entry point, CLI, startup
  adapter.rs       # ExtProc ↔ HttpFilterContext translation
  config.rs        # YAML config loading, pipeline construction
  error.rs         # Error types (thiserror)
  health.rs        # gRPC health check service
  metrics.rs       # Prometheus metrics endpoint
  response.rs      # ProcessingResponse builders + chunking
  server.rs        # ExternalProcessor gRPC implementation
  tls.rs           # TLS configuration for the gRPC listener
```

## Request Lifecycle

Each HTTP request flowing through Envoy produces a
single bidirectional gRPC stream. The ExtProc server
processes messages in the order Envoy sends them:

### 1. Request Headers

Envoy sends an `HttpHeaders` message with
pseudo-headers (`:method`, `:path`, `:authority`,
`:scheme`) and regular headers.

The adapter (`adapter.rs`) converts these into a
Praxis [`Request`] with parsed `Method`, `Uri`, and
`HeaderMap`. If `end_of_stream` is true (no body),
the pipeline runs immediately and replies with
header mutations. Otherwise, a passthrough response
is sent so Envoy proceeds to send the body.

[`Request`]: https://docs.rs/praxis-proxy-filter/latest/praxis_filter/struct.Request.html

### 2. Request Body

Body bytes arrive in one or more `HttpBody` messages.
Chunks are accumulated in a per-stream buffer (capped
at 10 MiB). When `end_of_stream` is true, the
pipeline runs with the full body. Filters can inspect,
mutate, or reject based on body content.

### 3. Response Headers

Envoy sends upstream response headers. The adapter
converts these into a Praxis [`Response`] with parsed
`StatusCode` and `HeaderMap`. Response-phase filters
run immediately so header mutations are included in
the reply (Envoy sends headers to the client after
receiving the ExtProc response; body-phase mutations
on headers would be too late).

[`Response`]: https://docs.rs/praxis-proxy-filter/latest/praxis_filter/struct.Response.html

### 4. Response Body

Same accumulation pattern as the request body. On
`end_of_stream`, response body filters run and
mutations are returned.

### 5. Trailers

Request and response trailers receive passthrough
responses (no filter hooks).

## Pipeline Execution

The server builds a [`FilterPipeline`] at startup
from the configured filter chains. The pipeline is
shared across all streams via `Arc`.

For each stream, a fresh [`HttpFilterContext`] is
constructed from the converted request. The pipeline
runs two phases:

1. **Request phase**: `execute_http_request` runs
   request filters, then `execute_http_request_body`
   runs body filters if body data is present.
2. **Response phase**: `execute_http_response` runs
   response filters, then `execute_http_response_body`
   runs response body filters.

Filter execution state (executed filter indices,
branch iteration counters) is preserved between
the request and response phases so that filters like
branch chains maintain correct state across the full
request lifecycle.

[`FilterPipeline`]: https://docs.rs/praxis-proxy-filter/latest/praxis_filter/struct.FilterPipeline.html
[`HttpFilterContext`]: https://docs.rs/praxis-proxy-filter/latest/praxis_filter/struct.HttpFilterContext.html

## Adapter Translation

The adapter layer (`adapter.rs`) bridges ExtProc
protobuf types and Praxis filter types:

| ExtProc Concept | Praxis Concept |
| --- | --- |
| `:method` pseudo-header | `Request.method` |
| `:path` pseudo-header | `Request.uri` |
| Regular headers | `Request.headers` / `Response.headers` |
| `:status` pseudo-header | `Response.status` |
| `x-forwarded-for` header | `HttpFilterContext.client_addr` |
| `extra_request_headers` | `HeaderMutation.set_headers` |
| `rewritten_path` | `:path` mutation |
| `FilterAction::Reject` | `ImmediateResponse` |

Routing fields (`cluster`, `upstream`) default to
`None` because Envoy owns routing decisions.

## Body Chunking

Envoy enforces a ~64 KiB limit per body chunk in
ExtProc responses. The `response` module splits
outbound body data into 62 KiB chunks (with a safety
margin) when body replacement is needed.

## Auxiliary Services

The server runs three listeners:

| Service | Default Port | Purpose |
| --- | --- | --- |
| gRPC (ExtProc) | 50051 | Main ExtProc protocol |
| Health | 50052 | gRPC health check (tonic-health) |
| Metrics | 9090 | Prometheus text exposition |

Health and metrics run on separate ports so Envoy and
Kubernetes can probe readiness without going through
the ExtProc protocol.

### Metrics

Three metrics are exported:

| Metric | Type | Description |
| --- | --- | --- |
| `praxis_extproc_requests_total` | counter | Total ExtProc streams |
| `praxis_extproc_immediate_responses_total` | counter | Rejection count |
| `praxis_extproc_request_duration_seconds` | histogram | Per-stream duration |

## TLS

The gRPC listener supports three TLS modes:

- **`none`** (default): plaintext gRPC
- **`self_signed`**: generates an ephemeral
  certificate at startup (development only)
- **`provided`**: loads PEM certificate and key from
  disk

See [Configuration](configuration.md) for TLS
settings.

## Graceful Shutdown

The server listens for `SIGTERM` and `SIGINT`. On
signal, the gRPC server stops accepting new streams
and drains in-flight connections. Health and metrics
servers shut down via a broadcast channel.
