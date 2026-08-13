# Configuration

The ExtProc server is configured via a YAML file
passed with the `-c` flag. The config defines
processing profiles and server settings; listeners
and clusters are omitted because Envoy owns
networking.

## Top-Level Structure

```yaml
pre_processing:
  - name: observability
    filters:
      - filter: request_id
      - filter: access_log

profiles:
  - name: default
    filter_chains:
      - name: security
        filters:
          - filter: guardrails
            rules:
              - target: body
                contains: "DROP TABLE"

post_processing:
  - name: transformation
    filters:
      - filter: headers
        response_set:
          - name: X-Proxy
            value: praxis-extproc

server:
  grpc_address: "0.0.0.0:50051"
  health_address: "0.0.0.0:50052"
  metrics_address: "0.0.0.0:9090"
  tls:
    mode: none

insecure_options:
  allow_unbounded_body: true
```

## Processing Pipeline

Requests flow through a three-tier pipeline:
**pre-processing** → **profile** → **post-processing**.

- **`pre_processing`**: filter chains that run before
  profile selection. Use for authentication,
  observability, and request identification.
- **`profiles`**: named processing profiles, each with
  its own filter chains. Exactly one profile is
  selected per request.
- **`post_processing`**: filter chains that run after
  the selected profile completes. Use for response
  transformation and logging.

All three sections are optional. Each contains named
filter chains that are concatenated in order to form
a pipeline for that tier.

Response processing runs in reverse tier order:
post → profile → pre.

## Filter Chains

Named filter chains defined within each tier.
Chains are concatenated in order to form the tier's
pipeline. This matches the [Praxis] filter chain
model and supports the same filters.

```yaml
profiles:
  - name: default
    filter_chains:
      - name: security
        filters:
          - filter: guardrails
            rules:
              - target: header
                name: "User-Agent"
                pattern: "bad-bot.*"

      - name: transformation
        filters:
          - filter: headers
            request_add:
              - name: X-Processed-By
                value: praxis-extproc
            response_set:
              - name: X-Proxy
                value: praxis-extproc
```

The security chain runs first, then transformation.
Filters within each chain execute in order.

[Praxis]: https://github.com/praxis-proxy/praxis

### Available Filters

All built-in Praxis HTTP filters are available in
ExtProc mode, plus in-tree [Praxis AI] filters from
`praxis-ai-filters`. Commonly used filters:

| Filter | Description |
| --- | --- |
| `request_id` | Generate or propagate request IDs |
| `access_log` | Structured JSON access logging |
| `headers` | Add, set, or remove headers |
| `guardrails` | Reject requests matching string or regex rules |
| `ip_acl` | Allow or deny by source IP/CIDR |
| `forwarded_headers` | Inject `X-Forwarded-*` headers |
| `cors` | CORS preflight and origin validation |
| `csrf` | CSRF protection via origin validation |
| `rate_limit` | Token bucket rate limiting |
| `json_body_field` | Extract JSON body field to header |
| `path_rewrite` | Rewrite request path |
| `url_rewrite` | Regex path + query rewriting |
| `model_to_header` | Promote JSON `model` field to a request header |
| `prompt_enrich` | Prepend/append chat messages |
| `token_count` | Count tokens for a provider |
| `ai_guardrails` | AI content guardrails |
| `openai_*` / `anthropic_*` | Provider API format, validate, and stream filters |

See the [Praxis filter documentation] for core filters
and the [Praxis AI filter documentation] for AI filter
names and configuration options.

[Praxis AI]: https://github.com/praxis-proxy/ai
[Praxis filter documentation]: https://github.com/praxis-proxy/praxis/blob/main/docs/filters.md
[Praxis AI filter documentation]: https://github.com/praxis-proxy/ai/blob/main/docs/filters/README.md

### Branch Chains

Filter chains support conditional branching via the
`branches` field. Branch chains execute based on
filter results, enabling conditional logic within
the pipeline.

```yaml
profiles:
  - name: default
    filter_chains:
      - name: main
        filters:
          - filter: guardrails
            name: content_check
            rules:
              - target: body
                contains: "blocked-content"
            branches:
              - chain:
                  filters:
                    - filter: headers
                      request_add:
                        - name: X-Content-Blocked
                          value: "true"
                on_result:
                  filter: content_check
                  key: rejected
                  value: "true"
```

See [branch-chains.yaml] for a working example.

[branch-chains.yaml]: ../examples/branch-chains.yaml

### Conditions

Filters support `when` and `unless` conditions for
request predicates (`path`, `path_prefix`, `methods`,
`headers`) and `response_conditions` for response
predicates (`status`, `headers`).

## Server

The `server` section configures bind addresses and
TLS for the three listeners.

```yaml
server:
  grpc_address: "0.0.0.0:50051"
  health_address: "0.0.0.0:50052"
  metrics_address: "0.0.0.0:9090"
  tls:
    mode: none
```

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `grpc_address` | string | `0.0.0.0:50051` | gRPC ExtProc listen address |
| `health_address` | string | `0.0.0.0:50052` | gRPC health check address |
| `metrics_address` | string | `0.0.0.0:9090` | Prometheus metrics address |
| `tls` | object | `mode: none` | TLS configuration |

