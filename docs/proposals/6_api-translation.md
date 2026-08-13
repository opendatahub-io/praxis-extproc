---
issue: https://github.com/opendatahub-io/praxis-extproc/issues/6
discussion:
status: proposed
authors:
  - mkoushni
---

# API Translation

## What?

Introduce a provider-aware translation stage to the Praxis filter pipeline
that bidirectionally rewrites inference traffic between the
consumer-facing contract and provider-specific wire formats. The stage
operates after authorization and route selection have completed, covers
both buffered and streaming (SSE) responses, and produces a deterministic,
ordered set of header and body mutations.

Translation is a **pure protocol concern**. Credential injection is out of
scope and is addressed as a separate pipeline stage (see issue #6,
provider-credential sub-task). The two stages are designed to compose —
translation runs first, credential injection follows — so that the
translation layer never needs access to secret material.

### Goals

- **Bidirectional format translation.** Rewrite consumer inference
  requests to the wire format required by the selected provider, and
  rewrite provider responses back to the canonical consumer-facing schema.
  Providers in scope for the first release: OpenAI, Anthropic, Vertex AI,
  Bedrock, and any other provider included in the pinned release fixtures.

- **Streaming / SSE correctness.** Handle streaming and SSE responses
  across arbitrary chunk boundaries. Event framing, ordering, and the
  `data:` / `[DONE]` envelope must be preserved through translation
  without buffering the full stream.

- **Deterministic mutation ordering.** Header and body mutations produced
  by translation must be applied in a fixed, documented order. This makes
  the translation pipeline predictable, independently testable, and safe
  to compose with other filter stages.

- **Consumer credential removal.** Any consumer-supplied provider
  credential present in the request (header or body) must be stripped
  during this stage, before the credential injection stage runs. This
  establishes a clean security boundary and ensures the runtime is the
  sole authority on which credential reaches a provider.

- **Fixture-backed correctness.** Every provider in scope must have
  golden fixtures covering: normal request/response, error response, and
  streaming/SSE. Fixtures are the acceptance gate — a translation is
  correct when its output matches the fixture, not when it passes a
  unit test written from the same assumptions as the implementation.

- **Fail closed on untranslatable input.** If the translation stage
  cannot produce a valid provider request (missing required field,
  unsupported model parameter, schema mismatch), it must reject the
  request with a stable error response rather than forwarding a malformed
  request to the provider.

- **Authorization ordering enforced.** Translation must not execute
  before the authorization stage has completed. The pipeline stage
  ordering must be statically enforced, not a runtime assumption.

### Non-Goals

- **Credential injection and management.** Cloud token generation, API
  key injection, token caching, and refresh are handled by the separate
  provider-credential stage.
- **Routing and authorization.** Provider selection and caller
  authorization are upstream concerns, already covered by existing Praxis
  stages.
- **Feature parity with every provider capability.** The scope is the
  pinned release fixtures, not a complete adapter for every provider
  endpoint or extension.
- **Consumer-to-consumer format bridging.** This proposal covers
  consumer-to-provider and provider-to-consumer translation only. It does
  not introduce a canonical intermediate representation as a new
  public API surface.

## Why?

### Motivation

Each inference provider exposes an incompatible proprietary API: field
names, request envelope shapes, error codes, streaming event schemas,
required headers, and authentication conventions all differ across
OpenAI, Anthropic, Vertex AI, and Bedrock. Praxis can already select a
provider by routing, but the routed request still carries the consumer's
wire format. Without a translation layer, one of three failure modes
applies:

1. **Consumer complexity.** Every calling application must implement
   provider-specific adapters, exposing business code to the full surface
   area of every provider and making provider migration an application
   change.
2. **Operational fragmentation.** Operators run separate ingress
   endpoints per provider, preventing unified policy enforcement,
   observability, and traffic shaping.
3. **Credential leakage.** Without an explicit strip step, consumer-
   supplied provider credentials (e.g. an `Authorization` header from a
   development client) may be forwarded to the provider, bypassing the
   runtime's credential authority.

A translation layer eliminates all three failure modes by making provider
heterogeneity an infrastructure concern:

- **Consumer stability.** Applications code to one stable inference
  contract. Provider migrations are configuration changes, not code
  changes.
- **Operational coherence.** A single gateway endpoint handles all
  providers. Policy, rate limiting, observability, and routing all
  operate uniformly.
- **Security boundary.** The translation stage is the defined point where
  consumer credentials are stripped and the pipeline is handed off to
  the credential injection stage. No other stage needs to reason about
  this responsibility.
- **Testability.** Deterministic, ordered mutations with golden fixtures
  mean translation correctness can be verified independently of the
  routing, authorization, and credential stages.

### User Stories

- As an **AI application developer**, I want to send requests in a stable
  consumer-facing format regardless of which provider backs my model,
  so that I can migrate between providers without changing my application
  code or adding provider-specific logic to my client.

- As a **platform operator**, I want provider format translation to be
  handled automatically by the gateway after a route is selected, so
  that I can add, remove, or swap providers through configuration without
  requiring any change to consumer-side code or interfaces.

- As a **platform operator**, I want streaming and SSE responses to be
  translated transparently — preserving event framing and ordering — so
  that clients using streaming inference observe consistent behavior
  regardless of which provider served the request.

- As a **security engineer**, I want consumer-supplied provider
  credentials stripped during the translation stage before any authorized
  credential is injected, so that the runtime is the sole authority on
  which credential reaches a provider and consumers cannot influence
  provider authentication.

- As a **security engineer**, I want translation to fail closed when
  input cannot be translated to a valid provider request, so that
  malformed or unexpected consumer payloads never reach a provider in an
  undefined state.

- As a **platform engineer**, I want every provider translation to be
  covered by golden request, response, error, and streaming fixtures,
  so that regressions in format correctness are caught before reaching
  production, independently of provider availability.
