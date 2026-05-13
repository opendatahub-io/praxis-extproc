# Praxis External Processor

An [ExtProc] server to use [Praxis] filter pipelines in [Envoy].

Runs Praxis filters as an Envoy external processor,
enabling header/body inspection, mutation, and rejection
over gRPC without replacing the Envoy data plane.

[ExtProc]: https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter
[Praxis]: https://github.com/praxis-proxy/praxis
[Envoy]: https://github.com/envoyproxy/envoy
