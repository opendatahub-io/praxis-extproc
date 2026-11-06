# Kind deployment and qualification

This guide describes the implemented Kind path only. A run builds immutable images from one source lock, deploys the native serving graph, qualifies signal-driven routing, and performs natural cleanup in one run-owned environment.

## Request and signal paths

```text
Client with verified TLS
  → task Gateway and HTTPRoute
  → ExtProc request/response processing
  → Praxis intelligent_route reads the current Grid overlay
  → selected mTLS provider proxy
  → pool Gateway and authorization policy
  → KServe LLMInferenceService / InferencePool
  → EPP
  → llm-d-sim
```

```text
simulator queue control
  → authenticated EPP metrics
  → Grid candidate score and rank
  → Distributed overlay revision
  → Gateway consumes that revision
  → request reaches the newly preferred pool
```

The caller identity and the provider-hop identity are separate. The task Gateway verifies client-facing TLS. Provider proxies verify the Grid CA and use projected, short-lived credentials for pool authorization. Neither token bytes nor private keys are stored in generated config or evidence.

## Prerequisites

The local runner needs Docker, Kind, kubectl, Helm, kustomize, jq, Python 3, Git, Cargo, `skopeo`, `ip`, and `istioctl` 1.28.0. It needs enough free disk, memory, and CPU for a one-node cluster and sequential builds of the pinned controller and serving images. `create-kind-environment.sh` checks capacity and refuses existing cluster, registry, network, state, or evidence names; it never adopts them.

Choose a unique run ID and private state/evidence roots. The automation generates the Kind context, kubeconfig, registry, namespaces, TLS identities, and immutable image repositories from that run ID. Do not point the helpers at an existing Kind cluster or reuse an earlier run's identities, registry, state, images, or evidence directory.

## Ownership and install boundaries

| Owner | Resources and responsibilities |
| --- | --- |
| Kind helpers | Cluster, local registry, base APIs/controllers, local TLS trust, run namespaces, and environment teardown. |
| MaaS and AIGC controllers | MaaS API/auth resources and tenant payload processors. The qualification does not install ExternalModel fixtures. |
| KServe | LLMInferenceService-derived InferencePools, EPP/simulator workloads, and pool HTTPRoutes. The serving proof uses reconciled resources, not hand-built substitutes. |
| Istio | Gateway data-plane Deployments and Services. |
| Grid operator | GridSite/GridNetwork status, provider state, generated Praxis configuration, candidate scores/ranks, and overlay revisions. |
| Qualification helpers | Task and provider proxies, task Gateway/HTTPRoute, ExtProc filter, projected client identity, simulator control requests, receipts, and evidence. |

The code boundary is deliberate:

| Code | Owns |
| --- | --- |
| `xtask/src/e2e.rs` and the qualification hooks | Read-only routing gates, metric/score/rank consistency, overlay and Gateway revision checks, trusted attribution, simulator controls/receipts, traffic assertions, and evidence. They do not create a Kind cluster or install platform dependencies. |
| `create-kind-*`, `install-kind-*`, and `configure-kind-*` | Kind cluster lifecycle, registry, dependency installation, local CA/trust setup, and Kind networking. |
| `build-kind-images.sh` and `plan_kind_images.py` | Kind-local immutable image builds and image provenance from the run's locked sources. |
| Grid, Gateway, pool, provider-proxy, and ExtProc deployment helpers | Task-owned runtime resources, using the shared serving and routing contracts. |
| `cleanup-grid-operator.sh` and Kind cleanup helpers | Natural Grid withdrawal ordering, task-resource removal, and run-owned Kind/registry/network teardown. |

Runtime Grid overlays stay under Grid ownership. A future deployment adapter can supply the resources and evidence inputs expected by the qualification runner without copying its traffic assertions. The current guide and qualification evidence cover Kind only. A few common resource helpers still contain narrowly scoped Kind branches for image references and serving transport; they are not an alternate platform installer.

## Deployment order

The sequence follows controller dependencies rather than applying every manifest in one bundle:

