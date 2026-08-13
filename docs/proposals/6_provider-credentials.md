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

# Provider Credential Generation

## What?

Introduce a provider credential stage to the Praxis filter pipeline that
resolves, injects, and manages the credentials required to authenticate
the final hop from the gateway to a selected provider. The stage operates
after API translation has completed, never trusts consumer-supplied
provider credentials, and fails closed on any unresolvable, expired, or
invalid credential path.

Credential material is classified as a **runtime secret**. It never
appears in configuration state, routing decisions, logs, traces, metrics,
or diagnostic output. Configuration carries only opaque credential
references; the runtime resolves references to values and the values
remain local to the authorized runtime for the duration of their
validity window.

### Goals

- **Reference-only configuration.** Credential configuration carries
  references (e.g. a secret name, a service-account path, a mounted
  volume key) — never literal secret values. Resolution from reference
  to value occurs at runtime inside the authorized credential store, not
  at configuration load time.

- **Provider authentication coverage.** Support the authentication
  mechanism required by every provider in the pinned release contract:
  static API-key injection (OpenAI, Anthropic), GCP service-account
  OAuth2 access tokens (Vertex AI), AWS SigV4 request signing (Bedrock),
  and any other mechanism required by pinned fixtures. New providers must
  not require changes to the credential-stage interface, only to the
  provider-specific resolver.

- **No-consumer-credential precondition.** Primary removal of consumer-
  supplied provider credentials (headers, query parameters, body fields)
  is owned by the API translation stage (see `6_api-translation.md`).
  The credential stage treats a clean request as a precondition: it must
  verify that no consumer-supplied credential remains in any
  credential-bearing location before injecting the authorized credential.
  If a consumer credential is still present when this stage executes —
  indicating the translation stage did not run or was bypassed — the
  request must be rejected. The credential stage must not overwrite or
  shadow a consumer credential with an authorized one; it must refuse to
  inject until the consumer credential is absent. This defense-in-depth
  check ensures that a misconfigured or bypassed translation stage cannot
  cause consumer credentials to reach a provider.

- **Bounded token lifecycle management.** Cloud-generated credentials
  (OAuth2 access tokens, STS session tokens) have finite validity
  windows. The stage must: cache tokens to avoid per-request generation
  overhead; refresh proactively before expiry within a configurable
  lead time; bound concurrent refresh attempts to prevent thundering-
  herd on token endpoint; and treat refresh failure as a fail-closed
  event rather than serving a stale or missing token.

- **Fail closed on every unresolvable path.** A missing reference, a
  secret that cannot be read, an expired token that cannot be refreshed,
  a provider whose auth mechanism is not configured — all must result in
  a stable rejection response to the consumer. The gateway must never
  forward a request to a provider with a missing, partial, or stale
  credential.

- **Full redaction across all observability surfaces.** Credential
  values must be absent from logs, distributed traces, metrics labels,
  configuration dumps, health or diagnostic endpoints, and error
  responses. Redaction must be enforced structurally, not by convention
  or review.

- **Authorization ordering enforced.** The credential stage must
  execute only after the authorization stage has granted the caller
  access to the selected provider. This ordering must be statically
  enforced in the pipeline, not assumed at runtime.

### Non-Goals

- **API format translation.** Request and response reshaping between
  consumer and provider wire formats is handled by the separate
  API translation stage (see `docs/proposals/6_api-translation.md`).
- **Route selection and caller authorization.** Provider selection and
  caller identity verification are upstream concerns handled by existing
  Praxis stages.
- **Consumer identity credential management.** This stage manages
  credentials for authenticating the gateway to providers, not
  credentials used to authenticate consumers to the gateway.
- **Secret storage and rotation.** The stage resolves credentials from
  external stores (Kubernetes secrets, mounted volumes, cloud secret
  managers). It is not responsible for creating, rotating, or auditing
  those stores.
