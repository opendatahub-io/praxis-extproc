---
issue: https://github.com/opendatahub-io/praxis-extproc/issues/6
discussion: >-
  Sub-task of epic issue #6, which was opened from the project discussion
  phase and serves as the approved discussion artifact for all child
  proposals. No separate GitHub Discussion was opened for this sub-task;
  the epic issue is considered sufficient per maintainer agreement.
  See https://github.com/opendatahub-io/praxis-extproc/issues/6
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

The translation stage derives the target provider exclusively from the
trusted route result written into stream state by the authorization and
routing stage. Provider identity is never inferred from consumer-controlled
inputs such as model names, URI paths, or request headers. If the trusted
route result is absent or names a provider not in the configured allowlist,
the stage rejects the request and halts the pipeline.

Translation is a **pure protocol concern**. Credential injection is out of
scope and is addressed as a separate pipeline stage (see issue #6,
provider-credential sub-task). The two stages are designed to compose —
translation runs first, credential injection follows — so that the
translation layer never needs access to secret material.

### Goals

- **Bidirectional format translation.** Rewrite consumer inference
  requests to the wire format required by the selected provider, and
  rewrite provider responses back to the canonical consumer-facing schema.
  Supported providers are defined by a versioned, operator-configured
  allowlist. Each allowlist entry must declare: the provider identifier,
  transport protocol, credential-bearing locations (headers, query
  parameters, body fields), request and response schemas, and a fixture
  manifest. Providers for the first release: OpenAI, Anthropic, Vertex
  AI, and Bedrock. Adding a provider requires an explicit allowlist entry
  with all required fields — fixture inclusion alone does not grant
  support scope.

- **Streaming correctness with transport-specific framing.** Provider
  streaming transports are not uniform and must be handled with
  transport-specific decoders rather than a single SSE path:

  - **OpenAI / Anthropic** use Server-Sent Events (`text/event-stream`):
    `data:` lines, blank-line delimiters, and `data: [DONE]` termination.
  - **Amazon Bedrock** (`InvokeModelWithResponseStream`) uses
    `application/vnd.amazon.eventstream` binary framing: length-prefixed
    messages with headers, payload, and CRC32 checksums. This is not SSE
    and must not be processed through the SSE decoder. In-stream exception
    events and normal stream completion must both be mapped to the
    consumer contract.
  - **Vertex AI** uses its own event-stream framing and must be handled
    separately.

  Each transport requires its own decoder with fixtures covering: normal
  chunk events, stream completion, and in-stream exception or error events.

  All decoders must operate **incrementally**: maintain only a bounded
  incomplete-frame buffer, emit complete events as soon as they are
  recognised, and never buffer the full stream. The current ExtProc server
  accumulates the full response body (up to 10 MiB) before running
  filters; the streaming translation path must define explicit parser state
  that avoids this full-buffer model and handles valid end-of-stream
  termination correctly regardless of chunk boundaries.

- **Deterministic mutation ordering.** Header and body mutations produced
  by translation must be applied in a fixed, documented order. This makes
  the translation pipeline predictable, independently testable, and safe
  to compose with other filter stages.

- **Consumer credential removal.** Consumer-supplied provider credentials
  must be stripped across every location where a provider can accept them,
  before the credential injection stage runs. The scope is per-provider
  and covers: authentication headers (e.g. `Authorization`, `x-api-key`,
  `X-Goog-Api-Key`), SigV4 query parameters (e.g. `X-Amz-Credential`,
  `X-Amz-Signature`, `X-Amz-Security-Token`, `X-Amz-Algorithm`), URI
  path components that embed identity or session tokens, and any nested
  body fields used by the provider for authentication. Each provider's
  complete credential-bearing surface must be enumerated in a fixture that
  asserts the final forwarded request contains only runtime-injected
  credentials and nothing consumer-supplied.

- **Fixture-backed correctness with provenance and negative coverage.**
  Fixtures are the acceptance gate — a translation is correct when its
  output matches the fixture, not when it passes a unit test written from
  the same assumptions as the implementation. To prevent a stale or
  incomplete contract from passing the gate, fixtures must satisfy the
  following requirements:

  - **Provenance.** Each fixture records the provider API version, model
    schema version, and the source of truth it was derived from (e.g.
    provider SDK test suite, live endpoint capture, specification). Fixture
    updates require a corresponding provenance update.
  - **Secret scan.** Fixtures must be scanned for credential material
    before merge. Any fixture containing a real key, token, or signature
    must be rejected.
  - **Positive coverage.** Normal request/response, error response, all
    supported streaming transports (SSE, Bedrock event-stream), and normal
    end-of-stream termination.
  - **Negative coverage.** Unsupported or unknown request fields,
    malformed stream frames, arbitrary chunk splits across frame boundaries,
    consumer-supplied credentials in every credential-bearing location
    (asserting they are absent from the forwarded request), in-stream
    exception and error events, and mid-stream provider failures. A
    negative fixture that does not assert a rejection is not a negative
    fixture.

- **Fail closed on untranslatable input.** If the translation stage
  cannot produce a valid provider request (missing required field,
  unsupported model parameter, schema mismatch), it must reject the
  request with a stable error response rather than forwarding a malformed
  request to the provider.

- **Trusted provider context, explicit allowlist.** The translation stage
  must operate from an explicit, operator-configured allowlist of known
  providers. Provider identity is read exclusively from the trusted route
  result in stream state — it must never be inferred or overridden from
  consumer-controlled inputs (model names, URI paths, headers, or body
  fields). A missing route result, an unrecognized provider identifier,
  or a conflict between route-state values must cause an immediate
  rejection before any translation or mutation is applied.

- **Authorization ordering enforced.** Translation must not execute
  before the authorization stage has completed. The enforcement strategy
  will be detailed in the How? section.

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
  credentials stripped across every credential-bearing location —
  headers, SigV4 query parameters, URI path tokens, and body fields —
  before any authorized credential is injected, so that the runtime is
  the sole authority on which credential reaches a provider regardless
  of which transport mechanism the consumer used to supply it.

- As a **security engineer**, I want provider identity derived
  exclusively from the trusted route result stored in stream state, and
  never inferred from consumer-controlled model names, URI paths, or
  headers, so that a consumer cannot steer the translation stage toward
  a provider they are not authorized to reach.

- As a **security engineer**, I want translation to fail closed when
  input cannot be translated to a valid provider request, so that
  malformed or unexpected consumer payloads never reach a provider in an
  undefined state.

- As a **platform engineer**, I want every provider translation to be
  covered by golden request, response, error, and streaming fixtures,
  so that regressions in format correctness are caught before reaching
  production, independently of provider availability.
