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

- `adapter`: ExtProc <-> `HttpFilterContext` translation
- `config`: YAML config loading (filter chains only)
- `error`: error types
- `health`: gRPC health check service
- `metrics`: Prometheus metrics endpoint
- `response`: `ProcessingResponse` builders + chunking
- `server`: `ExternalProcessor` gRPC implementation
- `tls`: TLS configuration for gRPC listener

## Conventions

Full conventions in [`docs/conventions.md`]. Key points:

- `#![deny(unsafe_code)]` in all crate roots
- All items (public and private) require `///` doc
  comments; enforced by `missing_docs` and
  `missing_docs_in_private_items` lints
- Clippy with `-D warnings --all-targets`
- Errors via `thiserror`; logging via `tracing`
- Comments answer "why?", never "what?"; use `tracing`
  for runtime narration
- Prefer `to_owned()` over `to_string()` for `&str`
  to `String`; `String::new()` for empty strings
- Use inline format args: `format!("{var}")`
- Use let-chains, `is_some_and()`, `strip_prefix()`
- Reference-style rustdoc links, not inline
- No re-export-only files
- Do not document memory efficiency in rustdoc
- Use enums for fixed value sets in config, not
  strings; `#[serde(deny_unknown_fields)]` on
  config structs; `#[serde(try_from)]` for
  constrained numerics; `#[serde(default)]`
  instead of `Option<T>` with `unwrap_or`.
  See `docs/conventions.md` "Type Design".

[`docs/conventions.md`]: docs/conventions.md

## File Ordering

1. Constants (with separator comment)
2. Public types, impls, functions
3. Private types and impls
4. Private utility functions (with separator)
5. `#[cfg(test)] mod tests` (always last)

Inside `mod tests`: imports, test functions, then test
utilities (with `// Test Utilities` separator).

Struct fields: `name` first (if present), then
alphabetical. Impl blocks: `new()` first, then
`name()`, then alphabetical. Blank line between each
documented field.

## Function Size

30-line threshold enforced by `clippy.toml`. Do not
suppress `too_many_lines` in production code; extract
helpers instead. Suppression is OK in test modules.
Prefer many small files and functions over fewer
large ones.

## Test Conventions

- Tests must verify precise behavior, not directional
  correctness.
- Never use inline comments in test function bodies.
  Explanatory text goes in assertion messages or
  `tracing::info!`/`debug!`/`trace!` calls.
- Do not add doc comments or regular comments on test
  functions. The function name is the documentation.
- Do not add per-test separator comments. Use one
  full-width separator to mark where tests begin.
- Use "Test Utilities" in separator comments, not
  "Helpers".
- All separator comments must be full-width (77
  dashes).
- Test utilities must stay inside `#[cfg(test)]`
  blocks.

## Separator Comments

Full-width separators (77 dashes) delineate logical
sections:

```rust
// -----------------------------------------------------------------------------
// Section Name
// -----------------------------------------------------------------------------
```

Never use short-form separators like
`// --- Section ---`.