- **Consumer-authorized pass-through.** Forwarding a consumer-supplied
  provider credential when the product contract explicitly authorizes
  it is a separate capability not addressed here. Until that contract
  is defined and authorized, consumer credentials are always stripped.

## Why?

### Motivation

Authorizing a route and selecting a provider does not automatically
produce a request that a provider will accept. Providers require
authentication: static API keys, short-lived OAuth2 access tokens,
SigV4-signed requests, or other mechanisms. Without a dedicated
credential stage, the gateway must either trust consumer-supplied
credentials — creating a security failure — or embed credential
logic into routing, translation, or other pipeline stages — creating
maintainability and auditability failures.

A dedicated credential stage addresses these risks with a clear
division of responsibility:

**Security boundary.** Consumer-supplied provider credentials are an
active threat vector. A consumer who has obtained or guessed a provider
API key can bypass the gateway's quota, policy, and cost controls by
embedding that key in a request and having it forwarded. Unconditional
stripping — before authorized injection — closes this vector
structurally rather than by policy enforcement, which is fragile.

**Credential confinement.** Cloud-generated tokens (OAuth2 access tokens,
AWS STS session credentials) are high-value, short-lived secrets. Allowing
them to propagate into routing state, log pipelines, or diagnostic output
creates exfiltration surface. Treating credential values as runtime-only
material that never enters configuration or observability data is the
only way to enforce confinement at scale.

**Operational reliability.** Per-request token generation is expensive
and adds latency proportional to the provider's token endpoint response
time. Without caching, a token endpoint outage translates directly to a
gateway outage. Without bounded concurrency on refresh, a token expiry
under load produces a thundering-herd on the token endpoint, compounding
the problem. Bounded caching with proactive refresh decouples provider
token endpoint availability from request-path latency.

**Auditability.** When credential management is spread across multiple
pipeline stages, determining which credential was used for a given
request — and whether it was the authorized one — requires correlating
evidence across multiple systems. A single credential stage with
structured tracing (reference used, not value; resolution outcome;
injection point) makes this audit straightforward.

**Composability.** The translation and credential stages are designed
to compose without coupling. Translation produces a structurally correct
provider request with consumer credentials removed. The credential stage
then injects the authorized credential into the clean request. Neither
stage needs to reason about the other's internal logic, and the two can
be tested, deployed, and evolved independently.

### User Stories

- As a **platform operator**, I want to configure provider credentials
  as references to secrets in the runtime environment, so that literal
  credential values never appear in gateway configuration files,
  version control, or operator tooling.

- As a **platform operator**, I want cloud-generated tokens (Vertex AI
  OAuth2, Bedrock STS) to be cached and proactively refreshed, so that
  token endpoint latency and availability do not appear on the critical
  path of inference requests.

- As a **security engineer**, I want consumer-supplied provider
  credentials stripped unconditionally before any authorized credential
  is injected, so that a consumer who possesses or guesses a provider key
  cannot bypass gateway policy by embedding it in a request.

- As a **security engineer**, I want the credential stage to fail closed
  on every unresolvable credential path — missing reference, unreadable
  secret, failed refresh — so that the gateway never forwards a provider
  request with a missing or stale credential under any failure mode.

- As a **security engineer**, I want credential values to be structurally
  absent from all logs, traces, metrics, configuration dumps, and error
  responses, so that observability pipelines and diagnostic tooling cannot
  become exfiltration vectors for secret material.

- As a **platform engineer**, I want each provider's credential mechanism
  to be implemented as an isolated resolver behind a common interface,
  so that adding a new provider requires only a new resolver and does not
  require changes to the credential stage's core logic, caching, or
  injection plumbing.

- As an **AI application developer**, I want provider authentication to
  be fully transparent — my requests are accepted or rejected based on
  my authorization, not on whether I supplied the right provider key —
  so that I never need to manage or embed provider credentials in my
  application.
