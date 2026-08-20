---
issue: https://github.com/opendatahub-io/praxis-extproc/issues/8
discussion: https://github.com/opendatahub-io/praxis-extproc/issues/8
status: proposed
authors:
  - noalimoy
---

# Envoy Parity and Streaming-Scale Qualification

## What?

Add the qualification suite that decides, with
reproducible evidence, whether Praxis can stand in
for the pinned IPP/BBR ExternalProcessor in front
of real Envoy.

The suite has two parts. A functional matrix runs
the pinned baseline fixtures through Envoy with
Praxis as the processor and compares externally
observable behavior. A performance and
streaming-scale matrix runs the same workloads
and records whether Praxis meets reviewed numeric
thresholds.

This is the evidence gate for epic [#2][epic],
not the feature work that fills the contract.
[#3][i3]–[#7][i7] implement protocol, profiles,
routing, translation, credentials, and packaging.
This proposal requires that those behaviors be
proven on the Envoy path at the concurrency
targets defined by the performance test plan,
and that the proof be machine-readable.

[epic]: https://github.com/opendatahub-io/praxis-extproc/issues/2
[i3]: https://github.com/opendatahub-io/praxis-extproc/issues/3
[i4]: https://github.com/opendatahub-io/praxis-extproc/issues/4
[i5]: https://github.com/opendatahub-io/praxis-extproc/issues/5
[i6]: https://github.com/opendatahub-io/praxis-extproc/issues/6
[i7]: https://github.com/opendatahub-io/praxis-extproc/issues/7

### Goals

- Exercise the supported request path through real
  Envoy, not only an in-process tonic client.
- Match the pinned IPP/BBR fixtures, or document
  and approve every intentional deviation as a
  product-contract change.
- Cover headers, bodies, trailers, mode overrides,
  immediate responses, cancellation, malformed
  sequences, route-cache clearing, fail-open /
  fail-closed, and supported MaaS profiles.
- Pass every approved performance and scale
  threshold for unary and streaming workloads.
- Demonstrate no unbounded memory growth under
  concurrency, long-lived SSE streams,
  cancellation, or backpressure.
- Emit machine-readable qualification results with
  exact Envoy, IPP baseline, Praxis, and fixture
  revisions recorded.

### Non-goals

- Implementing missing protocol or MaaS features.
  Those remain [#3][i3]–[#7][i7]. This suite
  qualifies what they ship.
- Choosing the numeric pass/fail thresholds. The
  performance test plan owns those numbers; this
  work makes passing them part of done.

## Why?

### Motivation

v1.0.0 cannot ship on component tests alone.
Envoy creates a bidirectional gRPC stream per
request, and streaming inference can keep that
stream open for the full SSE response. A correct
`ProcessingResponse` from an in-process client
does not prove that Envoy accepts the wire
behavior, that fail-open vs fail-closed matches
the configured `failure_mode_allow`, or that RSS
stays bounded at MaaS concurrency.

The current coverage does not answer that
question:

1. **In-process gRPC tests** (`tests/grpc_server.rs`)
   cover protocol correctness but have no Envoy in
   the path.

2. **KIND integration tests** (`tests/integration.rs`)
   confirm the happy path through a real gateway
   but are not a pinned fixture matrix with
   revision-locked comparisons.

3. **PR [#21][pr21] K8s e2e** puts a real proxy on
   unary and SSE cases but does not run pinned
   IPP/BBR fixtures, does not cover cancellation,
   fail-open / fail-closed, or security-negative
   cases, and emits no performance comparison.

4. **Observability** is limited to three Prometheus
   counters. There is no active-stream gauge, no
   memory-per-stream metric, no concurrency limit,
   and no soak test for unbounded growth.

Without this suite, substituting Praxis for IPP
is a judgment call. Epic [#2][epic] requires
pinned fixtures through real Envoy and measurable
scale criteria. [#8][i8] makes those criteria
part of the definition of done.

[pr21]: https://github.com/opendatahub-io/praxis-extproc/pull/21
[i8]: https://github.com/opendatahub-io/praxis-extproc/issues/8

### User Stories

- As a maintainer, I want a pinned functional
  matrix through real Envoy so that a release
  cannot claim IPP parity from in-process tests
  alone.

- As a MaaS operator, I want CPU, RSS,
  p50/p95/p99, TTFT/TTLT, and memory per active
  stream so that resource requests and replica
  counts come from measurements.

- As a gateway owner, I want fail-open,
  fail-closed, cancellation, and overload
  recovery recorded so that an incident is a
  known contract, not a surprise.

- As a reviewer of [#2][epic], I want
  machine-readable qualification output with
  revision pins so that "no regression observed"
  cannot replace approved thresholds.

## Open questions

1. **Pinned baseline version.** Which IPP
   commit/tag is the baseline? Upstream
   [v0.1.0][ipp-v010] is stable; downstream
   [ai-gateway-payload-processing][downstream]
   has no tagged release. Pinning downstream to
   a `main` SHA is fragile: any push changes
   the baseline and invalidates prior results.
   A tagged release or a frozen fork is needed
   for reproducible qualification.
2. **Performance test plan.** Who writes the
   numeric pass/fail thresholds, and when?
   Qualification cannot complete without them.
3. **Copy measurement.** The issue requires
   "avoidable copies" measurement. Specify the
   expected method (allocator instrumentation,
   benchmarks, or other).

[ipp-v010]: https://github.com/llm-d/llm-d-inference-payload-processor/releases/tag/v0.1.0
[downstream]: https://github.com/opendatahub-io/ai-gateway-payload-processing