### TLS

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `mode` | string | `none` | `none`, `self_signed`, or `provided` |
| `cert_path` | string | — | PEM certificate path (required for `provided`) |
| `key_path` | string | — | PEM private key path (required for `provided`) |
| `ca_cert_path` | string | — | PEM CA cert for mTLS client verification (`provided` only) |
| `handshake_concurrency` | integer | `64` | Maximum simultaneous in-flight TLS handshakes |
| `handshake_timeout_secs` | integer | `10` | Per-handshake deadline in seconds; stalled connections are dropped |

TLS modes:

- **`none`**: plaintext gRPC. Use when Envoy
  connects over localhost or a trusted network.
- **`self_signed`**: generates an ephemeral
  self-signed certificate at startup. Useful for
  development and testing.
- **`provided`**: loads certificate and key from
  disk. Use for production deployments where the
  Envoy-to-ExtProc link must be encrypted.

When `ca_cert_path` is set, the server requires
clients to present a valid certificate signed by
that CA (mTLS). Only available in `provided` mode.

`handshake_concurrency` bounds how many TLS
handshakes run in parallel; when all slots are
occupied, new TCP accepts stall until a slot frees.
`handshake_timeout_secs` cancels any single
handshake that exceeds the deadline, freeing its
slot for the next connection. Both values must be
greater than zero.

```yaml
server:
  tls:
    mode: provided
    cert_path: /etc/tls/cert.pem
    key_path: /etc/tls/key.pem
    ca_cert_path: /etc/tls/ca.pem   # optional: enables mTLS
    handshake_concurrency: 64        # optional: default shown
    handshake_timeout_secs: 10       # optional: default shown
```

## Insecure Options

Development overrides under `insecure_options:`.
These relax safety validations and emit warnings at
startup.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `allow_unbounded_body` | bool | `false` | Allow unlimited body accumulation |

```yaml
insecure_options:
  allow_unbounded_body: true
```

## CLI Options

The binary accepts CLI flags that override config
values:

```console
praxis-extproc [OPTIONS]

Options:
  -c, --config <PATH>            Config file path
                                 [default: praxis-extproc.yaml]
      --grpc-address <ADDR>      Override gRPC listen address
      --health-address <ADDR>    Override health check address
      --metrics-address <ADDR>   Override metrics address
  -t, --validate                 Validate config and exit
  -h, --help                     Print help
  -V, --version                  Print version
```

### Validate Configuration

Check that a config file parses correctly and all
filters resolve without starting the server:

```console
praxis-extproc -t -c praxis-extproc.yaml
```

### Logging

Logging uses `tracing` with `RUST_LOG` env-filter
syntax. Default level is `info`.

```console
RUST_LOG=debug praxis-extproc -c praxis-extproc.yaml
RUST_LOG=praxis_extproc::server=trace praxis-extproc -c praxis-extproc.yaml
```

## Environment Variables

| Variable | Description |
| --- | --- |
| `RUST_LOG` | Tracing filter (e.g. `info`, `debug`, `praxis_extproc=trace`) |

## Example Configs

Working examples in the `examples/` directory:

| File | Description |
| --- | --- |
| [praxis-extproc.yaml] | Common filters: request ID, access log, guardrails, headers |
| [ai-model-to-header.yaml] | AI `model_to_header` with request ID and headers |
| [envoy.yaml] | Envoy config wiring up the ExtProc filter |
| [branch-chains.yaml] | Conditional branching on filter results |

[praxis-extproc.yaml]: ../examples/praxis-extproc.yaml
[ai-model-to-header.yaml]: ../examples/ai-model-to-header.yaml
[envoy.yaml]: ../examples/envoy.yaml
[branch-chains.yaml]: ../examples/branch-chains.yaml

## Error Behavior

The server fails fast at startup for configuration
problems:

- **Invalid YAML or missing fields**: the process
  exits with a descriptive error.
- **Unknown filter name**: pipeline construction
  fails with the unrecognized filter name.
- **TLS certificate load failure**: the process exits
  if `cert_path`, `key_path`, or `ca_cert_path`
  cannot be read, or if the certificate and key do
  not match each other.
- **Invalid TLS values**: `handshake_concurrency`
  or `handshake_timeout_secs` set to zero cause an
  immediate startup error.
- **Address bind failure**: the server fails to start
  if any listen address is already in use.

At runtime:

- **Filter error**: an `Err` from a filter produces
  a gRPC `INTERNAL` status on the stream.
- **Body too large**: exceeding the 10 MiB
  accumulation limit produces a gRPC
  `RESOURCE_EXHAUSTED` status.
- **Filter rejection**: a `FilterAction::Reject`
  returns an `ImmediateResponse` to Envoy, which
  sends the rejection directly to the client.