1. Resolve current source refs once into clean worktrees and immutable source locks. Prepare the separately locked MaaS API build input and apply the versioned Kind compatibility patch to the MaaS controller source. Build all required images into a run-owned registry and record OCI provenance/digests.
2. Create Kind and its registry. Install Gateway API, cert-manager, Istio/Gateway API support, then Kuadrant/Authorino. Verify API readiness, storage, node schedulability, controller/webhook readiness, and zero Pending Pods.
3. Install the upstream KServe controller, its CRDs, runtime templates, and certificate-backed webhooks from one source revision. Verify the actual `LLMInferenceService` API and controller image identity.
4. Install the MaaS database, tenant CA, default Gateway, controller and API from pinned sources. Install the Kind AIGC/Praxis payload components, CA trust, and the narrow Istiod xDS egress policy required by their workload. Verify the tenant, API, payload processors, and authorization controllers before serving resources.
5. Create the task namespace and bootstrap Gateway; create the pool Gateways and their authorization resources.
6. Deploy Pool A's actual `LLMInferenceService`, wait for KServe/EPP/simulator and policy readiness, and send one labeled setup probe that must match a simulator receipt. Repeat independently for Pool B and prove the simulator Pod UID sets are disjoint.
7. Install the Grid operator, GridSite, and GridNetwork. Validate the operator-created Grid CA and site identity without replacing Grid-owned Secrets. Set the site endpoint, register both providers, and require authenticated metrics, exact score/rank agreement, a Distributed overlay, and Gateway consumption of its current revision.
8. Deploy one Praxis provider proxy per provider, projected pool credentials, final Grid-generated Praxis config, the task Gateway/HTTPRoute, ExtProc filter, and client identity. Verify route/model/path, TLS, authorization, attribution, image IDs, and overlay revision before inference.
9. Run the bounded traffic sequence: baseline, pressure/reroute, recovery, failure-closed, and restored success. Every qualification request must have a unique request ID and an independent simulator receipt, except the failure-closed request, which must have none.
10. Remove provider and serving resources. Delete GridNetwork while the operator is Ready and wait for natural withdrawal/finalizer completion. Then delete GridSite and uninstall the operator. Remove task resources, restore any temporary MaaS cleanup setting, and destroy only the run-owned Kind cluster, registry, and network.

Every failure must save diagnostics before changing or deleting the affected resources. No finalizer is removed manually. Setup probes are labeled separately from qualification requests.

## Source and image provenance

`tests/shared/resolve_sources.py --scenario grid-llmd-kind` resolves the AIGC controller, MaaS, ExtProc, Praxis AI, Grid, Grid operator, Kuadrant, KServe, EPP, simulator, and payload processor. This scenario does not resolve a GitOps repository. Moving refs become full commit SHAs and clean detached worktrees; record each build-context hash. The KServe installer reads chart package versions from the three `Chart.yaml` files at the locked source revision, records their metadata hashes and the runtime chart values hash, and packages/renders those exact versions. Do not assume the chart package version equals the Git tag: the tested KServe `v0.21.0` source revision declares `v0.21.0-rc1` chart packages and runtime defaults.

`build-kind-images.sh` runs `prepare-kind-image-sources.py`, which makes an additional clean MaaS worktree for the MaaS API image and records the controller patch identity. The packaged patch is in `patches/maas-kind-cleanup-tls.patch`; it must apply to the resolved MaaS source, and its normalized file paths, changed lines, and context must exactly match the applied diff. Only Git blob IDs and line offsets are normalized so unrelated upstream line movement does not break provenance. The image helper validates the full image plan before starting sequential builds. The ExtProc image includes the qualification-only `k8s-e2e` feature. Capture source SHA, raw and normalized patch hashes, context hashes, OCI labels, immutable digests, architecture, and deployed runtime `imageID` for each image.

The shared `build_and_push.sh` helper defaults `DOCKERFILE` to `Containerfile`, builds from a clean, detached source lock, and requires an explicit Containerfile for each source:

| Image | Locked source | Containerfile or build context |
| --- | --- | --- |
| KServe controller | `kserve` | `llmisvc-controller.Dockerfile` |
| MaaS API | `maas` | `maas-api/Dockerfile`, context `maas-api/` |
| MaaS controller | `maas` plus the packaged Kind patch | `maas-controller/Dockerfile` |
| Tenant payload processor | `payload-processing` | `Dockerfile` |
| AIGC controller | `aigc` | `Dockerfile` |
| Praxis provider proxy | `praxis-ai` | `Containerfile` |
| Grid Gateway | `grid` | `deploy/gateway/Containerfile` |
| Grid operator | `grid-operator` | `deploy/operator/Containerfile` |
| EPP | `epp` | `Dockerfile.epp` |
| llm-d simulator | `llm-d-sim` | `Dockerfile` |
| Praxis ExtProc | `extproc` | `Containerfile` |

