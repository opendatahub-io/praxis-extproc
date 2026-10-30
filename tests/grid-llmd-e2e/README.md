# Native Grid, llm-d, and Praxis ExtProc E2E

This harness qualifies signal-driven routing from a Praxis Gateway through the ODH Grid operator to two independent llm-d serving pools. It is intended for externally provisioned OpenShift/RHOAI clusters and GitHub Actions nightlies. It does not qualify AIGC ExternalModel or ExternalProvider translation.

## Request path

~~~text
Client with verified TLS
  -> task-owned Praxis Gateway and HTTPRoute
  -> Praxis ExtProc request/response filters
  -> intelligent_route reads the current Distributed Grid overlay
  -> mTLS provider hop to the selected pool Gateway
  -> provider_route validates the selected candidate, removes client-supplied credentials, and owns provider attribution
  -> credential_inject sends a short-lived, audience-scoped projected task ServiceAccount token to the pool AuthPolicy
  -> pool HTTPRoute -> LLMInferenceService / InferencePool -> EPP -> llm-d-sim
~~~

The Grid operator collects authenticated EPP metrics and publishes candidate scores, ranks, and a versioned overlay. Praxis selects according to that overlay. Each provider hop uses mTLS and a provider-local provider_route; the provider proxy writes trusted response attribution after removing peer-supplied attribution and client credentials. The task-scoped client ServiceAccount is explicitly projected into each provider proxy with the pool AuthPolicy audience and a bounded lifetime; `credential_inject` reads the projected token at request time, and automount remains disabled. Token bytes are never written to Praxis config, manifests, or evidence. The harness independently correlates each request with the selected simulator’s completion log. A response body or an HTTP 200 alone is not routing proof.

During initial Gateway installation, `GRID_GATEWAY_BOOTSTRAP_MODE=true` permits only the checked static 503 startup responder while mounting the current Grid overlay. Normal BYO configs must consume `/etc/praxis/routing/routing-overlay.json`. Keep inference traffic disabled until the final operator-generated config and overlay revision pass the pretraffic checks.

MaaS and RHOAI are checked as platform prerequisites. They do not translate or select an ExternalModel in this lane. Enrollment is reported separately: the routing-smoke scope records it as unqualified unless the cluster’s etcd encryption prerequisite is proven.

## Modes and ownership

The primary workflow uses an existing RHOAI/OpenShift cluster. It reads cluster state during preflight and changes only explicitly run-owned resources during deployment and traffic phases. The optional cold-deploy.sh bootstrap installs the platform through the pinned odh-gitops Argo Application; use it only when the external cluster lacks RHOAI/MaaS. Platform installation and native routing qualification are separate phases.

Every run uses a new UTC run ID, explicit Kubernetes context, private kubeconfig/state paths, evidence directory, operator namespace, task namespace, labels, source locks, and image digests. Do not reuse cluster-local image references, namespaces, Pod UIDs, certificates, or state from another cluster. Shared-resource changes must record original UID and exact field presence/value before mutation. In particular, snapshot and restore DataScienceCluster.spec.components.kserve.enableLLMInferenceServiceTLS on the same UID.

## Inputs and profiles

Required local tools include oc, kubectl, helm, jq, Python 3, Cargo, a container engine, and skopeo. Non-plan commands require an explicit context and kubeconfig. Configure KUBE_CONTEXT, KUBECONFIG, EXPECTED_API_SERVER, and the exact `GRID_RUN_LABEL` used on run-owned namespace/resources; use KUBE_CA_FILE when API CA verification is available.

The development smoke uses:

~~~text
SOURCE_MODE=main
CLUSTER_ENV=ephemeral-dev
RESOURCE_PROFILE=reduced-dev
~~~

reduced-dev waives only the full worker-size and aggregate-CPU baseline. Ready workers, healthy ClusterOperators, default storage, zero Pending Pods, webhooks, and application readiness remain gates. It lowers Prometheus CPU request to 50m, Thanos to 10m, and the downstream Grid operator to 25m while preserving memory, limits, replicas, storage, and retention. The result is labeled DEV-SMOKE / REDUCED-CAPACITY.

