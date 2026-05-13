# Development

Guide for building, testing, and contributing to
Praxis ExtProc.

## Prerequisites

- Rust stable 1.94+ (edition 2024)
- Rust nightly (for `rustfmt`; `group_imports` and
  `imports_granularity` are nightly-only)
- Docker or Podman (container builds)
- [KIND] (local Kubernetes testing)

[KIND]: https://kind.sigs.k8s.io/

## Build Commands

```console
make build       # cargo build
make release     # cargo build --release
make test        # cargo test
make lint        # clippy + nightly fmt check
make fmt         # cargo +nightly fmt
make doc         # rustdoc with -D warnings
make audit       # cargo audit + cargo deny check
make container   # build container image
```

Run a single test:

```console
cargo test -- test_name
```

## Project Structure

```text
src/
  lib.rs               # Crate root, module declarations
  bin/
    praxis_extproc.rs  # Binary entry point, CLI, startup
  adapter.rs           # ExtProc ↔ Praxis type translation
  config.rs            # YAML config loading, pipeline build
  error.rs             # Error types (thiserror)
  health.rs            # gRPC health check service
  metrics.rs           # Prometheus metrics endpoint
  response.rs          # ProcessingResponse builders + chunking
  server.rs            # ExternalProcessor gRPC implementation
  tls.rs               # TLS configuration
tests/
  grpc_server.rs       # In-process gRPC server tests
  services.rs          # Health and metrics service tests
  integration.rs       # Kubernetes integration tests
examples/
  praxis-extproc.yaml  # Example ExtProc configuration
  envoy.yaml           # Example Envoy configuration
  branch-chains.yaml   # Branch chain example
deploy/
  namespace.yaml       # Kubernetes namespace
  configmap.yaml       # ConfigMap with ExtProc config
  deployment.yaml      # Deployment manifest
  service.yaml         # Service manifest
  envoyfilter.yaml     # Istio EnvoyFilter for ExtProc
  echo.yaml            # Echo backend for testing
  gateway.yaml         # Istio Gateway for testing
  httproute.yaml       # HTTPRoute for testing
hack/
  kind-config.yaml     # KIND cluster configuration
  setup-kind.sh        # KIND cluster setup script
  smoke-test.sh        # End-to-end smoke test
  teardown-kind.sh     # KIND cluster teardown
```

## Local Development with KIND

Set up a full local environment with Istio and the
ExtProc server deployed:

```console
make dev-env
```

This creates a KIND cluster, installs Istio, deploys
the ExtProc server, and configures an EnvoyFilter to
wire Envoy's ext_proc filter to the server.

### Iterative Development

After the initial setup, rebuild and redeploy:

```console
make dev-push
```

This rebuilds the container image, loads it into
KIND, and restarts the deployment.

Run integration tests against the running cluster:

```console
make dev-integration
```

### Smoke Test

Run end-to-end verification:

```console
make smoke-test
```

Tear down:

```console
make kind-down
```

### Environment Variables

| Variable | Default |
| --- | --- |
| `KIND_CLUSTER_NAME` | `praxis-extproc` |
| `EXTPROC_IMAGE` | `praxis-extproc:dev` |

## Testing

### Unit Tests

```console
make test
```

Unit tests are embedded in each source module. They
cover config parsing, adapter translation, response
building, body chunking, and TLS configuration.

### gRPC Server Tests

`tests/grpc_server.rs` starts the ExtProc server
in-process and exercises the full gRPC stream
lifecycle: request headers, request body, response
headers, response body, trailers, and rejection
scenarios.

### Service Tests

`tests/services.rs` tests the health and metrics
auxiliary services independently.

### Integration Tests

Require a running Kubernetes cluster with Istio and
the ExtProc server deployed:

```console
make test-integration
```

Gated behind the `integration` feature flag. These
tests verify end-to-end behavior through Envoy's
ext_proc filter in a real cluster.

Run with verbose output:

```console
make test-integration V=1
```

## Container Build

```console
make container
```

Multi-stage build using `rust:1.94-alpine` for
compilation and `alpine:3.22` for the runtime image.
The binary is statically linked against musl and
stripped. The runtime image runs as a non-root user.

## CI

GitHub Actions workflow (`.github/workflows/tests.yaml`)
runs on every push and pull request:

- Format check (`cargo +nightly fmt --check`)
- Clippy (`cargo clippy -- -D warnings`)
- Tests (`cargo test`)
- Doc build (`RUSTDOCFLAGS="-D warnings" cargo doc`)
- Audit (`cargo audit`, `cargo deny check`)

## Coding Conventions

See [conventions.md](conventions.md) for the full
coding standards, including file ordering, test
conventions, separator comment format, and
documentation requirements.
