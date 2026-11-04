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
          - X-MaaS-Internal                 # strip inbound MaaS-owned headers

      # 2. body-field-to-header (model-extractor)
      #    ->  json_body request_extract (full parity;
      #    see the promotion-header and spoofing notes below)
      - filter: json_body
        conditions:
          - unless:                         # skip extraction when a prior hop
              headers_present:              #   already set the routing header
                - X-Gateway-Model-Name
        request_extract:
          - pointer: /model                 # IPP fieldName: model
            header: X-Gateway-Model-Name    # IPP headerName
        on_invalid: continue                # absent/invalid body -> pass through
        max_body_bytes: 1048576             # core default is 10 MiB; size per workload

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

| Go IPP plugin                                | Praxis filter                         | Parity                 | Gap tracked by |
|----------------------------------------------|---------------------------------------|------------------------|----------------|
| `body-field-to-header`                       | `json_body` (`request_extract`)       | Full*                  | —              |
| `maas-headers-guard`                         | `headers` + `when`/`unless`           | Partial                | [#5]           |
| `model-provider-resolver`                    | `intelligent_route`                   | Partial (K8s)          | [#5], [#7]     |
| `stream-usage-enforcer`                      | *(none yet)*                          | **None**               | [#44]          |
| `api-translation`                            | provider-native chain                 | Partial (reverse)      | [#6]           |
| `apikey-injection`                           | `credential_inject`                   | Partial (SigV4/OAuth2) | [#6]           |
| `nemo-request-guard` / `nemo-response-guard` | `ai_guardrails`                       | Partial (timeout unit) | [#111]         |

[#5]: https://github.com/opendatahub-io/praxis-extproc/issues/5
[#6]: https://github.com/opendatahub-io/praxis-extproc/issues/6
[#7]: https://github.com/opendatahub-io/praxis-extproc/issues/7
[#111]: https://github.com/opendatahub-io/praxis-extproc/issues/111
[crd-weight]: https://github.com/opendatahub-io/ai-gateway-payload-processing/blob/02b214fc48e14ed12981d5e0c8bee76492e3800a/api/inference/v1alpha1/externalmodel_types.go#L101-L109

### Per-plugin notes

- **`body-field-to-header` → `json_body`.** The core
  `json_body` filter promotes any body field with a
  `request_extract` entry: `pointer` (→ `fieldName`,
  as an RFC 6901 JSON Pointer such as `/model`) and
  `header` (→ `headerName`). Set `on_invalid: continue`
  to match IPP's pass-through on an absent or invalid
  body. A single `json_body` filter can list multiple
  `request_extract` entries and rewrite the body in
  the same pass, so it supersedes the older
  `model_to_header` and `json_body_field` filters.

  **\*Promotion-header safety differs.** `model_to_header`
  rejected unsafe target headers at config time —
  transport (`host`, `content-length`), credential
  (`authorization`, `x-api-key`, `cookie`), and
  internal `x-praxis-*` names. `json_body` validates
  only that the `header` name is syntactically valid;
  it does **not** reject sensitive targets (that
  guardrail lives in the `praxis-ai` promotion layer,
  which core `json_body` cannot depend on). Because the
  controller renders these configs, it must not emit a
  `header` that targets a transport, credential, or
  `x-praxis-*` name.

  **\*Spoofing and the trust boundary.** The
  `unless: headers_present` condition above skips extraction
  when `X-Gateway-Model-Name` is already set, trusting a
  value a prior hop in the chain placed there. Core
  `json_body` otherwise only *writes* the header when the
  pointer resolves: when `/model` is absent it skips
  extraction, and with `on_invalid: continue` an unparseable
  body passes through untouched — so it never *adds* the
  header for an absent or invalid body, matching the old
  `model_to_header` filter. Unlike `model_to_header`,
  however, core `json_body` does **not** strip an inbound
  client copy on its own.

  If the request reaching this chain is not trusted, prevent
  spoofing by dropping the condition and stripping the
  header first with a `headers` filter
  (`request_remove: [X-Gateway-Model-Name]`) immediately
  before `json_body`:

  ```yaml
  - filter: headers
    request_remove:
      - X-Gateway-Model-Name
  - filter: json_body
    request_extract:
      - pointer: /model
        header: X-Gateway-Model-Name
    on_invalid: continue
  ```

  Whichever you choose is a trust-boundary decision, not an
  absolute guarantee: within a chain any filter can modify
  both the header and the body, so place the strip where the
  boundary actually is.
- **`maas-headers-guard` → `headers`.** IPP captures
  inbound `x-maas-*` headers into request state for
  downstream plugins and guards them from
  passthrough. The `headers` filter with `when`/
  `unless` conditions covers header add/set/remove,
  but the MaaS-specific capture-into-state semantics
  are not yet modeled — tracked in [#5].

  A port of this `maas-headers-guard` is proposed in #160.
  The `ipp` chain's leading `headers` filter removes the
  caller's own credentials (`authorization`, `x-api-key`),
  the internal remote-MaaS loop-detection header
  (`x-origin-cluster`), and every Authorino-injected MaaS
  identity/routing header: `x-maas-*`. This list is to be kept in
  lockstep with `ai-gateway-controller/pkg/tenant/extproc_headers.go`'s
  `GuardedRequestHeaders` (the dynamic ExternalModel chain's
  copy of the same filter) whenever a new header is added on
  either side.

  This `ipp` chain also carries maas-api's own (non-inference)
  traffic, which needs its own `authorization` and identity
  headers. The filter's `unless` conditions exempt every rule
  of the `maas-api-route` HTTPRoute
  (`models-as-a-service/deployment/base/maas-api/networking/httproute.yaml`):
  `/maas-api` (a `path_prefix`, covering both the rewritten
  `/maas-api/v1` and `/maas-api/health` rules), `/v1/models`,
  `/v1/subscriptions`, and `/v1/api-keys` (also `path_prefix`),
  plus the exact-match `/v1/me` route (`path`). This is
  defense-in-depth independent of the per-route ext_proc
  disable in `deploy/overlays/odh/envoy-filter.yaml`, which has
  its own route-name-matching caveats.

- **`model-provider-resolver` → `intelligent_route`.**
  The IPP plugin runs a live Kubernetes controller
  watch over inference CRDs. `intelligent_route`
  instead consumes a controller-rendered routing
  overlay (`routing-overlay.json`) with hot reload.
  The K8s-native routing behavior is tracked in [#5]
  and [#7].

  **Weight scale and disable semantics differ — the
  controller must translate.** The `ExternalModel`
  CRD's `externalProviderRefs[].weight` is an integer
  in `0–100`, defaulting to `1`, where `0` means
  *disabled* — folding magnitude and enablement into
  one field ([`externalmodel_types.go`][crd-weight]).
  The overlay splits these into two orthogonal fields
  per candidate:
  - `traffic_weight` — an integer in `1–1000`
    (only consulted when the selection group's picker
    mode is `weighted_random`). `0` is **rejected**,
    so a disabled provider must not be emitted as
    `traffic_weight: 0`.
  - `admission_state` — enablement, independent of
    weight. Use `none` to exclude a candidate.

  So when rendering the overlay, map a Go weight `w`:
  - `w == 0` → **omit the provider from the
    candidates list** (disabled).
  - `w in 1–100` → rescale into `1–1000` (for example
    `traffic_weight = w * 10`) and leave
    `admission_state` at its enabled value.
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
- **Convert the NeMo guardrail timeout to
  milliseconds.** The IPP plugin's `timeoutSeconds` is
  in seconds and defaults to `360`
  ([`nemo_guard_base.go:43`][ipp-nemo]); the Praxis
  `ai_guardrails` filter takes `timeout_ms` in
  milliseconds and defaults to `10000` (10 s). Render
  `timeout_ms` as the IPP `timeoutSeconds` value ×
  1000 (for example, 360 s -> `timeout_ms: 360000`),
  first resolving a zero or negative `timeoutSeconds`
  to `360` — IPP treats nonpositive values as its
  360 s default ([`nemo_guard_base.go`][ipp-nemo]), and
  multiplying them directly yields `timeout_ms: 0`
  (which Praxis rejects) or a negative value (which
  does not deserialize as its unsigned timeout). Do
  not copy the seconds value verbatim — `timeout_ms:
  360` is 360 ms and will fail most rail checks. `ai_guardrails` has no `failure_mode` of its
  own, and a timeout is handled differently per phase:
  a request-phase failure propagates to the enclosing
  Envoy HTTP `ext_proc` filter, where
  `failure_mode_allow` decides fail-open vs
  fail-closed; a response-phase failure replaces the
  response body with a `guardrail_error` payload
  (headers are already sent, so it cannot reject).
  Tracked in [#111].

[ipp-nemo]: https://github.com/opendatahub-io/ai-gateway-payload-processing/blob/7b58c88cb1187d08e43d5ec82e62130a667df1db/pkg/plugins/nemo/nemo_guard_base.go#L43

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
