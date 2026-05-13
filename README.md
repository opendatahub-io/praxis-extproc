# Praxis ExtProc

[![Tests](https://github.com/praxis-proxy/extproc/actions/workflows/tests.yaml/badge.svg)](https://github.com/praxis-proxy/extproc/actions/workflows/tests.yaml)
[![MSRV: 1.94](https://img.shields.io/badge/MSRV-1.94-brightgreen.svg)](https://blog.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

[Envoy] [ExtProc] server that runs [Praxis] filter
pipelines as an external processor. Enables header and
body inspection, mutation, and rejection over gRPC
without replacing Envoy.

[Envoy]: https://github.com/envoyproxy/envoy
[ExtProc]: https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter
[Praxis]: https://github.com/praxis-proxy/praxis

## Quick Start

```console
make build       # build
make test        # test
make lint        # clippy + fmt check
make container   # container image
```

See [Getting Started](docs/getting-started.md) for
deploying alongside Envoy.

## Documentation

- [Getting Started](docs/getting-started.md):
  deploy and run in minutes
- [Architecture](docs/architecture.md):
  how the ExtProc server works
- [Configuration](docs/configuration.md):
  YAML reference for filter chains, server, and TLS
- [Development](docs/development.md):
  building, testing, contributing
- [Conventions](docs/conventions.md):
  coding standards

## Contributing

[Issues] and [pull requests] are welcome. Familiarize
yourself with the following documentation first:

- [Architecture](docs/architecture.md)
- [Conventions](docs/conventions.md)
- [Development](docs/development.md)

For larger changes, open a [discussion] and follow
the [proposal process](docs/proposals.md).

[Issues]: https://github.com/praxis-proxy/extproc/issues/new
[pull requests]: https://github.com/praxis-proxy/extproc/compare
[discussion]: https://github.com/praxis-proxy/extproc/discussions
