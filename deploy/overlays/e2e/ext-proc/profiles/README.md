# e2e ext-proc profiles

The k8s e2e suite runs in one of two **profiles**, selected with
`make e2e-setup PROFILE=<profile>` (default `conformance`). A profile swaps the
two ext-proc filter chains; it is orthogonal to `MODE` (`fds` / `buffered`),
which only changes Envoy's body processing mode. The CI matrix crosses the two
(`profile × mode`).

## Profiles

| Profile | Chain | Purpose |
|---|---|---|
| `conformance` (default) | the forge baseline — `ai_guardrails` + routing (`deploy/overlays/e2e/ext-proc`) | Exercise the production-faithful pipeline (guardrail verdicts, routing, streaming **rejection**). |
| `integrity` | `identity_header_guard` + `e2e_header_oracle` + `e2e_body_oracle`, **no** `ai_guardrails` (`profiles/integrity/`) | Instrumented, non-production lane that probes proxy mechanics: body-digest integrity, header trust boundary (anti-spoofing / mutation), and streaming. |

forge applies the `conformance` baseline. `make e2e-setup` then reconciles the
selected profile's chains and rollout-restarts the two ext-proc Deployments:
`integrity` applies the `profiles/integrity` overlay, `conformance` re-applies
the baseline workload. The restart runs for **both** profiles (a ConfigMap
change alone does not restart the pods), so switching `PROFILE` on a shared
cluster converges to the right config in either direction without a teardown.

## Why two profiles (the invariant)

Response guardrails and client streaming (`stream: true`) are **mutually
exclusive** (praxis-proxy/ai#1604): a response guardrail must see the whole body
to render a verdict, which an SSE stream cannot provide, so the proxy rejects
streaming while a response guardrail is active. A single chain therefore cannot
both assert "streaming is rejected" (conformance) and "streaming works"
(integrity). The oracles and `identity_header_guard` also need a guardrail-free
chain to be observable, hence the dedicated `integrity` profile.

Instrumentation (the `e2e_*_oracle` filters) is test-only and lives in the
integrity profile; it must never enter a production (odh) chain. The conformance
chain is production-faithful and carries no oracle, so its pre-processing hop
keeps `response_header_mode: SKIP` (as in `odh`). Only the integrity profile's
hop-1 oracles publish response headers, so `make e2e-setup PROFILE=integrity`
patches that hop's `response_header_mode` to `SEND` — the one place the oracle is
coupled to the Envoy processing mode.

## Test gating

Every test lives in exactly one profile module — `conformance::` or `integrity::`
under `tests/k8s_e2e/`. The runner (`hack/scripts/e2e-test.sh`) selects a lane
with a name filter: `cargo test --test k8s_e2e conformance` or `… integrity`. A
test's module is therefore its profile gate — drop a test in `conformance/` or
`integrity/` and it runs only in that lane, with nothing to track. `conformance`
holds the production-behavior tests (guardrails, routing, completions, errors,
…); `integrity` holds the instrumentation/mechanics tests (body and header
oracles, header protection, streaming). The other lane's tests report as
**filtered out** in the summary.
