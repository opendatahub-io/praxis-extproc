# Configuration

The ExtProc server is configured via a YAML file
passed with the `-c` flag. The config defines filter
chains and server settings; listeners and clusters
are omitted because Envoy owns networking.

## Top-Level Structure

```yaml
filter_chains:
  - name: security
    filters:
      - filter: guardrails
        rules:
          - target: body
            contains: "DROP TABLE"

  - name: observability
    filters:
      - filter: request_id
      - filter: access_log

server:
  grpc_address: "0.0.0.0:50051"
  health_address: "0.0.0.0:50052"
  metrics_address: "0.0.0.0:9090"
  tls:
    mode: none

insecure_options:
  allow_unbounded_body: true
```

## Filter Chains

Named filter chains defined under `filter_chains:`.
All chains are concatenated in order to form a single
pipeline. This matches the [Praxis] filter chain model
and supports the same filters.

```yaml
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
ExtProc mode. Commonly used filters:

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

See the [Praxis filter documentation] for the full
list and configuration options.

[Praxis filter documentation]: https://github.com/praxis-proxy/praxis/blob/main/docs/filters.md

### Branch Chains

Filter chains support conditional branching via the
`branches` field. Branch chains execute based on
filter results, enabling conditional logic within
the pipeline.

```yaml
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
| `cert_path` | string | none | PEM certificate path (required for `provided`) |
| `key_path` | string | none | PEM private key path (required for `provided`) |

TLS modes:

- **`none`**: plaintext gRPC. Use when Envoy
  connects over localhost or a trusted network.
- **`self_signed`**: generates an ephemeral
  self-signed certificate at startup. Useful for
  development and testing.
- **`provided`**: loads certificate and key from
  disk. Use for production deployments where the
  Envoy-to-ExtProc link must be encrypted.

```yaml
server:
  tls:
    mode: provided
    cert_path: /etc/tls/cert.pem
    key_path: /etc/tls/key.pem
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
| [envoy.yaml] | Envoy config wiring up the ExtProc filter |
| [branch-chains.yaml] | Conditional branching on filter results |

[praxis-extproc.yaml]: ../examples/praxis-extproc.yaml
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
  if `cert_path` or `key_path` cannot be read.
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
