---
issue: https://github.com/opendatahub-io/praxis-extproc/issues/27
discussion: https://github.com/opendatahub-io/praxis-extproc/issues/27
status: proposed
authors:
  - henschwartz
graduation_criteria:
  - Implementation PRs linked from How? land under
    [#27](https://github.com/opendatahub-io/praxis-extproc/issues/27)
  - How? section added after the What? and Why? direction is accepted
  - Open questions closed in Decisions before How? (long-running test tier
    mechanism, chained ext-proc scope for 3.6)
  - Idle-timeout success bar recorded in Decisions (gate before How?; see
    Decisions below)
  - A separate long-running e2e tier exists, is excluded from default CI,
    has a documented manual or scheduled trigger, a hard maximum runtime,
    retained machine-readable results, and an assigned failure owner
  - >-
    Release qualification requires executing the long-running tier (manual
    run or release pipeline) before tagging; failures block release unless
    explicitly waived with a recorded reason and owner approval
  - >-
    Authenticated TLS with certificate validation is exercised — valid
    handshakes succeed; Envoy rejects untrusted, expired, and wrong-SAN
    server certificates; Praxis rejects untrusted, expired, and missing
    client certificates when mTLS is enabled; client SAN/hostname validation
    is required (CA-chain verification alone is insufficient); each run
    records the effective validation mode; TLS-only runs do not count toward
    production qualification unless explicitly documented as an exception
  - >-
    On the default MaaS two-hop ext-proc path (pre-IPP → post-IPP), with both
    hops in FULL_DUPLEX_STREAMED request mode, scenarios assert first-chunk
    delivery before upstream EOS, multiple client chunks, final completion, and
    per-hop request-body integrity via independent header oracles across empty,
    single-chunk, multi-chunk, exactly-10-MiB, and above-cap inputs; above-cap
    rejection applies to FULL_DUPLEX_STREAMED accumulation only (10 MiB
    `MAX_BODY_ACCUMULATION` cap in `src/server.rs`); qualification fails when
    any FULL_DUPLEX_STREAMED scenario did not execute on that two-hop profile
  - Stale-idle scenarios establish a connection before the idle window,
    disable retries for the assertion, record connection reuse, send exactly
    one post-idle request per cycle, and sweep idle / pool / keepalive
    variants; qualification pass/fail follows the idle-timeout success bar
    in Decisions (reproduce on reused connection or documented clean matrix;
    root-cause proof not required for v1)
  - >-
    Production-shaped qualification targets FULL_DUPLEX_STREAMED only
    (BUFFERED / STREAMED mode sweeps are out of this tier's initial scope)
  - >-
    Chained ext-proc body-integrity scenarios publish `chain=executed` on the
    default `maas_two_hop_fd_streamed` profile; `chain=deferred` is allowed only
    for explicitly documented non-MaaS profiles, not the default pass bar
  - >-
    Each qualification run publishes machine-readable metadata — Envoy,
    Istio, and Praxis versions or image digests; effective TLS and
    certificate-validation configuration; ext_proc_modes; idle thresholds; retry
    policy; and connection-pool settings
stakeholders:
  - crstrn13
  - shaneutt
  - alexsnaps
---

# Extended E2E: TLS Idle Timeout, FULL_DUPLEX_STREAMED, and Chained Ext-Proc

## What?

The Forge-based Kubernetes e2e suite
([#21](https://github.com/opendatahub-io/praxis-extproc/issues/21))
covers baseline ext-proc behavior in CI: plaintext, single ext-proc hop,
and BUFFERED / STREAMED body modes. That is the right default for
fast feedback, but it does not exercise two risks surfaced by the
[praxis-proxy/ai#459](https://github.com/praxis-proxy/ai/issues/459)
spike and tracked in
[#27](https://github.com/opendatahub-io/praxis-extproc/issues/27):

1. **Stale connection failures after long idle** — first requests after
   minutes of quiet traffic can fail while later requests self-heal;
   observed with MaaS-style configs targeting Praxis instead of the Go
   IPP. Root cause is not confirmed (TLS session reuse, upstream pool
   keepalive, intermediate LB idle timeout, and similar hypotheses remain
   open).
2. **Request-body loss with chained `FULL_DUPLEX_STREAMED` ext-proc** —
   when two or more ext-proc filters are chained in full-duplex streamed
   mode, Envoy can deliver `EndOfStream` with zero request bytes to the
   downstream processor
   ([envoyproxy/envoy#44605](https://github.com/envoyproxy/envoy/issues/44605)).
   Response streaming is unaffected. A standalone repro exists at
   [crstrn13/ext-proc-tests](https://github.com/crstrn13/ext-proc-tests);
   praxis-extproc needs qualification in its own topology.

This proposal adds a **second e2e tier**: longer-running, TLS-qualified
scenarios that run **outside regular CI**, built on the
[#21](https://github.com/opendatahub-io/praxis-extproc/issues/21)
harness and
[#3](https://github.com/opendatahub-io/praxis-extproc/issues/3) TLS/mTLS
server support. It does not replace the baseline suite; it extends
coverage for production-adjacent failure modes before MaaS / 3.6 rollout.

### Scope of scenarios (directional)

Operators and maintainers should be able to run (locally or in a
scheduled/nightly job) scenarios that cover at least:

**Stale idle / TLS**

- Reproduce or narrow the post-idle first-request failure with Praxis in
  the e2e cluster (not only the Go IPP baseline).
- Run chat completion and streaming workloads over **authenticated TLS**
  (and **mTLS** when the declared topology requires client certificates)
  between Envoy and Praxis, including negative certificate cases (see
  Goals).
- For each idle / pool / keepalive variant: **establish and record** an
  upstream connection, **disable client and Envoy retries** for the
  assertion, wait through a configurable idle window (on the order of
  5–10 minutes to start), then send **exactly one** request on the reused
  connection. Assert **connection reuse** and request success. Repeat the
  cycle across idle durations and Envoy upstream connection-pool /
  keepalive settings to characterize failure thresholds.

**`FULL_DUPLEX_STREAMED` (production path)**

- Use a **delayed multi-chunk upstream**. Assert **first-chunk delivery
  before upstream EOS**, observe multiple client chunks, and assert final
  completion through ext-proc in **FULL_DUPLEX_STREAMED** mode (not merely
  eventual delivery after buffering).
- Validate **request-body integrity** through the **two-hop** ext-proc chain
  using an **independent oracle**: the client generates known **length and
  SHA-256** values; compare them at **each processor boundary** and against
  an upstream echo or recorded backend payload. Cover **empty**, **single-
  chunk**, **multi-chunk**, **exactly 10 MiB** (per
  [architecture.md](../architecture.md) body cap), and **above-cap**
  inputs. Above-cap rejection applies to **FULL_DUPLEX_STREAMED**
  accumulation via `check_body_limit` in `src/server.rs`.

**Chained ext-proc (MaaS-style two-hop — default for this tier)**

- Declared production topology for MaaS-style qualification is **two ext-proc
  hops**: **pre-IPP ext-proc → (Kuadrant/WASM — out of ext-proc scope) →
  post-IPP ext-proc**. This tier **always** exercises that dual-hop path; it
  does not default to single-hop with `chain=deferred`.
- Maintain an explicit **topology fixture or inventory** recording hop count,
  TLS mode, and idle matrix. Each qualification run must publish
  `chain=executed` or document deferral with reason.
- Exercise request-body integrity through the chain under conditions that
  reproduce
  [envoyproxy/envoy#44605](https://github.com/envoyproxy/envoy/issues/44605),
  comparing the client oracle digest at **every hop**. Qualification
  **fails** when the chained scenario did not execute.

### Test tier

These scenarios are **long-running** (multi-minute idle waits, longevity
runs, concurrency sweeps). They must **not** gate default PR CI, but they
**must** participate in **release qualification** — executed manually or via
a release pipeline before tagging, with failures blocking release unless
explicitly waived with a recorded reason and owner approval. The exact
mechanism (Cargo feature flag, `#[ignore]`, separate crate, nightly
workflow only, etc.) is an open question for Decisions / How?; the
contract here is **separation from the
[#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) fast
path** plus an **executable, bounded operation contract**:

- A **named command or workflow** invocation (implementation left to How?)
- A **manual or scheduled trigger** policy
- A **hard maximum runtime** per qualification job
- **Retained machine-readable results** and an **assigned owner** for failures

Each run must also publish reproducibility metadata: Envoy / Istio /
Praxis versions or image digests; effective TLS and certificate-validation
configuration; mode combinations exercised; idle thresholds; retry policy;
and connection-pool settings.

### Goals

- Extend the
  [#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) e2e
  harness with a **documented long-running tier** that has a named
  invocation, trigger policy, timeout, retained artifacts, and owner
- Require the long-running tier to run as part of **release qualification**
  (manual or release pipeline) before tagging; failures block release
  unless explicitly waived with a recorded reason and owner approval
- **Reproduce or narrow** stale-connection behavior using connection-reuse
  evidence per the **idle-timeout success bar** in Decisions (not
  burst-and-retry tests that mask pool refresh)
- Exercise **authenticated TLS** with certificate validation: valid
  handshakes; Envoy rejection of untrusted, expired, and wrong-SAN server
  certificates; Praxis rejection of untrusted, expired, and missing client
  certificates when mTLS is enabled; require client SAN/hostname validation
  (CA-chain verification alone is insufficient); record effective validation
  mode per run; exclude TLS-only qualification unless documented as an
  exception
- Qualify **FULL_DUPLEX_STREAMED** on the default **MaaS two-hop** path using
  pre-EOS streaming evidence and **per-hop** request-body integrity (client
  digest vs `x-qualification-request-sha256-hop-<N>` and upstream echo); fail
  qualification when any scenario did not run on that profile
- Publish **`chain=executed`** metadata for the default profile; non-MaaS
  profiles may document `chain=deferred` with reason — not the default pass bar
- Publish **machine-readable run metadata** so passes are reproducible after
  upgrades
- Produce results maintainers can use for release qualification and for
  upstream Envoy / platform discussions (without fixing Envoy in this repo)

### Non-Goals

- Replacing or slowing the **baseline**
  [#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) CI
  scenarios (BUFFERED / STREAMED, plaintext, single ext-proc)
- Fixing
  [envoyproxy/envoy#44605](https://github.com/envoyproxy/envoy/issues/44605)
  inside praxis-extproc (upstream Envoy defect; tests may document and
  guard against regression)
- Running multi-hour longevity or 1000-stream concurrency sweeps in
  **default** CI (spike-scale runs remain manual or scheduled)
- Implementing new ext-proc protocol features beyond what
  [#3](https://github.com/opendatahub-io/praxis-extproc/issues/3) already
  covers for TLS/mTLS
- Defining MaaS 3.6 production topology
  ([#26](https://github.com/opendatahub-io/praxis-extproc/issues/26));
  this proposal qualifies ext-proc behavior under **declared** topologies
  via fixture/inventory, not production rollout decisions
- Duplicating or relocating this tier into
  [opendatahub-io/opendatahub-tests](https://github.com/opendatahub-io/opendatahub-tests);
  platform-level suites may invoke these scenarios later, but ext-proc
  harnesses, fixtures, and mode matrices belong in this repository

### Open Questions

Resolved in **Decisions** (long-running tier wiring, 3.6 chained scope).
No remaining open questions block implementation.

### Decisions

Resolved in this proposal (must be accepted before the How? PR begins).

**Idle-timeout success bar.** The stale-idle bug is not yet reproduced in
this harness and its root cause is unconfirmed. For **v1 of this tier**,
**pass** means one of:

1. **Reproduce** the post-idle first-request failure on a **reused**
   connection (retries disabled, connection-reuse evidence recorded), with
   the idle / pool / keepalive / TLS parameter set published; or
2. **Document clean pass** across the configured idle / pool / keepalive
   matrix (failure not observed under those conditions, with the same
   evidence and metadata).

**Pass does not require** eliminating the failure in production configs or
**proving** a single root cause (TLS session reuse vs Envoy pool vs
intermediate LB) for v1 graduation. Narrowing via the parameterized matrix
and published run metadata is sufficient; root-cause attribution may follow
in platform or follow-up work.

How? must implement assertions and reporting against this bar — not an
undefined “first request succeeds” check that retries or fresh connections
can satisfy.

**Ext-proc body modes (v1).** Go IPP → Rust migration targets
**`FULL_DUPLEX_STREAMED`** only for this extended tier. **BUFFERED** and
**STREAMED** mode comparison matrices are **out of initial scope** — they
remain covered by baseline
[#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) fast tests
where applicable, not this long-running tier.

**Repository placement.** Scenario definitions, topology fixtures, and the
[#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) harness
extension stay in **praxis-extproc**. Cross-component platform qualification
in
[opendatahub-io/opendatahub-tests](https://github.com/opendatahub-io/opendatahub-tests)
may consume results later; this proposal does not move ext-proc-specific
fixtures out of this repo.

**Release qualification.** The long-running tier does **not** gate default PR
CI. It **does** gate **release qualification**: maintainers run it manually or
via a release pipeline before tagging. Failures **block** release unless
explicitly waived with a recorded reason and owner approval. How? wires the
exact workflow hook; this decision locks the release contract.

**Long-running tier wiring.** Extend the existing Forge **k8s-e2e** harness
([#21](https://github.com/opendatahub-io/praxis-extproc/issues/21)) — do **not**
introduce a parallel qualification framework. Extended scenarios live beside
baseline k8s-e2e tests under `tests/k8s_e2e/extended/`, marked **`#[ignore]`**
and selected with the **`k8s_e2e::extended`** module filter so default
`cargo test` and PR CI stay fast. The named operator entry points are
**`make e2e-setup-extended`** (baseline `make e2e-setup` plus
`deploy/overlays/e2e-extended/`) and **`make test-e2e-extended`** (wrapper
around `cargo test --features k8s-e2e -- k8s_e2e::extended --ignored`). Deploy
artifacts for this tier live under **`deploy/overlays/e2e-extended/`** (a thin
overlay on top of **`deploy/overlays/e2e/`**, which baseline #21 CI already
uses). Extend **`.github/workflows/ci-k8s-e2e.yaml`** with a
**`workflow_dispatch`** job for the extended tier; do **not** add a separate
`qualification.yaml` workflow.

**3.6 chained ext-proc scope (v1 default).** MaaS-style production topology
for this tier is **two ext-proc hops** — **pre-IPP ext-proc → post-IPP
ext-proc** (Kuadrant/WASM between them is out of ext-proc test scope). v1
qualification **always** runs the chained-body-integrity scenario on that path
with both hops in **`FULL_DUPLEX_STREAMED`** request mode. Qualification
**fails** when the chained scenario did not execute. Topology inventory
records hop count and ext-proc Deployment names; `chain=deferred` is allowed
only for explicitly documented non-MaaS profiles, not the default pass bar.

## Why?

### Motivation

The
[praxis-proxy/ai#459](https://github.com/praxis-proxy/ai/issues/459)
spike showed that praxis-extproc handles long AI response streams well in
isolation — BUFFERED, STREAMED, and FULL_DUPLEX_STREAMED modes survived
extended runs, scaled linearly to high concurrency, and respected timeouts.
That confidence does not extend to two production-shaped risks:

**Idle and TLS.** MaaS deployments keep Envoy↔processor connections warm
across long quiet periods. Intermittent “first request after idle fails,
then recovery” behavior is exactly the class of incident that is hard to
catch with short CI tests and expensive to debug in production. A burst
test with retries enabled can pass even when pooled connections are stale,
because Envoy or the client silently opens a fresh connection. TLS
introduces session reuse, certificate validation, and handshake paths that
plaintext
[#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) tests do
not cover. Without reproducible e2e scenarios that record connection reuse
and validation mode, operators cannot tell whether Praxis, Envoy pool
settings, or platform networking is at fault.

**Full-duplex streamed request bodies.** MaaS inference often streams
responses while still sending non-trivial request bodies. Full-duplex
streamed mode is attractive for latency, but a test that only checks
eventual delivery cannot distinguish true streaming from buffering until
EOS. Envoy also has a known defect when **multiple** ext-proc filters use
full-duplex streamed mode on the request path — bodies can vanish silently.
Even if 3.6 uses a single Praxis ext-proc in some deployments, teams need
evidence that **FULL_DUPLEX_STREAMED** is safe on the **MaaS two-hop path**
(pre-EOS chunks, independent body oracle at each hop). Chained tests with an
explicit topology declaration protect against topology drift and prevent
qualification gaps when platform wiring adds a second ext-proc hop.

Baseline
[#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) tests must
stay fast. This work belongs in a **separate tier** with bounded runtime,
retained artifacts, and reproducibility metadata so qualification depth does
not trade off against every PR’s CI time.

### User Stories

These are stakeholder needs derived from
[#27](https://github.com/opendatahub-io/praxis-extproc/issues/27);
they are not separate tracked issues.

- As a maintainer, I want long-running TLS and idle-timeout e2e scenarios
  that prove connection reuse so that we can reproduce or rule out
  stale-connection failures before MaaS rollout.
- As a maintainer, I want `FULL_DUPLEX_STREAMED` qualification with
  pre-EOS chunk delivery and independent request-body oracles so that
  streaming inference configs are evidence-based, not assumed from the
  spike alone.
- As a platform engineer, I want chained ext-proc regression tests with
  an explicit topology declaration (`chain=executed` / `chain=deferred`)
  so that Envoy body-loss behavior is visible before we compose multiple
  processors.
- As an operator, I want **FULL_DUPLEX_STREAMED** qualification evidence on
  the MaaS two-hop ext-proc path so that streaming inference configs are
  evidence-based without assuming full request buffering for every workload.
- As a release owner, I want these scenarios outside default CI but required
  for release qualification (manual or pipeline), with named invocation,
  timeouts, retained machine-readable results, and an owner, so that
  qualification runs are repeatable, accountable, and gate tagging when they
  fail

## How?

### Implementation

Ship as implementation work under
[#27](https://github.com/opendatahub-io/praxis-extproc/issues/27) in
tranches after this How? is accepted:

1. **Harness extension** — `make test-e2e-extended`, `deploy/overlays/e2e-extended/`
   overlay, topology inventory, metadata reporter, `workflow_dispatch` job on
   `ci-k8s-e2e.yaml`.
2. **FD_STREAMED + body oracle** — `e2e_body_oracle` helper (feature-gated),
   streaming oracle, body-size matrix on the two-hop chain.
3. **Idle / TLS** — connection-reuse evidence, cert validation matrix,
   parameterized idle / pool / keepalive sweep (same cluster as baseline e2e).
4. **Chained body integrity** — default MaaS two-hop profile (pre-IPP →
   post-IPP); header-based per-hop verification.

No production ExtProc protocol changes beyond what
[#3](https://github.com/opendatahub-io/praxis-extproc/issues/3) already
provides for TLS/mTLS.

### Requirements

- Extend the [#21](https://github.com/opendatahub-io/praxis-extproc/issues/21)
  Forge k8s-e2e harness (`make e2e-setup`, `make e2e-setup-extended`,
  `make e2e-test`, `make test-e2e-extended`, existing `deploy/overlays/e2e/`,
  `deploy/overlays/e2e-extended/`, `.github/workflows/ci-k8s-e2e.yaml`) — do
  **not** fork a second cluster bootstrap or parallel qualification stack.
- Named invocation: **`make e2e-setup-extended`** then **`make test-e2e-extended`**
  (wrapper around `cargo test --features k8s-e2e -- k8s_e2e::extended --ignored`;
  documented in `docs/development.md` and `make help`).
- Hard job timeout: **120 minutes** per extended-tier workflow run. Per-cell
  idle waits come from `idle_matrix[].idle_secs`; when a cell omits
  `idle_secs`, use **`QUALIFICATION_IDLE_SECS`** (default **300**).
- Retained artifacts: **`target/e2e-extended/report.json`** plus JUnit XML
  under **`target/e2e-extended/junit/`** uploaded from the release workflow.
- Assigned failure owner: **release qualification maintainers** (initially
  proposal authors; recorded in workflow README).
- Every run publishes **`chain=executed`** or **`chain=deferred`** with reason,
  topology profile name, and the metadata block from What? (versions, TLS mode,
  ext_proc_modes, idle/pool/keepalive/retry settings).
- Idle scenarios: establish connection, **disable client and Envoy retries**,
  record reuse evidence via Envoy access-log `%CONNECTION_ID%` /
  `%UPSTREAM_CONNECTION_ID%` (see **Idle / TLS scenarios**), **one**
  post-idle request, evaluate against the idle-timeout success bar in Decisions.
- Chained scenarios: verify each ext-proc hop via response headers
  `x-qualification-request-sha256-hop-<N>` — not pod log scraping.
- FD_STREAMED scenarios (`tests/k8s_e2e/extended/fd_streamed.rs`,
  `tests/k8s_e2e/extended/chained.rs`): run only on profile
  **`maas_two_hop_fd_streamed`**; delayed multi-chunk upstream; assert **first
  client chunk before upstream EOS**; compare client digest to **each** hop
  header `x-qualification-request-sha256-hop-<N>` and upstream echo.
- Body-size matrix on that two-hop profile: **empty**, **single-chunk**,
  **multi-chunk**, **exactly 10 MiB** (`10_485_760` bytes per
  `MAX_BODY_ACCUMULATION` in `src/server.rs`), and **above-cap** input on
  **FULL_DUPLEX_STREAMED**.
- Above-cap contract (**FULL_DUPLEX_STREAMED only**): expect **request
  rejection** (gRPC `ResourceExhausted` / `body exceeds maximum size` from
  `check_body_limit`) — not silent truncation — matching today's 10 MiB
  `MAX_BODY_ACCUMULATION` cap in `src/server.rs`.
- TLS matrix: valid handshake; Envoy rejects untrusted / expired / wrong-SAN
  server certs; Praxis rejects untrusted / expired / missing client certs when
  mTLS is enabled; **client SAN/hostname validation required**; record effective
  validation mode in `report.json`.

### Design

#### Repository layout

```text
tests/k8s_e2e/
  mod.rs                        # re-exports baseline + extended modules
  extended/                     # long-running tier (#[ignore]; k8s_e2e::extended filter)
    mod.rs                        # shared helpers: gateway URL, oracle, metadata
    idle_tls.rs                   # stale-idle + TLS/mTLS matrix
    fd_streamed.rs                # FULL_DUPLEX_STREAMED streaming (two-hop profile)
    chained.rs                    # per-hop body integrity on MaaS chain (default profile)
deploy/overlays/e2e/            # baseline #21 overlay (existing Forge harness)
deploy/overlays/e2e-extended/   # supplemental overlay for this tier (on top of e2e/)
  topology/                     # inventory YAML (profiles, hop count, idle matrix)
  body-oracle.yaml              # e2e_body_oracle filter chain (feature image)
  access-log-reuse.yaml         # Envoy access log for connection-reuse evidence
  envoyfilter-*.yaml            # per-scenario patches (FD mode, TLS, two-hop chain)
  tls/                          # generated or checked-in test CA / leaf / client certs
src/e2e/                        # compiled only with --features k8s-e2e (extended helpers)
  body_oracle.rs                # hashes request body; sets response header (see below)
hack/e2e-extended-report.sh     # merge cargo junit + env versions → report.json
.github/workflows/ci-k8s-e2e.yaml  # add workflow_dispatch extended-tier job
Makefile                        # e2e-setup-extended + test-e2e-extended targets
docs/development.md             # operator runbook beside existing e2e-setup / e2e-test
```

Baseline fast k8s-e2e tests remain in the existing integration / k8s-e2e module
without the `extended` filter.

#### Topology inventory

YAML profile under `deploy/overlays/e2e-extended/topology/` (example fields):

| Field | Purpose |
| --- | --- |
| `name` | Profile id recorded in `report.json` |
| `ext_proc_hops` | Number of ext-proc filters in the chain (default **2** for MaaS) |
| `ext_proc_modes` | Request/response `BodySendMode` pairs — **`FULL_DUPLEX_STREAMED`** only in v1 |
| `tls` | `plaintext`, `tls`, or `mtls` between Envoy and praxis-extproc |
| `idle_matrix` | List of `{ idle_secs, pool_settings_ref }` cells to sweep |

Default profile for v1: `maas_two_hop_fd_streamed` with `ext_proc_hops: 2` →
`chain=executed`.

#### Tier invocation

```bash
# Local (Forge k8s-e2e cluster + extended overlay on baseline e2e/)
make e2e-setup-extended
make test-e2e-extended

# Equivalent
make e2e-setup
kubectl apply -k deploy/overlays/e2e-extended/
cargo test --features k8s-e2e -- k8s_e2e::extended --ignored
```

Environment knobs (documented defaults):

| Variable | Default | Purpose |
| --- | --- | --- |
| `QUALIFICATION_IDLE_SECS` | `300` | Default `idle_secs` when a matrix cell omits the field |
| `E2E_EXTENDED_PROFILE` | `maas_two_hop_fd_streamed` | Topology inventory selection |
| `GATEWAY_URL` | `http://localhost:18080` | Same as [#21](https://github.com/opendatahub-io/praxis-extproc/issues/21) k8s-e2e tests |

#### Idle / TLS scenarios

1. Warm up: send a request on a **reused HTTP/2 connection** (client pool max
   idle connections = 1). **Connection reuse evidence** uses Envoy **access
   logs**, not admin `/stats` polling: apply
   `deploy/overlays/e2e-extended/access-log-reuse.yaml` on the gateway so each
   request line includes `%CONNECTION_ID%` and `%UPSTREAM_CONNECTION_ID%`.
   `tests/k8s_e2e/extended/idle_tls.rs` reads these fields from the warm-up
   access-log line and asserts the post-idle request reuses the same
   downstream `%CONNECTION_ID%` (and the same upstream
   `%UPSTREAM_CONNECTION_ID%` when the profile routes through ext-proc).
   This is deterministic per request without stats scrape timing or
   ambiguous counter deltas under idle-timeout boundary conditions.
2. Configure Envoy **upstream connection pool** and **keepalive** from the
   active profile overlay; set **route retry policy to zero** and use a client
   with retries disabled.
3. Sleep the active cell's `idle_secs` (or `QUALIFICATION_IDLE_SECS` when
   the cell omits `idle_secs`).
4. Send **exactly one** POST/chat-style request on the same connection; capture
   success/failure and reuse evidence.
5. Repeat for each idle-matrix cell; classify pass/fail per Decisions (reproduce
   on reused connection **or** documented clean matrix).

TLS overlays swap `deploy/overlays/e2e-extended/envoyfilter-tls-*.yaml` and
praxis-extproc TLS config from [#3](https://github.com/opendatahub-io/praxis-extproc/issues/3).
Negative cases mount wrong/expired certs via separate overlay files under
`deploy/overlays/e2e-extended/`; each run records `tls_validation_mode` in
metadata.

#### FULL_DUPLEX_STREAMED (MaaS two-hop — default profile)

All scenarios in `tests/k8s_e2e/extended/fd_streamed.rs` and
`tests/k8s_e2e/extended/chained.rs` run against profile
**`maas_two_hop_fd_streamed`** (`ext_proc_hops: 2`, `chain=executed`). They
do **not** qualify single-hop FULL_DUPLEX_STREAMED in this tier.

- Apply **`deploy/overlays/e2e-extended/`** on top of **`deploy/overlays/e2e/`**
  so baseline Gateway/Route remain shared with [#21](https://github.com/opendatahub-io/praxis-extproc/issues/21).
- Deploy profile with `request_body_mode` and `response_body_mode` set to
  **`FULL_DUPLEX_STREAMED`** (Envoy enum value `4`) on **both** ext-proc hops.
- Upstream: **delayed multi-chunk** echo service (extended echo Deployment in
  `deploy/overlays/e2e-extended/` or patched echo command) emitting at least two
  response chunks with a configurable delay between them.
- Client asserts: first chunk received **before** upstream signals EOS; multiple
  chunks observed; final body complete.
- Body oracle (three-way compare):
  1. **Client** generates payload with known length + SHA-256.
  2. **Processor boundary** — extended deploy enables
     `deploy/overlays/e2e-extended/body-oracle.yaml`, which activates the
     **`e2e_body_oracle`** helper (`src/e2e/body_oracle.rs`, compiled only with
     `--features k8s-e2e`). After request-body EOS the filter hashes the
     accumulated body and sets response header **`x-qualification-request-sha256`**
     (lowercase hex). In the two-hop chain, hop *N* sets
     **`x-qualification-request-sha256-hop-<N>`** and forwards prior hop headers
     unchanged. The test reads these headers from the gateway response.
  3. **Upstream echo** — echo Deployment returns the request body; the test
     hashes the response body and compares to the client digest.
  Any mismatch fails the scenario.

#### Chained ext-proc (MaaS two-hop — default)

Apply an EnvoyFilter overlay under `deploy/overlays/e2e-extended/` that
inserts **two** `ext_proc` filters in **`FULL_DUPLEX_STREAMED`** request mode
(pre-IPP and post-IPP Deployments), reproducing
[envoyproxy/envoy#44605](https://github.com/envoyproxy/envoy/issues/44605)
conditions. Enable `body-oracle.yaml` on **each** ext-proc Deployment.

**Per-hop verification (header-based).** Each hop exposes its digest on
`x-qualification-request-sha256-hop-<N>` in the gateway response. The test
compares the client digest to **every** hop header. Structured logs
(`qualification_body_digest` at DEBUG) are optional diagnostics only —
qualification **pass/fail does not depend** on pod log scraping.

Publish `chain=executed` for the default `maas_two_hop_fd_streamed` profile.
Non-MaaS profiles may document `chain=deferred` with reason — not the default
pass bar.

#### Run metadata (`report.json`)

Written at end of `make test-e2e-extended` (via `hack/e2e-extended-report.sh`):

```json
{
  "profile": "maas_two_hop_fd_streamed",
  "chain": "executed",
  "chain_reason": "maas pre-ipp and post-ipp ext-proc hops",
  "ext_proc_hops": 2,
  "envoy_version": "...",
  "istio_version": "...",
  "praxis_extproc_image_digest": "...",
  "tls_validation_mode": "mtls_client_san",
  "ext_proc_modes": { "request": "FULL_DUPLEX_STREAMED", "response": "FULL_DUPLEX_STREAMED" },
  "idle_matrix_results": [],
  "started_at": "...",
  "finished_at": "...",
  "outcome": "pass"
}
```

#### Release qualification workflow

Extend **`.github/workflows/ci-k8s-e2e.yaml`**:

- Add a **`workflow_dispatch`** job (and optional nightly cron) for the extended
  tier — **not** a separate `qualification.yaml`.
- Steps: `make e2e-setup-extended` → build/load image →
  `make test-e2e-extended` → upload `target/e2e-extended/**` artifacts.
- Job `timeout-minutes: 120`.
- Failures block release unless waived per Decisions (manual issue comment with
  owner approval).

Default PR CI **unchanged** — baseline k8s-e2e only; extended tests stay
`#[ignore]` / filtered out of the fast path.

### Key files (planned)

| Area | Files |
| --- | --- |
| Makefile + docs | `Makefile` (`e2e-setup-extended`, `test-e2e-extended`), `docs/development.md` |
| Extended k8s-e2e tests | `tests/k8s_e2e/extended/*.rs` |
| Body oracle helper | `src/e2e/body_oracle.rs`, `deploy/overlays/e2e-extended/body-oracle.yaml` |
| Connection reuse logging | `deploy/overlays/e2e-extended/access-log-reuse.yaml` |
| Deploy overlays | `deploy/overlays/e2e/` (baseline), `deploy/overlays/e2e-extended/**` |
| Reporting | `hack/e2e-extended-report.sh`, `target/e2e-extended/report.json` |
| Release hook | `.github/workflows/ci-k8s-e2e.yaml` (extended job) |
| Baseline harness (reuse) | Forge `make e2e-setup` / `make e2e-test`, existing k8s-e2e tests |

### Test plan

- Unit-level helpers for SHA-256 oracle and report JSON schema (Rust tests, no
  cluster).
- Qualification integration (ignored, `k8s_e2e::extended` filter): each scenario
  file above with `V=1` logs.
- Release workflow dry-run on fork before first tag gate.
- Document local run in `docs/development.md` beside existing `e2e-setup` /
  `e2e-test`.

### Explicitly out of this How (see Non-Goals)

- Fixing [envoyproxy/envoy#44605](https://github.com/envoyproxy/envoy/issues/44605)
  in Envoy or praxis-extproc production code.
- Moving harnesses to
  [opendatahub-io/opendatahub-tests](https://github.com/opendatahub-io/opendatahub-tests).
- Multi-hour longevity or 1000-stream concurrency in default qualification
  (manual spike scripts only).
- New ExtProc protocol features beyond [#3](https://github.com/opendatahub-io/praxis-extproc/issues/3).