Kind pins Kuadrant v1.4.7 for this installation. The shared source resolver's default remains v1.4.2; the newer pin is scoped to this scenario.

## One-command lifecycle and Actions invocation

`run-kind-lifecycle.sh` owns the full run boundary: it resolves sources once, builds and records the images, provisions the run-owned Kind environment, runs the dependency and serving gates, executes the traffic xtask, restores temporary MaaS state, and performs ordered natural cleanup. A failed run preserves the first failure and sanitized diagnostics for the workflow cleanup step. It never deletes a finalizer or prunes shared Docker resources.

The manually triggered workflow is `.github/workflows/kind-native-grid-llmd-e2e.yaml`. Select an exact branch or commit from `nerdalert/praxis-extproc`; the workflow checks out that revision, locks all runtime sources, records the tested SHA, and uses the explicit `gha-standard-4cpu` profile. It does not use provider credentials or write permission. Its cleanup retry runs after failure or cancellation, evidence is sanitized before upload, and private state is removed after the artifact upload attempt.

For a local run with a fresh, isolated host that satisfies the full profile guard:

```bash
SOURCE_MODE=main \
CLUSTER_ENV=kind \
RESOURCE_PROFILE=reduced-dev \
KIND_CAPACITY_PROFILE=full \
KIND_RUN_ROOT="/tmp/grid-llmd-kind-$(date -u +%Y%m%dT%H%M%SZ)" \
EXTPROC_REPOSITORY_URL=https://github.com/nerdalert/praxis-extproc.git \
EXTPROC_REF=<exact-fork-commit> \
bash tests/grid-llmd-e2e/run-kind-lifecycle.sh --mode run
```

The four-CPU profile is reserved for `ubuntu-24.04` Actions, where the runner VM constrains both the Docker workloads and the Kind node. Lowering an admission threshold on an unconstrained shared host is not a four-CPU experiment. The workflow records runner-visible CPUs, Docker capacity, cgroup CPU and memory data, disk usage, phase durations, scheduling failures, and OOM events.

The run evidence tree contains the run ID and context/API identity, source/image locks, phase results, request-correlated traffic evidence, resource samples, and diagnostics. The kubeconfig, client token, and other private state stay under the separate private-state directory and are never uploaded.

The final report must show these proofs from the same source and image freeze:

- Both real serving graphs are Ready, independent, and match the pinned runtime images.
- Both providers are current and Available; authenticated EPP samples match two candidate score breakdowns and ranks.
- A current Distributed overlay and its exact revision are consumed by the serving Gateway.
- Baseline reaches the current top-ranked pool and has HTTP success, ExtProc body digest/response marker, trusted provider/backend attribution, one upstream attempt, and a matching simulator completion receipt.
- Simulator pressure produces the full EPP metric → Grid score/rank → overlay revision → Gateway revision → alternate-pool receipt chain.
- Signal recovery produces the reverse chain and a receipt from the original pool.
- ExtProc failure is observed, the client receives the expected error, and there are zero backend requests and zero simulator receipts.
- ExtProc restoration produces a further successful attributed request and matching receipt.
- Natural GridNetwork withdrawal/finalizer completion precedes GridSite and operator deletion; run-owned environment cleanup succeeds.

The qualification runner consumes the same source lock used for image builds; it does not resolve moving refs again when `--source-locks-file` is supplied:

```bash
GRID_LLMD_CLUSTER_PLATFORM=kind \
cargo run --locked -p xtask -- e2e grid-llmd \
  --mode run --qualification-scope routing-smoke \
  --context "$KUBE_CONTEXT" --kubeconfig "$KUBECONFIG" \
  --gateway-url "$GRID_LLMD_GATEWAY_URL" \
  --source-locks-file "$RUN_DIR/source-resolution/source-locks.json" \
  --require-signal-transition \
  --evidence-dir "$RUN_DIR/qualification"
```

An HTTP 200 without attribution or a matching simulator receipt does not pass. A passing run does not cover GPU saturation, performance, Enrollment prerequisites, or ExternalModel/ExternalProvider translation.
