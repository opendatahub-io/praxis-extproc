# Native Grid and llm-d Kind qualification

This directory contains the reusable routing qualification checks and the Kind-specific deployment helpers for the native Grid → Praxis ExtProc → EPP → llm-d-sim path. The tested setup uses real KServe `LLMInferenceService` resources and two independent EPP/simulator pools.

Start with [KIND.md](KIND.md) for prerequisites, source and image locking, the deployment flow, required proofs, and cleanup. Generated Grid configuration and overlays remain owned by the Grid operator; they are runtime resources, not repository changes.

## Qualification contract

The shared traffic runner and hooks verify the live routing contract without choosing a pool or editing Grid scores:

- Authenticated EPP samples match candidate score breakdowns, ranks, and the Distributed overlay consumed by the Gateway.
- A baseline request matches the current preferred candidate and an independent simulator completion receipt. ExtProc request-body and response oracles, trusted attribution, and bounded upstream attempts must pass.
- Simulator-reported pressure changes the EPP metric, Grid score/rank, distributed overlay revision, and Gateway-consumed revision before the next request reaches the other pool.
- Restoring the signal proves the same chain returns to the original candidate and pool.
- Making the task ExtProc unavailable causes the expected client error, records the ExtProc failure, and produces zero backend requests and simulator receipts. Restoring ExtProc is followed by another attributed request and independent receipt.
- Natural cleanup removes serving resources, observes GridNetwork withdrawal and finalizer completion while the operator is Ready, then removes the GridSite/operator and the run-owned Kind environment.

Readiness, rendered manifests, an HTTP success, or a response header alone do not prove a routing phase. Qualification requires request-correlated evidence across the client, Gateway, provider, and simulator.

Enrollment and ExternalModel/ExternalProvider translation are excluded. The qualification-only `k8s-e2e` ExtProc feature is used for the body digest and response marker; its image is not a release image.