ALLOW_INSECURE_DEV_API=true is permitted only for a disposable development API whose kubeconfig cannot verify its CA. Keep Gateway and provider TLS verification enabled. Never put login tokens, auth headers, kubeconfigs, Secret values, private keys, raw certificates, or private endpoint URLs in the repository or evidence.

## Sources and images

Resolve sources once per run with tests/shared/resolve_sources.py. main mode resolves moving refs to immutable SHAs; the resulting source-locks.json is the source of truth for that run. It includes the remote, resolved SHA, clean detached worktree, and build-context hash. Preserve the caller checkout and build from the resolved worktrees, not from a dirty working tree.

Build and push the Grid Gateway, downstream ODH Grid operator, Praxis AI provider proxy, Praxis ExtProc, and EPP images with tests/shared/build_and_push.sh. It verifies the clean source lock before building, applies OCI source/revision/build-context labels, pushes the image, resolves the immutable digest, and verifies the registry manifest. For ExtProc, include the qualification filter feature:

~~~text
EXT_PROC_CARGO_FEATURES=responses,aws-sigv4,store-postgres-cert-auth,k8s-e2e
~~~

The k8s-e2e feature is qualification-only and must not be included in release or FIPS images. Record the Containerfile hash, feature list, image digest, architecture, OCI labels, and deployed runtime image IDs. Build llm-d-sim with `SERVING_POOLS_MODE=build` in the run-owned task namespace. That helper creates the owned ImageStream/BuildConfig, checks the locked Git revision and image labels, and records the immutable digest used by both pools. Do not push directly to its ImageStream name before running that helper; an integrated-registry push may create an ImageStream without the run ownership labels. Do not use floating image tags or historical digest defaults as provenance.

Example for one component, with registry auth and trust material kept outside the evidence tree:

~~~bash
COMPONENT=llm-d-sim \
SOURCE_COMPONENT=llm-d-sim \
SOURCE_LOCKS_FILE="$RUN_DIR/source-locks.json" \
IMAGE_REPOSITORY="$PUSH_REGISTRY/$RUN_NAMESPACE/llm-d-sim" \
DEPLOY_IMAGE_REPOSITORY="$CLUSTER_REGISTRY/$RUN_NAMESPACE/llm-d-sim" \
E2E_RUN_ID="$RUN_ID" \
EVIDENCE_DIR="$RUN_DIR/images/llm-d-sim" \
REGISTRY_AUTH_FILE="$PRIVATE_STATE/registry-auth.json" \
REGISTRY_CERT_DIR="$PRIVATE_STATE/registry-certs" \
bash tests/shared/build_and_push.sh
~~~

The build helper returns the digest-pinned deploy reference on stdout. Record its JSON provenance beside the run locks.

The resolver component names and xtask qualification names differ for a few images. Use this mapping when forming the xtask pin lists:

| Resolver/source-lock component | Qualification source key | Qualification image key |
|---|---|---|
| grid-operator | praxis-ai-grid-operator | praxis-ai-grid-operator |
| grid | grid | grid-gateway |
| extproc | praxis-extproc | praxis-extproc |
| epp | llm-d-epp | llm-d-epp |
| praxis-ai | praxis-ai | praxis-ai |
| llm-d-sim | llm-d-sim | llm-d-sim |
| maas | maas | maas-controller (record the installed platform image digest) |

## Deployment and gates

Use the existing helpers in this order, supplying fresh source SHAs, chart hashes, image digests, run labels, and evidence paths explicitly:

1. Verify the API server, /readyz, OpenShift version, workers, storage, ClusterOperators, CatalogSources, and Pending Pods. Verify the full RHOAI/MaaS readiness gate; do not reinstall an already-ready platform.
2. Install the downstream ODH Grid operator and task-owned GridNetwork/GridSite. Verify current-generation status, TLS identity, Grid CA, endpoint, CRDs, chart hash, image digest, and runtime image ID.
3. Create the task Gateway/HTTPRoute and separate pool Gateways. Require current Accepted, Programmed, and ResolvedRefs=True status and verify the task route targets only the Grid Gateway Service.
4. Create two independent simulator-backed LLMInferenceService/InferencePool graphs. Require both LLMInferenceServices and EPP Deployments Ready, usable backend Services and EndpointSlices, exactly one Ready pinned simulator per pool, disjoint simulator Pod UIDs, and matching runtime image IDs.
5. Register two current-generation providers and require authenticated EPP metrics for both pools. Configure scoreFirst, queueDepth, deterministic selection, and a 10 second refresh. Require exactly two candidates and exact agreement between EPP samples, score breakdowns, ranks, Distributed overlay revision, and the revision consumed by the serving Gateway.
6. Generate/mount the operator-owned Praxis configuration only after provider endpoints exist. Use the source-supported intelligent_route plus mTLS provider_route topology. The mounted router and both pool HTTPRoutes must agree on model/path, trust, candidate identity, and provider attribution before traffic.
7. Deploy the task-owned ExtProc filter and client identity. Require the current Deployment generation and completed rollout, exactly one active Ready Pod, pinned runtime image ID, verified Gateway TLS/SNI, and ExtProc attachment.

Relevant helpers are cold-deploy.sh, deploy-grid-operator.sh, configure-grid-site-endpoint.sh, deploy-gateway-path.sh, deploy-pool-gateways.sh, deploy-serving-pools.sh, deploy-grid-providers.sh, deploy-extproc-filter.sh, and deploy-grid-client-identity.sh. Each helper supports a non-mutating plan mode where applicable; render/server-side dry-run before Helm apply.

The required deployment order is to apply both serving pools, register and verify both providers, then run inference qualification. With the run-specific environment above already exported, the exact helper entry points are:

~~~bash
SERVING_POOLS_MODE=build bash tests/grid-llmd-e2e/deploy-serving-pools.sh
SERVING_POOLS_MODE=apply bash tests/grid-llmd-e2e/deploy-serving-pools.sh
GRID_PROVIDERS_MODE=apply bash tests/grid-llmd-e2e/deploy-grid-providers.sh
cargo run --locked -p xtask -- e2e grid-llmd --mode preflight
cargo run --locked -p xtask -- e2e grid-llmd --mode run --qualification-scope routing-smoke --require-signal-transition
~~~

## Traffic acceptance

Run cargo run --locked -p xtask -- e2e grid-llmd --mode preflight first, with the live resources and evidence inputs below. Do not run a baseline until preflight proves all pool, provider, score, overlay, Gateway revision, route, mTLS, attribution, ExtProc, retry, simulator request-ID, and TLS contracts in the current generation.

Then run with --require-signal-transition and both pressure and recovery hooks. The runner derives the expected baseline pool from the current rank-1 candidate; it does not assume Pool A wins. If the overlay ranks Pool B first, the baseline expectation is Pool B and the pressure signal targets that pool. The pressure overlay must rank the other pool first.

A baseline passes only with all of the following in one request:

- HTTP 200, a unique request ID, no retry, and the selected pool matching live overlay rank.
- ExtProc response marker and request-body SHA-256 oracle matching the exact serialized request.
- Trusted provider/backend attribution and the expected simulator response identity.
- The runner keeps the unique client request ID and records the response `x-request-id` separately. Envoy may replace the client ID; the pinned simulator's `Finished processing request` log must match the backend-visible response ID. Raw simulator logs are not persisted.
- Gateway upstream request evidence and exact Grid/Gateway revision evidence.

After baseline passes, the pressure hook changes only the selected simulator’s supported /admin/config signal through the run-owned control ConfigMap. Require simulator control, authenticated EPP sample, score/rank reversal, a new Distributed overlay, Gateway consumption, and an independent receipt from the other pool. Recovery restores the original metric/control and proves the same chain returns to the baseline-ranked pool. Run ExtProc failure-closed last; require a correlated 5xx and zero backend receipts, then verify ExtProc recovery.

