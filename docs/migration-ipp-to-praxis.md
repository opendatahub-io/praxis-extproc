# Migrating from Go IPP to Praxis ExtProc

This guide helps operators move from the Go
Inference Payload Processor (IPP) to the Praxis
ExtProc payload-processing path.

It follows the spike in [issue #16], which concluded
a **clean break with no compatibility adapter**: the
two configuration models are structurally
incompatible, and an adapter would carry permanent
legacy debt while adding no operator value. Operators
interact with the `ExternalModel` / `ExternalProvider`
CRDs, not with plugin wiring, so the change-over is a
controller-side re-render, not a resource migration.

[issue #16]: https://github.com/opendatahub-io/praxis-extproc/issues/16

## TL;DR

- **Config model changes shape.** Go IPP declares a
  flat list of `plugins` (declaration) plus `profiles`
  that reference them by name (wiring). Praxis uses
  inline `filter_chains` where each filter is declared
  and wired in one place.
- **Request/response phases collapse.** IPP lists a
  plugin separately under `request:` and `response:`.
  A Praxis filter implements both phases in a single
  instance placed once in the chain.
- **CRDs are unchanged.** `ExternalModel` and
  `ExternalProvider` are consumed by the controller,
  which renders Praxis config. No CRD resource
  migration is required.
- **One capability has no Praxis coverage yet:**
  `stream-usage-enforcer` (tracked in [#44]).

[#44]: https://github.com/opendatahub-io/praxis-extproc/issues/44

## 1. Before / After Configuration

The "before" is the standard MaaS production profile
shipped in
[`ai-gateway-payload-processing`][ipp-values]
(`deploy/payload-processing/values.yaml`,
`upstreamIpp.payloadProcessor.customConfig`). The
"after" is the equivalent Praxis ExtProc config
(`filter_chains`), as consumed by this server.

[ipp-values]: https://github.com/opendatahub-io/ai-gateway-payload-processing/blob/main/deploy/payload-processing/values.yaml

### Before — Go IPP `customConfig`

```yaml
customConfig:
  plugins:
  - type: maas-headers-guard
  - type: body-field-to-header
    name: model-extractor
    parameters:
      fieldName: model
      headerName: X-Gateway-Model-Name
  - type: model-provider-resolver
  - type: stream-usage-enforcer
  - type: api-translation
  - type: apikey-injection
  profiles:
  - name: default
    plugins:
      request:
      - pluginRef: maas-headers-guard
      - pluginRef: model-extractor
      - pluginRef: model-provider-resolver
      - pluginRef: stream-usage-enforcer
      - pluginRef: api-translation
      - pluginRef: apikey-injection
      response:
      - pluginRef: api-translation
```

### After — Praxis ExtProc `filter_chains`

The six request-phase plugins become an ordered
filter chain. `api-translation` appears once because
a single Praxis translation filter handles both the
request and response phases that IPP declares
separately.

```yaml
filter_chains:
  - name: maas-default
    filters:
      # 1. maas-headers-guard  ->  headers + when/unless
      #    Partial: full x-maas-* capture/guard
      #    semantics tracked in #5.
      - filter: headers
        request_remove:
          - X-MaaS-Internal   # strip inbound MaaS-owned headers

      # 2. body-field-to-header (model-extractor)
      #    ->  model_to_header  (full parity)
      - filter: model_to_header
        header: X-Gateway-Model-Name

      # 3. model-provider-resolver  ->  intelligent_route
      #    K8s routing gap tracked in #5, #7. Candidates
      #    are supplied by a controller-rendered overlay
      #    rather than a live CRD watch.
      - filter: intelligent_route
        overlay_file: /etc/praxis/routing/routing-overlay.json

      # 4. stream-usage-enforcer  ->  NOT YET AVAILABLE
      #    No Praxis equivalent exists. Tracked upstream
      #    in #44. Leave commented until the filter lands
      #    and the praxis-ai rev is bumped.
      # - filter: stream_usage_enforcer
      #     providers: [openai, vllm]

      # 5. api-translation  ->  provider-native chain
      #    One filter handles request + response
      #    translation. Reverse direction tracked in #6.
      #    Choose the filter matching the client/backend
      #    wire pair (example: Anthropic in, Chat
      #    Completions backend).
      - filter: anthropic_to_openai

      # 6. apikey-injection  ->  credential_inject
      #    SigV4 / OAuth2 strategies tracked in #6;
      #    bearer_token is supported today.
      - filter: credential_inject
        credentials:
          - name: my-api-secret
            namespace: maas-system
            key: token
            strategy: bearer_token
            file: /run/secrets/maas/my-api-secret/token
```

> The `headers`, `intelligent_route`, and
> `credential_inject` values above are illustrative.
> Fill in the real header names, overlay path, and
> secret references your controller renders. See each
> filter's reference for the full option set.

### Plugin mapping

| Go IPP plugin | Praxis filter | Parity | Gap tracked by |
|---|---|---|---|
| `body-field-to-header` | `model_to_header` | Full | — |
| `maas-headers-guard` | `headers` + `when`/`unless` | Partial | [#5] |
| `model-provider-resolver` | `intelligent_route` | Partial (K8s) | [#5], [#7] |
| `stream-usage-enforcer` | *(none yet)* | **None** | [#44] |
| `api-translation` | provider-native chain | Partial (reverse) | [#6] |
| `apikey-injection` | `credential_inject` | Partial (SigV4/OAuth2) | [#6] |

[#5]: https://github.com/opendatahub-io/praxis-extproc/issues/5
[#6]: https://github.com/opendatahub-io/praxis-extproc/issues/6
[#7]: https://github.com/opendatahub-io/praxis-extproc/issues/7

### Per-plugin notes

- **`body-field-to-header` → `model_to_header`.**
  Full parity. IPP's `fieldName: model` is fixed in
  the Praxis filter (it always promotes the `model`
  field); `headerName` maps to the `header` option.
- **`maas-headers-guard` → `headers`.** IPP captures
  inbound `x-maas-*` headers into request state for
  downstream plugins and guards them from
  passthrough. The `headers` filter with `when`/
  `unless` conditions covers header add/set/remove,
  but the MaaS-specific capture-into-state semantics
  are not yet modeled — tracked in [#5].
- **`model-provider-resolver` → `intelligent_route`.**
  The IPP plugin runs a live Kubernetes controller
  watch over inference CRDs. `intelligent_route`
  instead consumes a controller-rendered routing
  overlay (`routing-overlay.json`) with hot reload.
  The K8s-native routing behavior is tracked in [#5]
  and [#7].
- **`stream-usage-enforcer` → none.** No Praxis
  filter injects `stream_options: {include_usage:
  true}` today. This is the only capability with no
  coverage anywhere; implementation is tracked in
  [#44]. Until it lands, streaming token counting for
  OpenAI-compatible backends has no usage data.
- **`api-translation` → provider-native chain.** IPP
  ships one bidirectional translator with per-provider
  backends (Anthropic, Azure, Bedrock, OpenAI,
  Vertex). Praxis models translation as provider-
  native filters (for example `anthropic_to_openai`,
  `responses_to_chat_completions`) that handle both
  directions in a single filter. Full reverse-
  direction coverage is tracked in [#6].
- **`apikey-injection` → `credential_inject`.** IPP
  watches Kubernetes Secrets and injects provider API
  keys. `credential_inject` replaces caller
  credentials with the upstream credential selected by
  the preceding routing filter, sourcing the token
  from a mounted Secret file (`bearer_token`). SigV4
  and OAuth2 strategies are tracked in [#6].

## 2. Controller Change List

What the MaaS controller must render differently when
targeting Praxis ExtProc instead of Go IPP:

- **Emit inline `filter_chains`, not plugins +
  profiles.** Drop the two-part declaration/wiring
  model. Each filter is declared and configured at its
  position in the chain. Chains are concatenated in
  declaration order to form one pipeline.
- **Collapse request/response phases.** Do not render
  a plugin twice for `request:` and `response:`. Place
  each filter once; the filter implements both phases.
  `api-translation`'s dual listing becomes a single
  `anthropic_to_openai` (or equivalent) entry.
- **Render credentials as mounted files, not filter
  config.** `credential_inject` reads the token from a
  Secret volume path (`file:`), keeping token bytes
  out of the Praxis `ConfigMap`. The controller
  mounts the Secret and renders the `name` /
  `namespace` / `key` locator that matches the routing
  filter's credential metadata.
- **Render routing as an overlay, not a live watch.**
  Instead of relying on an in-process CRD watch,
  render `intelligent_route` candidates into a
  `routing-overlay.json` envelope projected via a
  `ConfigMap`. Mount it **without** `subPath` so hot
  reload detects updates. Every cluster referenced by
  any overlay revision must already exist in the
  downstream `load_balancer` filter.
- **Order pre/post-auth filters explicitly.** The
  chain order is the execution order. Ensure routing
  precedes `credential_inject` (which depends on the
  routing filter's credential metadata) and that any
  translation filter precedes credential injection.
  Pre/post-auth profile ordering is tracked in [#4].
- **Omit `stream_usage_enforcer` until available.**
  Do not render this filter yet; it does not exist in
  the registry and pipeline construction fails on
  unknown filter names. Track via [#44].

[#4]: https://github.com/opendatahub-io/praxis-extproc/issues/4
[#16]: https://github.com/opendatahub-io/praxis-extproc/issues/16

## 3. CRD Compatibility Statement

> **Status: draft — requires review by a controller
> maintainer** (per the acceptance criteria of
> [#43]). The statement below reflects the spike's
> conclusion and must be confirmed against the
> controller's actual reconcile behavior before it is
> treated as authoritative.

The `ExternalModel` and `ExternalProvider` CRDs are
**unchanged** by this migration. Praxis ExtProc does
not watch these CRDs directly; it consumes
controller-rendered configuration (`filter_chains`
and the routing overlay). Operators do not need to
migrate, re-apply, or version-bump CRD resources —
the change is confined to what the controller renders
into the Praxis `ConfigMap` and mounted volumes.

[#43]: https://github.com/opendatahub-io/praxis-extproc/issues/43

## Gap Tracking

Capabilities not yet at full parity, and where each
is tracked. All reference [#16] as context.

| Gap | Tracked by |
|---|---|
| `stream-usage-enforcer` (no coverage) | [#44] |
| `model-provider-resolver` (K8s routing) | [#5], [#7] |
| `api-translation` (reverse direction) | [#6] |
| `apikey-injection` (SigV4 / OAuth2) | [#6] |
| `maas-headers-guard` (MaaS semantics) | [#5] |
| Pre/post-auth profile ordering | [#4] |

## See Also

- [Configuration reference](configuration.md) — full
  `filter_chains`, server, and TLS options.
- [Praxis AI filter reference] — AI filter names and
  configuration.
- [Praxis core filter reference] — built-in filters
  (`headers`, conditions, branch chains).

[Praxis AI filter reference]: https://github.com/praxis-proxy/ai/blob/main/docs/filters/README.md
[Praxis core filter reference]: https://github.com/praxis-proxy/praxis/blob/main/docs/filters/README.md
