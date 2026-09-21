# CLAUDE.md

This file provides guidance to Claude Code
(claude.ai/code) when working with code in this
repository.

## Project

Envoy ExtProc server for [Praxis], running Praxis
filter pipelines as an external processing service
for Envoy proxy.

[Praxis]: https://github.com/praxis-proxy/praxis

## Requirements

- Rust stable 1.94+
- Rust nightly (for `rustfmt`)

## Quick Reference

```console
make build          # workspace build
make test           # all tests
make fmt            # format with nightly rustfmt
make lint           # clippy + nightly fmt check
make audit          # cargo audit + cargo deny check
make doc            # rustdoc with -D warnings
make container      # container image build
```

Run a single test:

```console
cargo test -- test_name
```

## Architecture

Standalone gRPC server that translates Envoy ExtProc
messages into Praxis filter pipeline invocations.

```text
Envoy -> [gRPC] -> praxis-extproc -> FilterPipeline
```

**Module structure:**

- `adapter`: ExtProc <-> `HttpFilterContext`
  translation
- `config`: YAML config loading (filter chains only)
- `error`: error types
- `health`: gRPC health check service
- `metrics`: Prometheus metrics endpoint
- `response`: `ProcessingResponse` builders +
  chunking
- `server`: `ExternalProcessor` gRPC implementation
- `tls`: TLS configuration for gRPC listener

## Conventions

Full conventions in [`docs/conventions.md`].
Project-specific additions beyond the user-level
Rust Baseline:

- Use enums for fixed value sets in config, not
  strings; `#[serde(deny_unknown_fields)]` on
  config structs; `#[serde(try_from)]` for
  constrained numerics; `#[serde(default)]`
  instead of `Option<T>` with `unwrap_or`.

[`docs/conventions.md`]: docs/conventions.md

## Function Size

30-line threshold enforced by `clippy.toml`. Do not
suppress `too_many_lines` in production code; extract
helpers instead. Suppression is OK in test modules.