A failed baseline stops before pressure. The runner captures diagnostics before cleanup. A run is PASS only when baseline, pressure reroute, recovery, and failure-closed all pass in the same run. An HTTP 200 without simulator receipt, attribution, ExtProc proof, or matching rank is a failure. The failure-closed hook writes into a fresh `failure-closed/` child of the runner evidence directory. It records the unique client request ID and returned Envoy `x-request-id` separately because Envoy can replace the client value.

## xtask configuration

Pass repeated values as comma-separated GRID_LLMD_* environment values or repeated flags. The run requires:

- GRID_LLMD_GATEWAY_URL, GRID_LLMD_NAMESPACE, GRID_LLMD_MODEL, GRID_LLMD_GATEWAY_CA_FILE and either a live GRID_LLMD_AUTH_TOKEN or explicit GRID_LLMD_NO_AUTH=true when the task route has no auth policy.
- GRID_LLMD_GATE_RESOURCES for RHOAI, MaaS, Grid, Gateway, and both serving resources; GRID_LLMD_HTTPROUTE_RESOURCE, GRID_LLMD_GRID_GATEWAY_SERVICE, and the downstream operator Deployment.
- Two GRID_LLMD_POOL_RESOURCES, matching GRID_LLMD_POOL_MODEL_RESOURCES and GRID_LLMD_POOL_BACKEND_SERVICES; explicit GRID_LLMD_IPP_STATE and named IPP resources when present.
- Both provider names and expected provider/backend attribution values, exact pool response oracles, the ExtProc body-digest header, ExtProc marker/filter resources, and the pool route-contract hook.
- Two authenticated EPP metrics services with pool labels, the EPP sampler hook/account, metric name, queue capacity, and scoreFirst overlay evidence.
- Two simulator control ConfigMaps tagged @<InferencePool-name>, plus the simulator pressure/recovery, Gateway revision, and ExtProc failure-closed hooks. The signal hook maps each logical InferencePool name to its LLMInferenceService label through `GRID_LLMD_POOL_MODEL_RESOURCES`.
- Complete GRID_LLMD_SOURCE_REVISIONS, GRID_LLMD_IMAGE_PINS, and GRID_LLMD_RUNTIME_IMAGE_DIGESTS from the current run locks.

A typical invocation is:

~~~bash
cargo run --locked -p xtask -- e2e grid-llmd \
  --mode run --qualification-scope routing-smoke \
  --context "$KUBE_CONTEXT" --kubeconfig "$KUBECONFIG" \
  --gateway-url "$GRID_LLMD_GATEWAY_URL" \
  --require-signal-transition \
  --evidence-dir "$RUN_DIR/qualification"
~~~

Set the remaining GRID_LLMD_* values from the live resource inventory and run locks. Keep auth credentials in environment injection from the CI secret store; never place them in command-line arguments or shell history. Enrollment remains separately unqualified in routing-smoke.

## Evidence, cleanup, and CI

Each run writes a new private evidence directory with the source/image locks, sanitized invocations, phase/check report, score/metric samples, Grid overlay and Gateway revisions, resource snapshots, simulator receipt summaries, and failure classification. Do not copy kubeconfigs, registry auth, Secret values, private keys, raw credentials, or unredacted logs into evidence.

On failure, preserve diagnostics, then remove only resources labeled with the current run ID. Restore the original DSCI field and any temporary registry route on the same resource UID and verify restoration. Keep shared RHOAI/MaaS components intact. Do not run destructive namespace cleanup without a current run ownership check.

For nightlies, provision an OpenShift/RHOAI cluster externally and run the existing-cluster path on a self-hosted GitHub Actions runner with the required CLI tools, source access, registry credentials, API context, and TLS trust. Keep the optional cold bootstrap in a separate workflow/job. Request volume is bounded: one baseline request, one post-pressure reroute request, one recovery request, and one failure-closed request. This is functional qualification, not a performance benchmark.

Known limits: routing-smoke does not qualify Enrollment; synthetic queue/KV pressure is simulator-reported and is not real GPU saturation; the harness proves only the declared HTTP routes, providers, overlays, and ExtProc hops; TLS streaming/idle scenarios are not claimed unless explicitly run.
