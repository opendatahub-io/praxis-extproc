---
issue: https://github.com/opendatahub-io/praxis-extproc/issues/5
discussion: https://github.com/opendatahub-io/praxis-extproc/issues/5
status: proposed
authors:
  - yehuditkerido
---

# MaaS BBR Routing and Trusted-Header Boundary

## What?

Implement the routing logic that turns an authorized MaaS
model request into concrete Envoy routing decisions: route,
authority/host, path, effective model, and header mutations. Establish a trust boundary that prevents
consumer-supplied headers from influencing routing or
bypassing authorization.

### Goals

- Extract the requested model from inference request bodies
  (JSON `model` field).
- Resolve the model to an authorized provider from a trusted
  state snapshot.
- Return routing mutations that Envoy needs: route,
  authority, path, effective model, headers, and `clear_route_cache`.
- Capture or remove consumer-supplied internal MaaS headers
  and provider authentication headers before applying
  trusted replacements.
- Define deterministic failure behavior for missing models,
  unavailable providers, conflicting mutations, and deleted state.
- Preserve the authorization result across processing stages
  without allowing consumer override.

## Why?

### Motivation

MaaS allows users to request AI models through a unified
API. The platform must route each request to the correct
provider (OpenAI, Anthropic, internal KServe, etc.) based
on the requested model and the user's entitlements.

Today, `praxis-extproc` can run filter pipelines and return
header mutations, but it lacks:

1. **Trust boundary enforcement**: A consumer can send
   internal headers (`X-MaaS-Provider`, `X-MaaS-Route`) and
   potentially influence routing decisions.

2. **Model-to-provider resolution**: No mechanism to look up
   which provider serves a given model or whether the caller
   is authorized to use it.

3. **Route cache invalidation**: When routing inputs change
   (authority, path), Envoy must recalculate the route. The
   current code does not set `clear_route_cache`.

4. **Credential isolation**: Consumer credentials (OpenShift
   tokens, MaaS API keys) must not reach model backends.
   Provider credentials must come from trusted secrets, not
   consumer-supplied headers.

Without these, MaaS cannot safely route inference requests.

### User Stories

- As a **platform operator**, I want consumers to be unable
  to bypass model authorization by forging internal headers,
  so that access control is enforced.

- As a **consumer**, I want my request to route to the
  correct provider based on the model I specify, without
  needing to know provider details.

- As a **security auditor**, I want consumer credentials
  stripped before reaching backends, so that tokens cannot
  be exfiltrated by compromised models.

- As a **platform operator**, I want routing failures
  (missing model, unavailable provider) to return stable
  error responses rather than falling through to undefined
  behavior.

## How?

### Requirements

- Use the existing `model_to_header` filter from `praxis-ai-filters`
  to extract the model name from JSON request bodies.
- Store model-to-provider mappings in an in-memory routing table
  loaded from configuration.
- Perform case-insensitive model lookups to handle variations like
  "GPT-4" vs "gpt-4".
- Normalize path construction to prevent double/missing slashes when
  joining path prefixes.
- Strip headers matching configurable prefixes (e.g., `x-maas-`,
  `x-provider-`) and optionally the `Authorization` header.
- Set `clear_route_cache: true` when authority or path mutations
  would change Envoy's route selection.
- Return deterministic errors for missing models and unknown providers.

### Design

#### Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                      server.rs                              │
│  ┌─────────────────────────────────────────────────────┐   │
│  │              run_request_filters()                   │   │
│  │  1. Run filter pipeline (includes model_to_header)   │   │
│  │  2. Extract model from X-AI-Model header             │   │
│  │  3. Call BbrProcessor.process_request()              │   │
│  │  4. Merge BBR mutations with filter mutations        │   │
│  └─────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                     src/maas/                               │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────────┐ │
│  │   bbr.rs    │  │ routing.rs  │  │ trust_boundary.rs   │ │
│  │             │  │             │  │                     │ │
│  │ BbrProcessor│─▶│RoutingState │  │ TrustBoundary       │ │
│  │ BbrResult   │  │ProviderCfg  │  │ TrustBoundaryConfig │ │
│  │             │  │ ModelEntry  │  │                     │ │
│  └─────────────┘  └─────────────┘  └─────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

#### Components

| Component | Responsibility |
|-----------|----------------|
| `BbrProcessor` | Orchestrates model resolution, trust boundary, and mutation generation |
| `RoutingState` | In-memory model-to-provider lookup table |
| `ProviderConfig` | Provider details: authority, path_prefix, cluster, effective_model |
| `TrustBoundary` | Identifies headers to strip based on configurable prefixes |
| `BbrResult` | Output: headers_to_set, headers_to_remove, clear_route_cache |

#### Error Handling

| Condition | Behavior |
|-----------|----------|
| Model not in X-AI-Model header | Skip BBR processing (filter not configured) |
| Empty model string in header | Return `RoutingError::ModelMissing` |
| Model not found in routing table | Return `RoutingError::ModelMissing` |
| Provider resolution fails | Return `RoutingError::ProviderNotFound` |
| Conflicting mutations (filter vs BBR) | BBR mutations take precedence; BBR runs after filters |

#### Effective Model

The issue requires returning an "effective model" in mutations. This handles
cases where the provider uses a different model name internally:

- User requests `gpt-4` → provider expects `gpt-4-turbo-preview`
- The `effective_model` field in `ProviderConfig` specifies the mapping
- If set, BBR adds an `X-Effective-Model` header with the mapped name
- The original model name in the request body is NOT modified (body mutation
  is out of scope; only header mutations are returned)

