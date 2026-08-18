[![Tests](https://github.com/opendatahub-io/praxis-extproc/actions/workflows/tests.yaml/badge.svg)](https://github.com/opendatahub-io/praxis-extproc/actions/workflows/tests.yaml)
[![MSRV: 1.94](https://img.shields.io/badge/MSRV-1.94-brightgreen.svg)](https://blog.rust-lang.org/)
[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

# Praxis ExtProc

[Envoy] [ExtProc] server that runs [Praxis] filter
pipelines as an external processor. Enables header and
body inspection, mutation, and rejection over gRPC
without replacing Envoy.

See [Getting Started](docs/getting-started.md) to
start deploying alongside Envoy.

[Envoy]: https://github.com/envoyproxy/envoy
[ExtProc]: https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter
[Praxis]: https://github.com/praxis-proxy/praxis

## Documentation

- [Getting Started](docs/getting-started.md): deploy and run in minutes
- [Architecture](docs/architecture.md): how the ExtProc server works
- [Configuration](docs/configuration.md): YAML reference for filter chains, server, and TLS
- [IPP → Praxis Migration](docs/migration-ipp-to-praxis.md): moving from the Go Inference Payload Processor
- [Development](docs/development.md): building, testing, contributing
- [Conventions](docs/conventions.md): coding standards

## Contributing

[Issues] and [pull requests] are welcome. Familiarize
yourself with the following documentation first:

- [Architecture](docs/architecture.md)
- [Conventions](docs/conventions.md)
- [Development](docs/development.md)

For larger changes, open a [discussion] and follow
the [proposal process](docs/proposals.md).

[Issues]: https://github.com/opendatahub-io/praxis-extproc/issues/new
[pull requests]: https://github.com/opendatahub-io/praxis-extproc/compare
[discussion]: https://github.com/opendatahub-io/praxis-extproc/discussions
