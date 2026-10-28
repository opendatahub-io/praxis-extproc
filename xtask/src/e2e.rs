// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Qualification-only end-to-end runners.
//!
//! The runner in this module deliberately does not install Grid, RHOAI, `MaaS`,
//! or an inference pool. Those components have their own deployment owners.
//! Instead, it qualifies the already-prepared cross-component data plane on a
//! named cluster and records enough evidence for a nightly failure to be
//! actionable.

#![allow(
    clippy::assigning_clones,
    clippy::doc_markdown,
    clippy::disallowed_methods,
    clippy::format_in_format_args,
    clippy::if_not_else,
    clippy::missing_docs_in_private_items,
    clippy::needless_pass_by_value,
    clippy::struct_excessive_bools,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "the qualification runner is synchronous terminal tooling with structured evidence and report data"
)]

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

const REPORT_SCHEMA: &str = "praxis-extproc.grid-llmd-e2e/v1";
const DEFAULT_GATEWAY_PATH: &str = "/v1/chat/completions";
const DEFAULT_PROMPT: &str = "grid-llmd-extproc-e2e";
const GATEWAY_REVISION_HOOK_TIMEOUT_CAP_SECONDS: u64 = 60;
const DEFAULT_PROVIDER_HEADER: &str = "x-grid-llmd-provider-gateway";
const DEFAULT_BACKEND_HEADER: &str = "x-ai-demo-provider-gateway";
const FULL_REQUIRED_GATES: [&str; 6] = ["rhoai", "maas", "grid", "enrollment", "gateway", "llminferenceservice"];
const ROUTING_SMOKE_REQUIRED_GATES: [&str; 5] = ["rhoai", "maas", "grid", "gateway", "llminferenceservice"];
const REQUIRED_SOURCE_COMPONENTS: [&str; 7] = [
    "grid",
    "praxis-ai-grid-operator",
    "praxis-extproc",
    "praxis-ai",
    "maas",
    "llm-d-epp",
    "llm-d-sim",
];
const REQUIRED_IMAGE_COMPONENTS: [&str; 8] = [
    "praxis-ai-grid-operator",
    "grid-gateway",
    "grid-enrollment",
    "maas-controller",
    "praxis-ai",
    "praxis-extproc",
    "llm-d-epp",
    "llm-d-sim",
];
const ROUTING_SMOKE_REQUIRED_SOURCE_COMPONENTS: [&str; 7] = [
    "grid",
    "praxis-ai-grid-operator",
    "praxis-ai",
    "praxis-extproc",
    "maas",
    "llm-d-epp",
    "llm-d-sim",
];
const ROUTING_SMOKE_REQUIRED_IMAGE_COMPONENTS: [&str; 7] = [
    "praxis-ai-grid-operator",
    "praxis-ai",
    "grid-gateway",
    "maas-controller",
    "praxis-extproc",
    "llm-d-epp",
    "llm-d-sim",
];

static KUBE_COMMAND_CONFIG: OnceLock<KubeCommandConfig> = OnceLock::new();
static OUTPUT_REDACTIONS: OnceLock<OutputRedactions> = OnceLock::new();

struct KubeCommandConfig {
    api_ca_file: Option<PathBuf>,
    kubeconfig: Option<PathBuf>,
    allow_insecure_dev_api: bool,
}

struct OutputRedactions {
    auth_token: Option<String>,
    gateway_url: Option<String>,
    auth_header: String,
    prompt: String,
}

/// CLI arguments for `cargo xtask e2e`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Qualification suite to run.
    #[command(subcommand)]
    suite: Suite,
}

/// Available qualification suites.
#[derive(Debug, Subcommand)]
enum Suite {
    /// Qualify the Grid signal-driven llm-d and ExtProc data plane.
    GridLlmd(Box<GridLlmdArgs>),
    /// Source-gated optional MaaS/ExternalModel front door to the Grid lane.
    GridMaas(GridMaasArgs),
}

/// Source channel used to resolve integration repositories.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum SourceMode {
    /// Resolve current development branches and record their exact commits.
    #[default]
    Main,
    /// Resolve the checked-in immutable qualification commits.
    Locked,
}

impl SourceMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Locked => "locked",
        }
    }
}

/// Safe local actions available for the optional Grid/MaaS lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum GridMaasMode {
    /// Describe the path and current compatibility gate without resolving sources.
    #[default]
    Plan,
    /// Resolve clean sources and evaluate the source compatibility contract.
    Compatibility,
}

/// Arguments for the optional Grid/MaaS integration lane.
#[derive(Debug, ClapArgs)]
struct GridMaasArgs {
    /// Local action. The lane is blocked before cluster work until compatibility passes.
    #[arg(long, value_enum, default_value_t = GridMaasMode::Plan)]
    mode: GridMaasMode,

    /// Source channel. Development runs use main; locked is for qualification.
    #[arg(long, env = "SOURCE_MODE", value_enum, default_value_t = SourceMode::Main)]
    source_mode: SourceMode,

    /// Resource profile to record. Reduced-dev is accepted only for ephemeral development.
    #[arg(long, env = "RESOURCE_PROFILE", value_enum, default_value_t = ResourceProfile::Strict)]
    resource_profile: ResourceProfile,

    /// Cluster classification used to constrain the reduced-dev profile.
    #[arg(long, env = "CLUSTER_ENV")]
    cluster_env: Option<String>,

    /// Include platform GitOps source in source resolution.
    #[arg(long, env = "E2E_USE_PLATFORM_MANIFESTS", default_value_t = true)]
    platform_manifests: bool,

    /// Evidence directory. Defaults to target/e2e-grid-maas/<epoch-seconds>.
    #[arg(long, env = "GRID_MAAS_EVIDENCE_DIR")]
    evidence_dir: Option<PathBuf>,
}

/// Arguments for the Grid/llm-d/ExtProc qualification runner.
#[derive(Debug, ClapArgs)]
struct GridLlmdArgs {
    /// Operation to perform. `plan` is the safe default and makes no cluster
    /// calls; `run` performs probes and invokes explicitly supplied hooks.
    #[arg(long, value_enum, default_value_t = Mode::Plan)]
    mode: Mode,

    /// Qualification scope. `full` requires Enrollment identity/TLS checks;
    /// `routing-smoke` records Enrollment as unqualified and reports an overall
    /// partial result even when the routing path passes.
    #[arg(long, env = "GRID_LLMD_QUALIFICATION_SCOPE", value_enum, default_value_t = QualificationScope::Full)]
    qualification_scope: QualificationScope,

    /// Kubernetes context. Required for non-plan modes.
    #[arg(long, env = "KUBE_CONTEXT")]
    context: Option<String>,

    /// Namespace containing the task-owned qualification objects.
    #[arg(long, env = "GRID_LLMD_NAMESPACE", default_value = "")]
    namespace: String,

    /// Kubeconfig path. The path is never written to evidence.
    #[arg(long, env = "KUBECONFIG")]
    kubeconfig: Option<PathBuf>,

    /// Kubernetes API CA bundle. The path is never written to evidence.
    #[arg(long, env = "KUBE_CA_FILE")]
    api_ca_file: Option<PathBuf>,

    /// Opt in to unverified Kubernetes API TLS for an explicitly disposable
    /// development cluster only. Requires CLUSTER_ENV=ephemeral-dev and no API CA.
    #[arg(long, env = "ALLOW_INSECURE_DEV_API", default_value_t = false)]
    allow_insecure_dev_api: bool,

    /// Resource/capacity profile. Strict is the default. Reduced-dev is only
    /// accepted when CLUSTER_ENV=ephemeral-dev and remains fully functional-gated.
    #[arg(long, env = "RESOURCE_PROFILE", value_enum, default_value_t = ResourceProfile::Strict)]
    resource_profile: ResourceProfile,

    /// Resolve development sources from main or immutable qualification pins.
    #[arg(long, env = "SOURCE_MODE", value_enum, default_value_t = SourceMode::Main)]
    source_mode: SourceMode,

    /// Include odh-gitops in clean source resolution for platform-manifest runs.
    #[arg(long, env = "E2E_USE_PLATFORM_MANIFESTS", default_value_t = true)]
    platform_manifests: bool,

    /// Cluster classification used to constrain the dev-only API TLS opt-in.
    #[arg(long, env = "CLUSTER_ENV")]
    cluster_env: Option<String>,

    /// Gateway URL used for the inference probes. Required by `--mode run`.
    #[arg(long, env = "GRID_LLMD_GATEWAY_URL")]
    gateway_url: Option<String>,

    /// Optional curl --connect-to mapping for a load balancer while retaining
    /// the Gateway hostname in the HTTPS URL for SNI and certificate checks.
    /// Format: URL_HOST:URL_PORT:CONNECT_HOST:CONNECT_PORT.
    #[arg(long, env = "GRID_LLMD_GATEWAY_CONNECT_TO")]
    gateway_connect_to: Option<String>,

    /// Name of the Grid consumer Gateway recorded in GridNetwork status.
    #[arg(long, env = "GRID_LLMD_GRID_GATEWAY_NAME")]
    grid_gateway_name: Option<String>,

    /// Deployment reference for the downstream ODH Grid operator.
    #[arg(long, env = "GRID_LLMD_GRID_OPERATOR_DEPLOYMENT")]
    grid_operator_deployment: Option<String>,

    /// Service reference for the direct Grid Gateway HTTPRoute backend.
    #[arg(long, env = "GRID_LLMD_GRID_GATEWAY_SERVICE")]
    grid_gateway_service: Option<String>,

    /// Relative inference path appended to `--gateway-url`.
    #[arg(long, env = "GRID_LLMD_GATEWAY_PATH", default_value = DEFAULT_GATEWAY_PATH)]
    gateway_path: String,

    /// Model sent in the deterministic llm-d-sim request body.
    #[arg(long, env = "GRID_LLMD_MODEL", default_value = "")]
    model: String,

    /// Request header carrying the model name for Grid routing.
    #[arg(long, env = "GRID_LLMD_MODEL_HEADER", default_value = "x-model")]
    model_header: String,

    /// User content sent in each inference request.
    #[arg(long, env = "GRID_LLMD_PROMPT", default_value = DEFAULT_PROMPT)]
    prompt: String,

    /// API credential sent to the configured Grid Gateway. Read only from the environment;
    /// credentials must never appear in process arguments or shell history.
    #[arg(skip)]
    auth_token: Option<String>,

    /// Send no client credential. Use only when the inspected task route has no auth policy.
    #[arg(long, env = "GRID_LLMD_NO_AUTH", default_value_t = false)]
    no_auth: bool,

    /// Authentication header name, for example authorization or x-api-key.
    #[arg(long, env = "GRID_LLMD_AUTH_HEADER", default_value = "authorization")]
    auth_header: String,

    /// CA bundle for Gateway TLS verification. TLS verification is always enabled.
    #[arg(long, env = "GRID_LLMD_GATEWAY_CA_FILE")]
    gateway_ca_file: Option<PathBuf>,

    /// Response header emitted by `e2e_body_oracle`; its value must equal the
    /// SHA-256 of the exact serialized request body sent by this probe.
    #[arg(long, env = "GRID_LLMD_EXT_PROC_BODY_ORACLE")]
    extproc_body_oracle: Option<String>,

    /// Optional extra ExtProc request-body mutation oracle expected to be
    /// echoed by the deterministic backend response. The required request
    /// proof is the body SHA-256 response header above.
    #[arg(long, env = "GRID_LLMD_EXT_PROC_REQUEST_ORACLE")]
    extproc_request_oracle: Option<String>,

    /// Simulator-owned response identity expected from pool A at baseline and recovery.
    #[arg(long, env = "GRID_LLMD_POOL_A_RESPONSE_ORACLE")]
    pool_a_response_oracle: Option<String>,

    /// Simulator-owned response identity expected from pool B under pressure.
    #[arg(long, env = "GRID_LLMD_POOL_B_RESPONSE_ORACLE")]
    pool_b_response_oracle: Option<String>,

    /// Envoy/Istio ExtProc attachment resources, one per ExtProc hop, formatted
    /// kind/name or namespace/kind/name. Repeat or use a comma-separated list.
    #[arg(
        long = "extproc-filter-resource",
        env = "GRID_LLMD_EXTPROC_FILTER_RESOURCES",
        value_delimiter = ','
    )]
    extproc_filter_resources: Vec<String>,

    /// Number of distinct ExtProc-owned response markers expected on the request path.
    #[arg(long, env = "GRID_LLMD_EXPECTED_EXTPROC_HOPS", default_value_t = 1)]
    expected_extproc_hops: usize,

    /// Retained only for report compatibility with an older, separate MaaS
    /// diagnostic. ExternalModel translation is not part of this qualification.
    #[arg(skip)]
    external_model_resource: Option<String>,

    /// Retained only for report compatibility with an older, separate MaaS
    /// diagnostic. ExternalProvider translation is not part of this qualification.
    #[arg(skip)]
    external_provider_resource: Option<String>,

    /// Task-owned direct Grid Gateway HTTPRoute, formatted kind/name or namespace/kind/name.
    #[arg(long, env = "GRID_LLMD_HTTPROUTE_RESOURCE")]
    httproute_resource: Option<String>,

    /// Enrolled GridSite resource formatted gridsite/name or namespace/gridsite/name.
    #[arg(long, env = "GRID_LLMD_GRID_SITE_RESOURCE")]
    grid_site_resource: Option<String>,

    /// Site identity Secret reference formatted namespace/name.
    #[arg(long, env = "GRID_LLMD_SITE_IDENTITY_SECRET")]
    site_identity_secret: Option<String>,

    /// Grid CA Secret reference formatted namespace/name.
    #[arg(long, env = "GRID_LLMD_GRID_CA_SECRET")]
    grid_ca_secret: Option<String>,

    /// Expected certificate SAN on the enrolled site identity.
    #[arg(long, env = "GRID_LLMD_EXPECTED_SITE_SAN")]
    expected_site_san: Option<String>,

    /// EPP metrics Service reference formatted service/name:port[#pool-name].
    /// Repeat for separate EPP services; one service may expose both pools.
    #[arg(
        long = "epp-metrics-service",
        env = "GRID_LLMD_EPP_METRICS_SERVICES",
        value_delimiter = ','
    )]
    epp_metrics_services: Vec<String>,

    /// Authenticated EPP metrics sampler used for signal qualification.
    #[arg(long, env = "GRID_LLMD_EPP_METRICS_HOOK")]
    epp_metrics_hook: Option<PathBuf>,

    /// Dedicated ServiceAccount allowed to scrape the EPP metrics endpoint.
    #[arg(long, env = "GRID_LLMD_EPP_METRICS_SERVICE_ACCOUNT")]
    epp_metrics_service_account: Option<String>,

    /// Namespace containing the EPP metrics scraper ServiceAccount.
    #[arg(long, env = "GRID_LLMD_EPP_METRICS_SERVICE_ACCOUNT_NAMESPACE")]
    epp_metrics_service_account_namespace: Option<String>,

    /// Exact EPP metric name consumed by Grid scoring.
    #[arg(long, env = "GRID_LLMD_EPP_METRIC_NAME")]
    epp_metric_name: Option<String>,

    /// Raw queue capacity used by Grid to normalize the EPP queue metric.
    #[arg(long, env = "GRID_LLMD_EPP_QUEUE_CAPACITY")]
    epp_queue_capacity: Option<f64>,

    /// Exact Grid InferenceProvider cluster name for pool A.
    #[arg(long, env = "GRID_LLMD_GRID_PROVIDER_A")]
    grid_provider_a: Option<String>,

    /// Exact Grid InferenceProvider cluster name for pool B.
    #[arg(long, env = "GRID_LLMD_GRID_PROVIDER_B")]
    grid_provider_b: Option<String>,

    /// Prometheus label key that identifies a pool series.
    #[arg(long, env = "GRID_LLMD_EPP_POOL_LABEL", default_value = "name")]
    epp_pool_label: String,

    /// Expected provider/backend attribution for the baseline Pool A request.
    /// Use the provider/backend-specific flags when the two response headers
    /// carry different Grid identities.
    #[arg(long, env = "GRID_LLMD_POOL_A_ATTRIBUTION")]
    pool_a_attribution: Option<String>,

    /// Expected provider/backend attribution for the pressure Pool B request.
    #[arg(long, env = "GRID_LLMD_POOL_B_ATTRIBUTION")]
    pool_b_attribution: Option<String>,

    /// Expected value of the provider/site response header for baseline Pool A.
    #[arg(long, env = "GRID_LLMD_POOL_A_PROVIDER_ATTRIBUTION")]
    pool_a_provider_attribution: Option<String>,

    /// Expected value of the provider/site response header for pressure Pool B.
    #[arg(long, env = "GRID_LLMD_POOL_B_PROVIDER_ATTRIBUTION")]
    pool_b_provider_attribution: Option<String>,

    /// Expected value of the backend/cluster response header for baseline Pool A.
    #[arg(long, env = "GRID_LLMD_POOL_A_BACKEND_ATTRIBUTION")]
    pool_a_backend_attribution: Option<String>,

    /// Expected value of the backend/cluster response header for pressure Pool B.
    #[arg(long, env = "GRID_LLMD_POOL_B_BACKEND_ATTRIBUTION")]
    pool_b_backend_attribution: Option<String>,

    /// Provider attribution response header expected from the Grid route.
    #[arg(long, env = "GRID_LLMD_PROVIDER_HEADER", default_value = DEFAULT_PROVIDER_HEADER)]
    provider_header: String,

    /// Backend attribution response header expected from the provider gateway.
    #[arg(long, env = "GRID_LLMD_BACKEND_HEADER", default_value = DEFAULT_BACKEND_HEADER)]
    backend_header: String,

    /// ExtProc-owned response marker header(s). Repeat once per ExtProc hop.
    /// A marker must be explicitly configured; a generic product/version
    /// header is not proof unless ExtProc itself owns it.
    #[arg(long = "extproc-header", env = "GRID_LLMD_EXT_PROC_HEADERS", value_delimiter = ',')]
    extproc_headers: Vec<String>,

    /// Deployments that must be ready before probing. Values are comma-separated.
    #[arg(long = "deployment", env = "GRID_LLMD_DEPLOYMENTS", value_delimiter = ',')]
    deployments: Vec<String>,

    /// CRDs that must exist before probing. Values are comma-separated.
    #[arg(
        long,
        env = "GRID_LLMD_CRDS",
        value_delimiter = ',',
        default_values = [
            "gridnetworks.grid.praxis.fast",
            "gridsites.grid.praxis.fast",
            "inferenceproviders.grid.praxis.fast",
            "inferencepools.inference.networking.k8s.io"
        ]
    )]
    crds: Vec<String>,

    /// Named integration gate resources. Each value is
    /// `gate=resource/name#condition`, for example
    /// `gateway=gateway/e2e-gateway#Programmed`. Use `#exists` when a
    /// resource has no standard readiness condition. All six gates are
    /// required for non-plan modes.
    #[arg(long = "gate-resource", env = "GRID_LLMD_GATE_RESOURCES", value_delimiter = ',')]
    gate_resources: Vec<String>,

    /// At least two independently routable llm-d pool resources. Values are
    /// `resource/name#condition`, for example `inferencepool/pool-a#Ready`.
    /// The pressure/recovery phases must move traffic between these pools.
    #[arg(long = "pool-resource", env = "GRID_LLMD_POOL_RESOURCES", value_delimiter = ',')]
    pool_resources: Vec<String>,

    /// Per-pool LLMInferenceService references formatted
    /// pool-name=namespace/llminferenceservice/name.
    #[arg(
        long = "pool-model-resource",
        env = "GRID_LLMD_POOL_MODEL_RESOURCES",
        value_delimiter = ','
    )]
    pool_model_resources: Vec<String>,

    /// Per-pool serving Service references formatted pool-name=namespace/service/name.
    #[arg(
        long = "pool-backend-service",
        env = "GRID_LLMD_POOL_BACKEND_SERVICES",
        value_delimiter = ','
    )]
    pool_backend_services: Vec<String>,

    /// Whether this serving path uses IPP and the explicit IPP resources to capture when present.
    #[arg(long, env = "GRID_LLMD_IPP_STATE", value_enum, default_value_t = IppState::Unknown)]
    ipp_state: IppState,

    /// IPP configuration/attachment resources formatted kind/name or namespace/kind/name.
    #[arg(long = "ipp-resource", env = "GRID_LLMD_IPP_RESOURCES", value_delimiter = ',')]
    ipp_resources: Vec<String>,

    /// Executable that applies deterministic llm-d-sim pressure and waits for
    /// the Grid overlay to converge. It receives --context, --namespace, and
    /// --evidence-dir arguments and must emit a JSON simulator-pod report.
    #[arg(long, env = "GRID_LLMD_PRESSURE_HOOK")]
    pressure_hook: Option<PathBuf>,

    /// Executable that removes pressure and waits for route recovery. It
    /// receives the same arguments and JSON report contract as --pressure-hook.
    #[arg(long, env = "GRID_LLMD_RECOVERY_HOOK")]
    recovery_hook: Option<PathBuf>,

    /// Read-only executable that proves the serving Gateway has loaded a
    /// requested Grid revision and reports the effective route retry policy
    /// and upstream counters. Every inference request must use exactly one
    /// upstream attempt, even when the controller configures a retry limit.
    /// It receives --expected-revision, --config-map-namespace, --config-map,
    /// --context, --namespace, --evidence-dir, and --phase.
    #[arg(long, env = "GRID_LLMD_GATEWAY_REVISION_HOOK")]
    gateway_revision_hook: Option<PathBuf>,

    /// Required pre-traffic assertion that the mounted Praxis model extractor and both
    /// task pool HTTPRoutes agree on the request path/model contract.
    #[arg(long, env = "GRID_LLMD_POOL_ROUTE_CONTRACT_HOOK")]
    pool_route_contract_hook: Option<PathBuf>,

    /// Task-owned failure-closed ExtProc test. The hook must prove a correlated
    /// 5xx response and zero backend receipts without touching shared filters.
    #[arg(long, env = "GRID_LLMD_EXTPROC_FAILURE_CLOSED_HOOK")]
    extproc_failure_closed_hook: Option<PathBuf>,

    /// Simulator ConfigMap references formatted configmap/name#data-key@pool
    /// or namespace/configmap/name#data-key@pool. Signal mode requires one
    /// explicitly mapped control per distinct InferencePool. Values are
    /// compared in memory only.
    #[arg(
        long = "simulator-control-resource",
        env = "GRID_LLMD_SIMULATOR_CONTROL_RESOURCES",
        value_delimiter = ','
    )]
    simulator_control_resources: Vec<String>,

    /// Optional trust source for a topology-specific provider path: `system-ca` or a
    /// namespaced Secret/ConfigMap key reference (kind/namespace/name#key).
    #[arg(long, env = "GRID_LLMD_PROVIDER_TRUST")]
    provider_trust: Option<String>,

    /// Optional read-only checker for a topology-specific provider transport.
    #[arg(long, env = "GRID_LLMD_PROVIDER_TRANSPORT_HOOK")]
    provider_transport_hook: Option<PathBuf>,

    /// Pool identity that the simulator pressure hook is expected to affect.
    #[arg(long, env = "GRID_LLMD_PRESSURE_POOL")]
    pressure_pool: Option<String>,

    /// Poll interval for EPP metrics, Grid revisions, and Gateway consumption.
    #[arg(long, env = "GRID_LLMD_POLL_INTERVAL_SECONDS", default_value_t = 5)]
    poll_interval_seconds: u64,

    /// Header used to correlate each fresh inference request.
    #[arg(long, env = "GRID_LLMD_REQUEST_ID_HEADER", default_value = "x-request-id")]
    request_id_header: String,

    /// Require both hooks and require pressure to change provider attribution
    /// and recovery to return to the baseline attribution.
    #[arg(long, env = "GRID_LLMD_REQUIRE_SIGNAL_TRANSITION")]
    require_signal_transition: bool,

    /// Maximum seconds allowed for each curl probe and hook.
    #[arg(long, env = "GRID_LLMD_TIMEOUT_SECONDS", default_value_t = 120)]
    timeout_seconds: u64,

    /// Evidence directory. Defaults to target/e2e-grid-llmd/<epoch-seconds>.
    #[arg(long, env = "GRID_LLMD_EVIDENCE_DIR")]
    evidence_dir: Option<PathBuf>,

    /// Immutable source revisions, formatted component=git-sha.
    #[arg(long = "source-revision", env = "GRID_LLMD_SOURCE_REVISIONS", value_delimiter = ',')]
    source_revisions: Vec<String>,

    /// Immutable image references, formatted component=image@sha256:digest.
    #[arg(long = "image-pin", env = "GRID_LLMD_IMAGE_PINS", value_delimiter = ',')]
    image_pins: Vec<String>,

    /// Expected runtime manifest digests, formatted component=sha256:digest.
    /// Use the exact platform child digest when an image pin is an OCI index.
    #[arg(
        long = "runtime-image-digest",
        env = "GRID_LLMD_RUNTIME_IMAGE_DIGESTS",
        value_delimiter = ','
    )]
    runtime_image_digests: Vec<String>,
}

/// Safe-to-automation operation mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum Mode {
    /// Print and record the qualification contract without contacting a cluster.
    Plan,
    /// Verify cluster prerequisites and collect read-only diagnostics.
    Preflight,
    /// Run preflight, baseline, pressure, and recovery probes.
    Run,
    /// Collect post-failure diagnostics without issuing inference traffic.
    Collect,
}

/// Scope of the qualification claim made by one runner invocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum QualificationScope {
    /// Full qualification including Enrollment identity and TLS prerequisites.
    #[default]
    Full,
    /// Direct routing smoke; Enrollment is recorded as unqualified.
    RoutingSmoke,
}

/// Resource and worker-capacity policy selected for the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum ResourceProfile {
    /// Require the complete RHOAI worker capacity baseline.
    #[default]
    Strict,
    /// Disposable development smoke with the checked-in reduced resource profile.
    ReducedDev,
}

impl ResourceProfile {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::ReducedDev => "reduced-dev",
        }
    }

    const fn is_reduced_dev(self) -> bool {
        matches!(self, Self::ReducedDev)
    }
}

impl QualificationScope {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::RoutingSmoke => "routing-smoke",
        }
    }

    const fn required_gates(self) -> &'static [&'static str] {
        match self {
            Self::Full => &FULL_REQUIRED_GATES,
            Self::RoutingSmoke => &ROUTING_SMOKE_REQUIRED_GATES,
        }
    }

    const fn enrollment_qualification(self) -> &'static str {
        match self {
            Self::Full => "required",
            Self::RoutingSmoke => "skipped-unqualified-etcd-encryption-prerequisite-unverified",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApiTlsMode {
    CaVerified,
    ExplicitEphemeralDevInsecure,
}

impl ApiTlsMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::CaVerified => "ca-verified",
            Self::ExplicitEphemeralDevInsecure => "explicit-ephemeral-dev-unverified",
        }
    }
}

fn api_tls_mode(args: &GridLlmdArgs) -> Result<ApiTlsMode, String> {
    let kubeconfig_tls_verified = args.api_ca_file.is_none()
        && kubeconfig_tls_verification_is_enabled(args.kubeconfig.as_deref(), args.context.as_deref());
    api_tls_mode_from_parts(
        args.api_ca_file.as_deref(),
        kubeconfig_tls_verified,
        args.allow_insecure_dev_api,
        args.cluster_env.as_deref(),
    )
}

fn kubeconfig_tls_verification_is_enabled(kubeconfig: Option<&Path>, context: Option<&str>) -> bool {
    let mut command = Command::new("kubectl");
    if let Some(kubeconfig) = kubeconfig {
        command.arg("--kubeconfig").arg(kubeconfig);
    }
    if let Some(context) = context {
        command.arg("--context").arg(context);
    }
    let Ok(output) = command.args(["config", "view", "--minify", "--output=json"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let Ok(document) = serde_json::from_slice::<Value>(&output.stdout) else {
        return false;
    };
    kubeconfig_document_uses_tls_verification(&document)
}

fn kubeconfig_document_uses_tls_verification(document: &Value) -> bool {
    document
        .pointer("/clusters/0/cluster")
        .is_some_and(|cluster| cluster.get("insecure-skip-tls-verify").and_then(Value::as_bool) != Some(true))
}

fn api_tls_mode_from_parts(
    api_ca_file: Option<&Path>,
    kubeconfig_tls_verified: bool,
    allow_insecure_dev_api: bool,
    cluster_env: Option<&str>,
) -> Result<ApiTlsMode, String> {
    if let Some(path) = api_ca_file {
        if path.is_file() && fs::File::open(path).is_ok() {
            return Ok(ApiTlsMode::CaVerified);
        }
        return Err("KUBE_CA_FILE is not a readable regular file".to_owned());
    }
    if kubeconfig_tls_verified {
        return Ok(ApiTlsMode::CaVerified);
    }
    if allow_insecure_dev_api {
        if cluster_env == Some("ephemeral-dev") {
            return Ok(ApiTlsMode::ExplicitEphemeralDevInsecure);
        }
        return Err("ALLOW_INSECURE_DEV_API requires CLUSTER_ENV=ephemeral-dev".to_owned());
    }
    Err("non-plan modes require a readable KUBE_CA_FILE, a TLS-verifying kubeconfig, or both CLUSTER_ENV=ephemeral-dev and ALLOW_INSECURE_DEV_API=true".to_owned())
}

fn resource_profile_authorized(profile: ResourceProfile, cluster_env: Option<&str>) -> bool {
    profile == ResourceProfile::Strict || cluster_env == Some("ephemeral-dev")
}

fn capacity_preflight_allowed(
    under_baseline_mode: bool,
    ready_workers: usize,
    workers_meeting_full_baseline: usize,
    total_allocatable_worker_cpu_milli: u64,
    incomplete_worker_capacity: usize,
    pending_pods: usize,
    control_plane_nodes_no_schedule_tainted: bool,
) -> bool {
    ready_workers >= 2
        && incomplete_worker_capacity == 0
        && pending_pods == 0
        && control_plane_nodes_no_schedule_tainted
        && (under_baseline_mode || (workers_meeting_full_baseline >= 2 && total_allocatable_worker_cpu_milli >= 20_000))
}

fn api_access_acceptable(args: &GridLlmdArgs, hook_ca_verified: bool) -> bool {
    api_access_acceptable_for_mode(api_tls_mode(args), hook_ca_verified)
}

fn api_access_acceptable_for_mode(mode: Result<ApiTlsMode, String>, hook_ca_verified: bool) -> bool {
    match mode {
        Ok(ApiTlsMode::CaVerified) => hook_ca_verified,
        Ok(ApiTlsMode::ExplicitEphemeralDevInsecure) => true,
        Err(_) => false,
    }
}

/// Declared IPP role for the serving path; present resources are captured explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum IppState {
    /// Caller has verified that IPP participates in this request path.
    Present,
    /// Caller has verified that this request path does not use IPP.
    Absent,
    /// IPP involvement has not been established.
    Unknown,
}

impl IppState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Absent => "absent",
            Self::Unknown => "unknown",
        }
    }
}

impl Mode {
    /// Stable report spelling.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Preflight => "preflight",
            Self::Run => "run",
            Self::Collect => "collect",
        }
    }
}

/// One command or probe result in the retained report.
#[derive(Debug, Serialize)]
struct Check {
    name: String,
    status: String,
    detail: String,
    observed_epoch_seconds: u64,
}

/// One inference probe and its route attribution.
#[derive(Clone, Debug, Serialize)]
struct Probe {
    phase: String,
    observed_epoch_seconds: u64,
    request_id: String,
    expected_pool: String,
    model_header: String,
    status_code: Option<u16>,
    provider_attribution: Option<String>,
    backend_attribution: Option<String>,
    extproc_markers: BTreeMap<String, String>,
    request_body_sha256: String,
    body_oracle_match: bool,
    request_oracle_match: Option<bool>,
    simulator_identity_match: bool,
    simulator_request_id_echo_match: bool,
    simulator_receipt_match: bool,
    gateway_upstream_request_delta: Option<u64>,
    gateway_retry_attempt_delta: Option<u64>,
    headers_file: String,
    body_file: String,
    stderr_file: String,
    passed: bool,
}

/// One timestamped sample of the configured live EPP metric.
#[derive(Clone, Debug, Serialize)]
struct SignalSample {
    phase: String,
    observed_epoch_seconds: u64,
    values: BTreeMap<String, f64>,
}

/// Grid's published and distributed overlay state for one phase.
#[derive(Clone, Debug, Serialize)]
struct GridRevisionRecord {
    phase: String,
    observed_epoch_seconds: u64,
    status_field: String,
    gateway: String,
    overlay_phase: String,
    network_generation: Option<u64>,
    observed_generation: Option<u64>,
    config_map_namespace: String,
    config_map_name: String,
    rendered_revision: String,
    distributed_revision: String,
    content_digest: String,
    config_consistent: bool,
    candidate_count: usize,
    candidate_identity_count: usize,
    candidate_scores: BTreeMap<String, f64>,
    candidate_ranks: BTreeMap<String, u64>,
    candidate_queue_scores: BTreeMap<String, f64>,
}

type OverlayCandidateState = (BTreeMap<String, f64>, BTreeMap<String, u64>, BTreeMap<String, f64>);

#[derive(Clone)]
struct SimulatorControlState {
    pool: Option<String>,
    value: String,
    sha256: String,
    safe_metrics: (Option<f64>, Option<f64>, Option<f64>),
    request_id_headers_enabled: bool,
}

/// JSON contract emitted by deterministic simulator pressure/recovery hooks.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SimulatorHookReport {
    schema: String,
    phase: String,
    api_ca_verified: bool,
    observations: Vec<SimulatorPodObservation>,
}

/// Isolated ExtProc failure-closed result emitted by a task-owned hook.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExtProcFailureClosedReport {
    schema: String,
    api_ca_verified: bool,
    task_owned: bool,
    resource: String,
    request_id: String,
    request_status_code: u16,
    extproc_failure_observed: bool,
    failure_closed: bool,
    backend_request_count: u64,
}

/// Read-only proof of a topology-specific provider's effective TLS transport.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProviderTransportHookReport {
    schema: String,
    api_ca_verified: bool,
    resource: String,
    endpoint: String,
    sni: String,
    trust_source: String,
    tls_verified: bool,
    hostname_verified: bool,
    insecure_skip_verify: bool,
    configuration_revision: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EppMetricsHookReport {
    schema: String,
    phase: String,
    metrics_scraper_service_account: String,
    api_ca_verified: bool,
    values: BTreeMap<String, f64>,
}

/// Sanitized provider transport proof retained in the machine report.
#[derive(Clone, Debug, Serialize)]
struct ProviderTransportRecord {
    resource: String,
    trust_source: String,
    endpoint_matches: bool,
    sni_matches: bool,
    tls_verified: bool,
    hostname_verified: bool,
    insecure_skip_verify: bool,
    configuration_revision: String,
    observed_epoch_seconds: u64,
}

/// Hook observation of the config digest loaded by one live simulator Pod.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SimulatorPodObservation {
    control_resource: String,
    pod_name: String,
    pod_uid: String,
    container: String,
    ready: bool,
    image_id: String,
    control_sha256: String,
}

/// Verified simulator Pod identity retained in the report.
#[derive(Clone, Debug, Serialize)]
struct SimulatorPodRecord {
    phase: String,
    control_resource: String,
    namespace: String,
    pod_name: String,
    pod_uid: String,
    container: String,
    image_id: String,
    control_sha256: String,
    ready: bool,
    observed_epoch_seconds: u64,
}

/// Runtime Gateway evidence that a Grid overlay revision was consumed.
#[derive(Clone, Debug, Serialize)]
struct GatewayRevisionRecord {
    phase: String,
    revision: String,
    pod_uid: String,
    proxy_pod_uid: String,
    api_ca_verified: bool,
    route_retry_limit: u64,
    route_backend_count: usize,
    route_cluster_sha256: String,
    route_backend_service_matches: bool,
    serving_config_digest: String,
    mounted_serving_config_matches: bool,
    overlay_candidate_identities_match: bool,
    overlay_candidate_count: usize,
    retry_attempts: Option<u64>,
    retry_counter_present: bool,
    upstream_requests: u64,
    request_counter_present: bool,
    counter_source: String,
    observed_epoch_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayRevisionHookObservation {
    revision: String,
    pod_uid: String,
    proxy_pod_uid: String,
    api_ca_verified: bool,
    route_retry_limit: u64,
    route_backend_count: usize,
    route_cluster_sha256: String,
    route_backend_service_matches: bool,
    serving_config_digest: String,
    mounted_serving_config_matches: bool,
    overlay_candidate_identities_match: bool,
    overlay_candidate_count: usize,
    retry_attempts: Option<u64>,
    retry_counter_present: bool,
    upstream_requests: u64,
    request_counter_present: bool,
    counter_source: String,
}

/// Sanitized summary of a deterministic simulator ConfigMap control value.
#[derive(Clone, Debug, Serialize)]
struct SimulatorControlRecord {
    phase: String,
    observed_epoch_seconds: u64,
    resource: String,
    data_key: String,
    pool: Option<String>,
    value_bytes: usize,
    control_sha256: String,
    waiting_requests: Option<f64>,
    running_requests: Option<f64>,
    kv_cache_usage: Option<f64>,
    changed_from_baseline: Option<bool>,
    restored_to_baseline: Option<bool>,
}

#[derive(Clone, Debug, Serialize)]
struct RuntimeImageRecord {
    namespace: String,
    pod: String,
    pod_uid: String,
    container: String,
    image: String,
    image_id: String,
    ready: bool,
}

/// Cross-checked LLMInferenceService/InferencePool/EPP/simulator path for one pool.
#[derive(Clone, Debug, Serialize)]
struct ServingPoolRecord {
    pool: String,
    graph_passed: bool,
    model_resource: String,
    pool_ready: bool,
    model_reports_pool: bool,
    model_reports_epp_service: bool,
    epp_service: String,
    epp_ready_pod_uids: Vec<String>,
    epp_deployments: Vec<String>,
    epp_runtime_image_ids: Vec<String>,
    backend_service: String,
    backend_ready_pod_uids: Vec<String>,
    simulator_runtime_image_ids: Vec<String>,
    observed_epoch_seconds: u64,
}

struct ServingNamespaceSnapshot {
    pods: Value,
    endpoint_slices: Value,
    replica_sets: Value,
    deployments: Value,
}

fn simulator_receipt_logging_enabled(
    snapshot: &ServingNamespaceSnapshot,
    inspection: &ServiceTargetInspection,
) -> bool {
    if inspection.pod_uids.len() != 1 {
        return false;
    }
    let expected_uid = inspection.pod_uids.first().map(String::as_str).unwrap_or_default();
    snapshot.pods["items"].as_array().is_some_and(|pods| {
        pods.iter().any(|pod| {
            pod["metadata"]["uid"].as_str() == Some(expected_uid)
                && pod["status"]["phase"].as_str() == Some("Running")
                && pod["status"]["conditions"].as_array().is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"].as_str() == Some("Ready") && condition["status"].as_str() == Some("True")
                    })
                })
                && pod["spec"]["containers"].as_array().is_some_and(|containers| {
                    containers.iter().any(|container| {
                        container["args"]
                            .as_array()
                            .is_some_and(|args| args.iter().any(|arg| arg.as_str() == Some("--v=4")))
                    })
                })
        })
    })
}

struct ServiceTargetInspection {
    passed: bool,
    detail: String,
    pod_uids: Vec<String>,
    deployment_names: Vec<String>,
    image_ids: Vec<String>,
}

enum PoolBackendIdentityCheck {
    Incomplete(String),
    Overlap(String),
    Distinct(String),
}

/// Machine-readable qualification report.
#[derive(Debug, Serialize)]
struct Report {
    schema: &'static str,
    suite: &'static str,
    mode: Mode,
    source_mode: SourceMode,
    source_resolution_status: String,
    platform_manifests: bool,
    qualification_scope: QualificationScope,
    enrollment_qualification: String,
    capacity_baseline: String,
    under_baseline_dev_smoke: bool,
    resource_profile: ResourceProfile,
    result_label: String,
    api_tls_mode: String,
    started_epoch_seconds: u64,
    context: String,

    auth_configured: bool,
    auth_header: String,
    model: String,
    model_header: String,
    gateway_path: String,
    provider_trust_configured: bool,
    provider_transport_verified: bool,
    extproc_failure_closed_requested: bool,
    request_oracle_configured: bool,
    pool_response_oracles_configured: bool,
    signal_transition_required: bool,
    timeout_seconds: u64,
    poll_interval_seconds: u64,

    namespace: String,
    gateway_url: Option<String>,
    gateway_connect_to_configured: bool,
    provider_header: String,
    backend_header: String,
    extproc_headers: Vec<String>,
    expected_extproc_hops: usize,
    extproc_filter_resources: Vec<String>,
    required_deployments: Vec<String>,
    required_crds: Vec<String>,
    gate_resources: Vec<String>,
    grid_operator_deployment: Option<String>,
    grid_gateway_service: Option<String>,

    external_model_resource: Option<String>,

    external_provider_resource: Option<String>,

    epp_metrics_services: Vec<String>,

    httproute_resource: Option<String>,

    grid_site_resource: Option<String>,

    expected_site_san: Option<String>,

    image_pins: Vec<String>,
    runtime_image_digests: Vec<String>,
    pool_resources: Vec<String>,

    epp_metric_name: Option<String>,

    epp_queue_capacity: Option<f64>,

    grid_provider_a: Option<String>,

    grid_provider_b: Option<String>,

    signal_samples: Vec<SignalSample>,

    grid_revisions: Vec<GridRevisionRecord>,

    gateway_revisions: Vec<GatewayRevisionRecord>,

    simulator_controls: Vec<SimulatorControlRecord>,

    simulator_pods: Vec<SimulatorPodRecord>,

    runtime_images: Vec<RuntimeImageRecord>,

    pool_model_resources: Vec<String>,
    pool_backend_services: Vec<String>,
    ipp_state: IppState,
    ipp_resources: Vec<String>,
    serving_pools: Vec<ServingPoolRecord>,

    provider_transport: Option<ProviderTransportRecord>,

    source_revisions: Vec<String>,

    qualification: String,

    checks: Vec<Check>,
    probes: Vec<Probe>,
    status: String,
}

impl Report {
    /// Create a report with the immutable run metadata.
    fn new(args: &GridLlmdArgs, context: String, started_epoch_seconds: u64) -> Self {
        Self {
            schema: REPORT_SCHEMA,
            suite: "grid-llmd-extproc",
            mode: args.mode,
            source_mode: args.source_mode,
            source_resolution_status: if args.mode == Mode::Plan {
                "not-run-plan-only".to_owned()
            } else {
                "pending".to_owned()
            },
            platform_manifests: args.platform_manifests,
            qualification_scope: args.qualification_scope,
            enrollment_qualification: args.qualification_scope.enrollment_qualification().to_owned(),
            capacity_baseline: if args.resource_profile.is_reduced_dev() {
                "WAIVED_FOR_EPHEMERAL_DEV_SMOKE".to_owned()
            } else {
                "REQUIRED".to_owned()
            },
            under_baseline_dev_smoke: args.resource_profile.is_reduced_dev(),
            resource_profile: args.resource_profile,
            result_label: if args.resource_profile.is_reduced_dev() {
                "DEV-SMOKE / REDUCED-CAPACITY".to_owned()
            } else {
                "STRICT / FULL-CAPACITY".to_owned()
            },
            api_tls_mode: api_tls_mode(args)
                .map_or_else(|_| "not-validated".to_owned(), |mode| mode.as_str().to_owned()),
            started_epoch_seconds,
            context: sanitize_context(&context),
            auth_configured: request_auth_header_value(args.auth_token.as_deref(), args.no_auth, &args.auth_header)
                .is_ok_and(|header| header.is_some()),
            auth_header: args.auth_header.clone(),
            model: args.model.clone(),
            model_header: args.model_header.clone(),
            gateway_path: args.gateway_path.clone(),
            provider_trust_configured: args.provider_trust.is_some(),
            provider_transport_verified: false,
            extproc_failure_closed_requested: args.extproc_failure_closed_hook.is_some(),
            request_oracle_configured: args.extproc_request_oracle.is_some(),
            pool_response_oracles_configured: args.pool_a_response_oracle.is_some()
                && args.pool_b_response_oracle.is_some(),
            signal_transition_required: args.require_signal_transition,
            timeout_seconds: args.timeout_seconds,
            poll_interval_seconds: args.poll_interval_seconds,
            namespace: if args.namespace.is_empty() {
                "<required>".to_owned()
            } else {
                args.namespace.clone()
            },
            gateway_url: args.gateway_url.as_deref().map(sanitize_gateway_url),
            gateway_connect_to_configured: args.gateway_connect_to.is_some(),
            provider_header: args.provider_header.clone(),
            backend_header: args.backend_header.clone(),
            extproc_headers: args.extproc_headers.clone(),
            expected_extproc_hops: args.expected_extproc_hops,
            extproc_filter_resources: args.extproc_filter_resources.clone(),
            required_deployments: args.deployments.clone(),
            required_crds: args.crds.clone(),
            gate_resources: args.gate_resources.clone(),
            grid_operator_deployment: args.grid_operator_deployment.clone(),
            grid_gateway_service: args.grid_gateway_service.clone(),
            external_model_resource: args.external_model_resource.clone(),
            external_provider_resource: args.external_provider_resource.clone(),
            epp_metrics_services: args.epp_metrics_services.clone(),
            httproute_resource: args.httproute_resource.clone(),
            grid_site_resource: args.grid_site_resource.clone(),
            expected_site_san: args.expected_site_san.clone(),
            image_pins: args.image_pins.clone(),
            runtime_image_digests: args.runtime_image_digests.clone(),
            pool_resources: args.pool_resources.clone(),
            pool_model_resources: args.pool_model_resources.clone(),
            pool_backend_services: args.pool_backend_services.clone(),
            ipp_state: args.ipp_state,
            ipp_resources: args.ipp_resources.clone(),
            epp_metric_name: args.epp_metric_name.clone(),
            epp_queue_capacity: args.epp_queue_capacity,
            grid_provider_a: args.grid_provider_a.clone(),
            grid_provider_b: args.grid_provider_b.clone(),
            signal_samples: Vec::new(),
            grid_revisions: Vec::new(),
            gateway_revisions: Vec::new(),
            simulator_controls: Vec::new(),
            simulator_pods: Vec::new(),
            runtime_images: Vec::new(),
            serving_pools: Vec::new(),
            provider_transport: None,
            source_revisions: args.source_revisions.clone(),
            qualification: "not-run".to_owned(),
            checks: Vec::new(),
            probes: Vec::new(),
            status: "planned".to_owned(),
        }
    }

    /// Whether every recorded check and probe passed.
    fn passed(&self) -> bool {
        self.checks.iter().all(|check| {
            matches!(check.status.as_str(), "pass" | "warn")
                || (self.qualification_scope == QualificationScope::RoutingSmoke
                    && check.name == "enrollment/qualification"
                    && check.status == "skipped")
        }) && self.probes.iter().all(|probe| probe.passed)
    }
}

/// Captured process output used for diagnostics and JSON parsing.
struct CommandCapture {
    output: Output,
}

impl CommandCapture {
    /// Return the command's stdout as UTF-8-lossy text.
    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.output.stdout).into_owned()
    }

    /// Return the command's stderr as UTF-8-lossy text.
    fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.output.stderr).into_owned()
    }

    /// Return the process exit status as a human-readable value.
    fn status(&self) -> String {
        self.output
            .status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string())
    }

    /// Whether the process exited successfully.
    fn success(&self) -> bool {
        self.output.status.success()
    }
}

/// Run the selected qualification suite and exit non-zero on a failed run.
pub(crate) fn run(args: Args) {
    let result = match args.suite {
        Suite::GridLlmd(mut grid_args) => {
            grid_args.auth_token = std::env::var("GRID_LLMD_AUTH_TOKEN")
                .ok()
                .filter(|token| !token.is_empty());
            run_grid_llmd(&grid_args)
        },
        Suite::GridMaas(maas_args) => run_grid_maas(&maas_args),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn resolve_clean_sources(
    scenario: &str,
    source_mode: SourceMode,
    platform_manifests: bool,
    evidence_dir: &Path,
) -> Result<TempDir, String> {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "could not resolve ExtProc repository root".to_owned())?;
    let script = repo_root.join("tests/shared/resolve_sources.py");
    let source_root = tempfile::Builder::new()
        .prefix("praxis-extproc-e2e-sources-")
        .tempdir()
        .map_err(|error| format!("create temporary source root: {error}"))?;
    let worktrees = source_root.path().join("worktrees");
    fs::create_dir(&worktrees).map_err(|error| format!("create source worktree root: {error}"))?;
    let output = Command::new("python3")
        .arg(script)
        .arg("--scenario")
        .arg(scenario)
        .arg("--source-mode")
        .arg(source_mode.as_str())
        .arg("--platform-manifests")
        .arg(platform_manifests.to_string())
        .arg("--caller-repo-root")
        .arg(repo_root)
        .arg("--evidence-dir")
        .arg(evidence_dir)
        .arg("--worktree-root")
        .arg(&worktrees)
        .output()
        .map_err(|error| format!("run clean source resolver: {error}"))?;
    if !output.status.success() {
        return Err(format!("clean source resolution failed (exit {})", output.status));
    }
    Ok(source_root)
}

fn record_source_locks(evidence_dir: &Path, report: &mut Report) -> Result<(), String> {
    let path = evidence_dir.join("source-locks.json");
    let document: Value =
        serde_json::from_slice(&fs::read(&path).map_err(|error| format!("read source locks: {error}"))?)
            .map_err(|error| format!("decode source locks: {error}"))?;
    if document["source_mode"].as_str() != Some(report.source_mode.as_str()) {
        return Err("resolved source mode differs from the selected SOURCE_MODE".to_owned());
    }
    let components = document["components"]
        .as_object()
        .ok_or_else(|| "source locks have no component map".to_owned())?;
    for (component, record) in components {
        let sha = record["resolved_sha"]
            .as_str()
            .ok_or_else(|| format!("resolved SHA missing for {component}"))?;
        if !is_git_commit_sha(sha) {
            return Err(format!("resolved SHA malformed for {component}"));
        }
        report.source_revisions.push(format!("{component}={sha}"));
    }
    report.source_resolution_status = "resolved-clean-detached-worktrees".to_owned();
    Ok(())
}

fn write_source_resolution_blocker(evidence_dir: &Path, report: &mut Report, error: &str) -> Result<(), String> {
    report.checks.push(Check {
        name: "source-resolution".to_owned(),
        status: "blocked".to_owned(),
        detail: error.to_owned(),
        observed_epoch_seconds: epoch_seconds(),
    });
    report.status = "blocked".to_owned();
    write_report(evidence_dir, report)?;
    write_summary(evidence_dir, report)?;
    write_bug_capture(evidence_dir, report)
}

fn write_grid_maas_plan(evidence_dir: &Path, args: &GridMaasArgs, status: &str, detail: &str) -> Result<(), String> {
    let result = serde_json::json!({
        "schema": "praxis-extproc.grid-maas-run/v1",
        "scenario": "grid-maas",
        "mode": args.mode,
        "source_mode": args.source_mode,
        "resource_profile": args.resource_profile,
        "platform_manifests": args.platform_manifests,
        "result": status,
        "path": ["MaaS/AIGC Gateway", "ExternalModel ExtProc", "ExternalProvider", "Grid Gateway", "Grid ExtProc", "EPP", "LLMInferenceService/InferencePool", "llm-d-sim"],
        "detail": detail,
        "inference_traffic_sent": false
    });
    write_json_private(&evidence_dir.join("grid-maas-run.json"), &result)?;
    let summary = format!(
        "# Grid/MaaS E2E\n\n- Status: `{status}`\n- Source mode: `{}`\n- Resource profile: `{}`\n- Result label: `{}`\n- Inference traffic sent: `no`\n\n{detail}\n",
        args.source_mode.as_str(),
        args.resource_profile.as_str(),
        if args.resource_profile.is_reduced_dev() {
            "DEV-SMOKE / REDUCED-CAPACITY"
        } else {
            "STRICT / FULL-CAPACITY"
        },
    );
    write_private(&evidence_dir.join("FINAL-RESULT.md"), summary.as_bytes())
}

fn run_grid_maas(args: &GridMaasArgs) -> Result<(), String> {
    if !resource_profile_authorized(args.resource_profile, args.cluster_env.as_deref()) {
        return Err("RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev".to_owned());
    }
    let started = epoch_seconds();
    let evidence_dir = args
        .evidence_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("target/e2e-grid-maas/{started}")));
    if evidence_dir.exists() {
        return Err(format!("evidence directory already exists: {}", evidence_dir.display()));
    }
    fs::create_dir_all(&evidence_dir).map_err(|error| format!("create evidence directory: {error}"))?;
    set_private_permissions(&evidence_dir, 0o700)?;
    if args.mode == GridMaasMode::Plan {
        return write_grid_maas_plan(
            &evidence_dir,
            args,
            "BLOCKED",
            "The optional lane cannot proceed until resolved AI Gateway and MaaS sources prove the Grid Gateway ExternalProvider and TLS backend contract.",
        );
    }
    let worktrees = resolve_clean_sources("grid-maas", args.source_mode, args.platform_manifests, &evidence_dir)?;
    let checker = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("tests/grid-maas-e2e/check-compatibility.py");
    let check = Command::new("python3")
        .arg(checker)
        .arg("--source-locks")
        .arg(evidence_dir.join("source-locks.json"))
        .arg("--evidence-dir")
        .arg(&evidence_dir)
        .output()
        .map_err(|error| format!("run Grid/MaaS compatibility checker: {error}"))?;
    let status = if check.status.success() { "NOT RUN" } else { "BLOCKED" };
    let detail = if check.status.success() {
        "Source compatibility passed, but the live Grid/MaaS scenario remains NOT RUN until deployment and traffic assertions complete.".to_owned()
    } else {
        "Source compatibility gate did not prove a Grid Gateway ExternalProvider backend; cluster work is blocked."
            .to_owned()
    };
    write_grid_maas_plan(evidence_dir.as_path(), args, status, &detail)?;
    drop(worktrees);
    if check.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Grid/MaaS compatibility is BLOCKED; evidence: {}",
            evidence_dir.display()
        ))
    }
}

fn run_grid_llmd(args: &GridLlmdArgs) -> Result<(), String> {
    if !resource_profile_authorized(args.resource_profile, args.cluster_env.as_deref()) {
        return Err("RESOURCE_PROFILE=reduced-dev requires CLUSTER_ENV=ephemeral-dev".to_owned());
    }
    let started_epoch_seconds = epoch_seconds();
    let evidence_dir = resolve_evidence_dir(args.evidence_dir.as_deref(), started_epoch_seconds);
    if evidence_dir.exists() {
        return Err(format!(
            "evidence directory already exists; choose a new path: {}",
            evidence_dir.display()
        ));
    }
    fs::create_dir_all(&evidence_dir).map_err(|error| format!("create evidence directory: {error}"))?;
    set_private_permissions(&evidence_dir, 0o700)?;
    let context = args.context.clone().unwrap_or_else(|| "<required>".to_owned());
    let _redactions = OUTPUT_REDACTIONS.get_or_init(|| OutputRedactions {
        auth_token: args.auth_token.clone(),
        gateway_url: args.gateway_url.clone(),
        auth_header: args.auth_header.clone(),
        prompt: args.prompt.clone(),
    });
    let mut report = Report::new(args, context, started_epoch_seconds);
    let _source_worktrees = if args.mode == Mode::Plan {
        None
    } else {
        let worktrees =
            match resolve_clean_sources("grid-llmd", args.source_mode, args.platform_manifests, &evidence_dir) {
                Ok(worktrees) => worktrees,
                Err(error) => {
                    write_source_resolution_blocker(&evidence_dir, &mut report, &error)?;
                    return Err(format!(
                        "source resolution blocked; evidence: {}",
                        evidence_dir.display()
                    ));
                },
            };
        if let Err(error) = record_source_locks(&evidence_dir, &mut report) {
            write_source_resolution_blocker(&evidence_dir, &mut report, &error)?;
            return Err(format!(
                "source lock validation blocked; evidence: {}",
                evidence_dir.display()
            ));
        }
        Some(worktrees)
    };

    if args.mode != Mode::Plan {
        let tls_mode = api_tls_mode(args);
        if args.context.as_deref().is_none_or(str::is_empty) || args.namespace.is_empty() || tls_mode.is_err() {
            report.checks.push(Check {
                name: "secure-cluster-inputs".to_owned(),
                status: "blocked".to_owned(),
                detail: format!(
                    "non-plan modes require KUBE_CONTEXT and GRID_LLMD_NAMESPACE; API TLS setup: {}; resource profile: {}",
                    tls_mode.err().unwrap_or_else(|| "verified or explicitly opted in".to_owned()),
                    args.resource_profile.as_str()
                ),
                observed_epoch_seconds: epoch_seconds(),
            });
            report.status = "blocked".to_owned();
            write_report(&evidence_dir, &report)?;
            write_summary(&evidence_dir, &report)?;
            write_bug_capture(&evidence_dir, &report)?;
            return Err(format!(
                "secure cluster inputs are incomplete; evidence: {}",
                evidence_dir.display()
            ));
        }
        let command_config = KubeCommandConfig {
            api_ca_file: args.api_ca_file.clone(),
            kubeconfig: args.kubeconfig.clone(),
            allow_insecure_dev_api: matches!(tls_mode, Ok(ApiTlsMode::ExplicitEphemeralDevInsecure)),
        };
        if KUBE_COMMAND_CONFIG.set(command_config).is_err() {
            return Err("Kubernetes command configuration was already initialized".to_owned());
        }
    }

    let result = match args.mode {
        Mode::Plan => plan(args, &mut report, &evidence_dir),
        Mode::Preflight => preflight(args, &mut report, &evidence_dir),
        Mode::Run => run_probes(args, &mut report, &evidence_dir),
        Mode::Collect => collect(args, &mut report, &evidence_dir),
    };

    if let Err(error) = result {
        if args.mode != Mode::Plan && KUBE_COMMAND_CONFIG.get().is_some() {
            let context = args.context.clone().unwrap_or_default();
            if let Err(snapshot_error) = collect_snapshots(
                &mut report,
                &evidence_dir,
                &context,
                &args.namespace,
                &args.deployments,
                args.grid_operator_deployment.as_deref(),
            ) {
                report.checks.push(Check {
                    name: "failure-diagnostics".to_owned(),
                    status: "warn".to_owned(),
                    detail: format!("post-failure diagnostics collection also failed: {snapshot_error}"),
                    observed_epoch_seconds: epoch_seconds(),
                });
            }
        }
        report.checks.push(Check {
            name: "runner".to_owned(),
            status: if report.checks.iter().any(|check| check.status == "blocked") {
                "blocked".to_owned()
            } else {
                "fail".to_owned()
            },
            detail: error.clone(),
            observed_epoch_seconds: epoch_seconds(),
        });
        report.status = if report.checks.iter().any(|check| check.status == "blocked") {
            "blocked".to_owned()
        } else {
            "fail".to_owned()
        };
        write_report(&evidence_dir, &report)?;
        write_summary(&evidence_dir, &report)?;
        write_bug_capture(&evidence_dir, &report)?;
        return Err(format!("{}; evidence: {}", error, evidence_dir.display()));
    }

    report.status = match args.mode {
        Mode::Plan | Mode::Collect => "not-run".to_owned(),
        Mode::Run if !args.require_signal_transition => "not-run".to_owned(),
        Mode::Preflight | Mode::Run => status_from_checks(&report),
    };
    let qualification_detail = if args.mode == Mode::Run
        && args.resource_profile.is_reduced_dev()
        && report.status == "partial"
        && args.qualification_scope == QualificationScope::RoutingSmoke
    {
        "routing-signal-smoke-enrollment-unqualified"
    } else if args.mode == Mode::Run && args.resource_profile.is_reduced_dev() && report.status == "pass" {
        "complete-signal-qualification"
    } else if args.mode == Mode::Run
        && !args.require_signal_transition
        && args.qualification_scope == QualificationScope::RoutingSmoke
    {
        "routing-smoke-baseline-enrollment-unqualified"
    } else if args.mode == Mode::Run && !args.require_signal_transition {
        "baseline-smoke"
    } else if args.mode == Mode::Run
        && report.status == "partial"
        && args.qualification_scope == QualificationScope::RoutingSmoke
    {
        "routing-signal-smoke-enrollment-unqualified"
    } else if args.mode == Mode::Run && report.status == "pass" {
        "complete-signal-qualification"
    } else {
        "not-complete"
    };
    report.qualification = if args.resource_profile.is_reduced_dev() {
        format!("{}: {qualification_detail}", report.result_label)
    } else {
        qualification_detail.to_owned()
    };
    write_report(&evidence_dir, &report)?;
    write_summary(&evidence_dir, &report)?;
    if !matches!(report.status.as_str(), "pass" | "partial") && args.mode != Mode::Plan {
        write_bug_capture(&evidence_dir, &report)?;
    }
    println!("Grid llm-d ExtProc E2E {}: {}", report.status, evidence_dir.display());
    if matches!(report.status.as_str(), "fail" | "blocked") {
        return Err(format!("qualification failed; evidence: {}", evidence_dir.display()));
    }
    Ok(())
}

fn plan(args: &GridLlmdArgs, report: &mut Report, evidence_dir: &Path) -> Result<(), String> {
    report.checks.push(Check {
        name: "source-selection".to_owned(),
        status: "pass".to_owned(),
        detail: format!(
            "SOURCE_MODE={} requested; source resolution is performed in non-plan runs before cluster access; platform manifests={}",
            args.source_mode.as_str(),
            args.platform_manifests
        ),
        observed_epoch_seconds: epoch_seconds(),
    });
    report.checks.push(Check {
        name: "contract".to_owned(),
        status: "pass".to_owned(),
        detail: "plan mode makes no Kubernetes or inference calls".to_owned(),
        observed_epoch_seconds: epoch_seconds(),
    });
    let text = format!(
        "# Grid llm-d ExtProc qualification plan\n\n\
        mode: `{}`\nscope: `{}`\nEnrollment qualification: `{}`\nResource profile: `{}`\nResult label: `{}`\nCapacity baseline: `{}`\nAPI TLS mode: `{}`\ncontext: `{}`\nnamespace: `{}`\n\
        gateway URL: `{}`\nGrid Gateway name: `{}`\nGrid operator Deployment: `{}`\nGrid Gateway Service: `{}`\n\
        GridNetwork: `{}`\nMaaS readiness gate: `{}`\n\
        Enrollment gate input: `{}`\nHTTPRoute: `{}`\nLLMInferenceService/InferencePool: `{}`\n\
        pools: `{}`\nPool model resources: `{}`\nPool backend services: `{}`\nIPP state/resources: `{}` / `{}`\n\
        EPP metric/queue capacity: `{}` / `{}` via `{}`\nGrid provider A/B: `{}` / `{}`\n\
        source revisions: `{}`\nimage pins: `{}`\n\
        required assertions:\n\
        - MaaS/RHOAI platform readiness (ExternalModel translation is out of scope)\n\
        - GridNetwork overlay distribution and serving Gateway consumed revision\n\
        - EPP metric transition and deterministic simulator control/recovery\n\
        - A-to-B-to-A provider attribution and ExtProc-owned marker(s)\n\
        Plan mode makes no Kubernetes, Gateway, or inference calls.\n",
        args.mode.as_str(),
        args.qualification_scope.as_str(),
        args.qualification_scope.enrollment_qualification(),
        args.resource_profile.as_str(),
        report.result_label,
        report.capacity_baseline,
        api_tls_mode(args).map_or("not-validated", ApiTlsMode::as_str),
        report.context,
        report.namespace,
        report.gateway_url.as_deref().unwrap_or("<required by run mode>"),
        args.grid_gateway_name
            .as_deref()
            .unwrap_or("<required for exact Grid overlay binding>"),
        args.grid_operator_deployment
            .as_deref()
            .unwrap_or("<required downstream ODH operator Deployment>"),
        args.grid_gateway_service
            .as_deref()
            .unwrap_or("<required direct Grid Gateway Service>"),
        args.gate_resources
            .iter()
            .find(|v| v.starts_with("grid="))
            .map_or("<required>", String::as_str),
        args.gate_resources
            .iter()
            .find(|v| v.starts_with("maas="))
            .map_or("<required>", String::as_str),
        args.gate_resources
            .iter()
            .find(|v| v.starts_with("enrollment="))
            .map_or("<not supplied>", String::as_str),
        args.httproute_resource
            .as_deref()
            .unwrap_or("<required task-owned direct Grid Gateway HTTPRoute>"),
        args.gate_resources
            .iter()
            .find(|v| v.starts_with("llminferenceservice="))
            .map_or("<required>", String::as_str),
        args.pool_resources.join(", "),
        args.pool_model_resources.join(", "),
        args.pool_backend_services.join(", "),
        args.ipp_state.as_str(),
        args.ipp_resources.join(", "),
        args.epp_metric_name.as_deref().unwrap_or("<required for signal mode>"),
        args.epp_queue_capacity
            .map_or_else(|| "<required for signal mode>".to_owned(), |value| value.to_string()),
        args.epp_metrics_services.join(", "),
        args.grid_provider_a.as_deref().unwrap_or("<required for signal mode>"),
        args.grid_provider_b.as_deref().unwrap_or("<required for signal mode>"),
        args.source_revisions.join(", "),
        args.image_pins.join(", "),
    );
    println!("{text}");
    write_private(&evidence_dir.join("PLAN.md"), text.as_bytes())
}

fn preflight(args: &GridLlmdArgs, report: &mut Report, evidence_dir: &Path) -> Result<(), String> {
    let context = args.context.clone().unwrap_or_default();
    check_kubectl(report, evidence_dir, &context)?;
    check_openshift_prerequisites(report, evidence_dir, &context)?;
    check_cluster_capacity(args, report, evidence_dir, &context)?;
    check_integration_gates(args, report, evidence_dir, &context)?;
    check_grid_gateway_route(args, report, evidence_dir, &context)?;
    if args.qualification_scope == QualificationScope::RoutingSmoke {
        report.checks.push(Check {
            name: "enrollment/qualification".to_owned(),
            status: "skipped".to_owned(),
            detail: "routing-smoke scope does not qualify Enrollment; cluster etcd encryption prerequisite is unverified and was explicitly waived only for this disposable-dev routing smoke".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
        });
    } else {
        check_enrollment_contract(args, report, evidence_dir, &context)?;
    }
    check_external_model_contract(args, report, evidence_dir, &context)?;
    check_extproc_filter(args, report, evidence_dir, &context)?;
    check_grid_operator_contract(args, report, evidence_dir, &context)?;
    check_grid_operator_deployment(args, report, evidence_dir, &context)?;
    check_pool_resources(args, report, evidence_dir, &context)?;
    check_serving_graph(args, report, evidence_dir, &context)?;
    check_pool_route_contract(args, report, evidence_dir, &context)?;
    check_simulator_control_pool_coverage(args, report)?;
    for crd in &args.crds {
        check_resource(
            report,
            evidence_dir,
            &context,
            None,
            "crd",
            crd,
            &format!("CRD {crd}"),
            None,
        )?;
    }
    for deployment in &args.deployments {
        let deployment_ref = parse_deployment_ref(deployment, &args.namespace)?;
        check_deployment(
            report,
            evidence_dir,
            &context,
            &deployment_ref.namespace,
            &deployment_ref.name,
        )?;
    }
    collect_snapshots(
        report,
        evidence_dir,
        &context,
        &args.namespace,
        &args.deployments,
        args.grid_operator_deployment.as_deref(),
    )?;
    check_ipp_inventory(args, report, evidence_dir, &context)?;
    check_source_and_image_pins(args, report);
    Ok(())
}

fn run_probes(args: &GridLlmdArgs, report: &mut Report, evidence_dir: &Path) -> Result<(), String> {
    validate_run_inputs(args, report);
    preflight(args, report, evidence_dir)?;
    if report
        .checks
        .iter()
        .any(|check| matches!(check.status.as_str(), "blocked" | "fail"))
    {
        report.checks.push(Check {
            name: "inference-traffic".to_owned(),
            status: "not-run".to_owned(),
            detail: "preflight did not pass; no inference request or pressure hook was sent".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    }
    if report
        .checks
        .iter()
        .any(|check| check.name == "run-inputs" && check.status != "pass")
    {
        report.checks.push(Check {
            name: "inference-traffic".to_owned(),
            status: "not-run".to_owned(),
            detail: "required run inputs are incomplete; no inference request or pressure hook was sent".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    }
    if !args.require_signal_transition {
        let baseline_grid = observe_grid_revision(args, report, evidence_dir, "baseline")?;
        if !grid_revision_is_distributed(&baseline_grid) {
            add_blocked_check(
                report,
                "grid/baseline-revision",
                "Grid overlay is not fully distributed at baseline",
            );
            return Ok(());
        }
        wait_for_gateway_revision(args, report, evidence_dir, &baseline_grid, "baseline")?;
        if !check_baseline_score_overlay_contract(args, report, evidence_dir, &baseline_grid, "baseline")? {
            add_blocked_check(
                report,
                "grid/baseline-score-overlay-contract",
                "current authenticated EPP samples do not match the exact Distributed revision consumed by the Gateway",
            );
            return Ok(());
        }
        let baseline = run_probe(args, report, evidence_dir, "baseline")?;
        verify_pool_attribution(args, report, &baseline, &baseline, "baseline");
        report.checks.push(Check {
            name: "signal-transition".to_owned(),
            status: if baseline.passed { "not-run" } else { "blocked" }.to_owned(),
            detail: if baseline.passed {
                "baseline-only smoke requested; signal qualification requires --require-signal-transition".to_owned()
            } else {
                "baseline request failed its route/ExtProc assertion; no pressure was applied".to_owned()
            },
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    }

    let baseline_controls = read_simulator_controls(args, report, evidence_dir, "baseline", None)?;
    let simulator_request_id_configured = baseline_controls.len() == args.simulator_control_resources.len()
        && baseline_controls
            .values()
            .all(|control| control.request_id_headers_enabled);
    record_boolean_check(
        report,
        "simulator/request-id-receipt-config",
        simulator_request_id_configured,
        "both task simulator controls enable X-Request-Id echo; control contents omitted".to_owned(),
    );
    if !simulator_request_id_configured {
        add_blocked_check(
            report,
            "simulator/request-id-receipt-config",
            "both simulators must enable X-Request-Id echo before inference traffic",
        );
        return Ok(());
    }
    let baseline_grid = observe_grid_revision(args, report, evidence_dir, "baseline")?;
    if !grid_revision_is_distributed(&baseline_grid) {
        add_blocked_check(
            report,
            "grid/baseline-revision",
            "Grid overlay is not fully distributed at baseline",
        );
        return Ok(());
    }
    if baseline_controls.len() != args.simulator_control_resources.len() {
        add_blocked_check(
            report,
            "signal/baseline-inputs",
            "baseline simulator controls were incomplete; pressure was not applied",
        );
        return Ok(());
    }
    wait_for_gateway_revision(args, report, evidence_dir, &baseline_grid, "baseline")?;
    if !check_baseline_score_overlay_contract(args, report, evidence_dir, &baseline_grid, "baseline")? {
        add_blocked_check(
            report,
            "grid/baseline-score-overlay-contract",
            "current authenticated EPP samples do not match the exact Distributed revision consumed by the Gateway",
        );
        return Ok(());
    }
    let baseline_metrics = report
        .signal_samples
        .iter()
        .rev()
        .find(|sample| sample.phase == "baseline")
        .map(|sample| sample.values.clone())
        .unwrap_or_default();
    let baseline = run_probe(args, report, evidence_dir, "baseline")?;
    if !baseline.passed {
        report.checks.push(Check {
            name: "signal-transition".to_owned(),
            status: "blocked".to_owned(),
            detail: "baseline request failed its route/ExtProc assertion; no pressure was applied".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    }
    let Some(pressure_hook) = args.pressure_hook.as_deref() else {
        return Err("--require-signal-transition requires --pressure-hook".to_owned());
    };
    let Some(recovery_hook) = args.recovery_hook.as_deref() else {
        return Err("--require-signal-transition requires --recovery-hook".to_owned());
    };
    let pressure_result = apply_pressure_and_qualify(
        args,
        report,
        evidence_dir,
        pressure_hook,
        &baseline,
        &baseline_controls,
        &baseline_metrics,
        &baseline_grid,
    );
    // The pressure hook may have changed a ConfigMap before it timed out or
    // returned an error. Always invoke recovery after launching it.
    let recovery_result = recover_pressure_and_qualify(
        args,
        report,
        evidence_dir,
        recovery_hook,
        &baseline,
        &baseline_controls,
        &baseline_metrics,
        &baseline_grid,
    );
    let failure_closed_result = if recovery_result.is_ok() {
        run_extproc_failure_closed_test(args, report, evidence_dir)
    } else {
        add_blocked_check(
            report,
            "extproc/failure-closed",
            "failure-closed probe was skipped because simulator/Grid recovery did not pass",
        );
        Err("failure-closed probe skipped because recovery failed".to_owned())
    };
    match (pressure_result, recovery_result, failure_closed_result) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(pressure), Ok(()), Ok(())) => Err(pressure),
        (Err(pressure), Ok(()), Err(failure_closed)) => Err(format!("{pressure}; {failure_closed}")),
        (Ok(()), Err(recovery), _) => Err(format!("pressure was qualified but recovery failed: {recovery}")),
        (Err(pressure), Err(recovery), _) => Err(format!("{pressure}; recovery also failed: {recovery}")),
        (Ok(()), Ok(()), Err(failure_closed)) => {
            Err(format!("signal transition and recovery passed; {failure_closed}"))
        },
    }
}

fn run_extproc_failure_closed_test(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
) -> Result<(), String> {
    let Some(hook) = args.extproc_failure_closed_hook.as_deref() else {
        add_blocked_check(
            report,
            "extproc/failure-closed",
            "--require-signal-transition requires a task-owned failure-closed hook",
        );
        return Err("--extproc-failure-closed-hook is required for a complete run".to_owned());
    };
    let stdout = run_hook(args, report, evidence_dir, hook, "failure-closed", None)?;
    let result: ExtProcFailureClosedReport = serde_json::from_str(&stdout)
        .map_err(|error| format!("failure-closed hook did not emit the required JSON report: {error}"))?;
    let resource = parse_resource_ref(&result.resource, &args.namespace)?;
    let passed = result.schema == "praxis-extproc.grid-llmd-failure-closed/v1"
        && api_access_acceptable(args, result.api_ca_verified)
        && result.task_owned
        && resource.namespace == args.namespace
        && !result.request_id.is_empty()
        && result.request_status_code >= 500
        && result.extproc_failure_observed
        && result.failure_closed
        && result.backend_request_count == 0;
    record_boolean_check(
        report,
        "extproc/failure-closed",
        passed,
        format!(
            "task-owned resource={}/{}/{}, status={}, ExtProc failure observed={}, fail-closed={}, backend receipts={}, hook API CA verified={}, runner API mode={}",
            resource.namespace,
            resource.kind,
            resource.name,
            result.request_status_code,
            result.extproc_failure_observed,
            result.failure_closed,
            result.backend_request_count,
            result.api_ca_verified,
            report.api_tls_mode
        ),
    );
    if passed {
        let safe = serde_json::json!({
            "schema": result.schema,
            "apiCaVerified": result.api_ca_verified,
            "taskOwned": result.task_owned,
            "resource": format!("{}/{}/{}", resource.namespace, resource.kind, resource.name),
            "requestId": result.request_id,
            "requestStatusCode": result.request_status_code,
            "extprocFailureObserved": result.extproc_failure_observed,
            "failureClosed": result.failure_closed,
            "backendRequestCount": result.backend_request_count,
        });
        write_json_private(&evidence_dir.join("extproc-failure-closed.json"), &safe)?;
        Ok(())
    } else {
        Err("ExtProc failure-closed hook did not prove an isolated fail-closed response".to_owned())
    }
}

fn apply_pressure_and_qualify(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    pressure_hook: &Path,
    baseline: &Probe,
    baseline_controls: &BTreeMap<String, SimulatorControlState>,
    baseline_metrics: &BTreeMap<String, f64>,
    baseline_grid: &GridRevisionRecord,
) -> Result<(), String> {
    let pressure_pool = baseline.expected_pool.as_str();
    let hook_stdout = run_hook(
        args,
        report,
        evidence_dir,
        pressure_hook,
        "pressure",
        Some(pressure_pool),
    )?;
    let controls = read_simulator_controls(args, report, evidence_dir, "pressure", Some(baseline_controls))?;
    verify_simulator_hook_report(args, report, evidence_dir, "pressure", &hook_stdout, &controls)?;
    verify_simulator_pressure(report, baseline_controls, &controls, pressure_pool);
    let metrics = wait_for_metric_change(args, report, evidence_dir, baseline_metrics, "pressure", pressure_pool)?;
    let grid = wait_for_grid_revision_change(args, report, evidence_dir, baseline_grid, "pressure")?;
    if !grid_revision_is_distributed(&grid) {
        add_blocked_check(
            report,
            "grid/pressure-revision",
            "Grid did not distribute the pressure overlay revision",
        );
        return Err("Grid pressure overlay was not fully distributed".to_owned());
    }
    verify_grid_metric_signal(args, report, &metrics, baseline_metrics, &grid, pressure_pool);
    wait_for_gateway_revision(args, report, evidence_dir, &grid, "pressure")?;
    let pressure = run_probe(args, report, evidence_dir, "pressure")?;
    if !pressure.passed {
        return Err("pressure request failed its route, ExtProc, or no-retry attribution checks".to_owned());
    }
    verify_pool_attribution(args, report, baseline, &pressure, "pressure");
    Ok(())
}

fn recover_pressure_and_qualify(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    recovery_hook: &Path,
    baseline: &Probe,
    baseline_controls: &BTreeMap<String, SimulatorControlState>,
    baseline_metrics: &BTreeMap<String, f64>,
    baseline_grid: &GridRevisionRecord,
) -> Result<(), String> {
    let hook_stdout = run_hook(
        args,
        report,
        evidence_dir,
        recovery_hook,
        "recovery",
        Some(&baseline.expected_pool),
    )?;
    let controls = read_simulator_controls(args, report, evidence_dir, "recovery", Some(baseline_controls))?;
    verify_simulator_hook_report(args, report, evidence_dir, "recovery", &hook_stdout, &controls)?;
    verify_simulator_recovery(report, baseline_controls, &controls);
    let metrics = wait_for_metric_recovery(
        args,
        report,
        evidence_dir,
        baseline_metrics,
        "recovery",
        &baseline.expected_pool,
    )?;
    let grid = wait_for_grid_revision_recovery(args, report, evidence_dir, baseline_grid, "recovery")?;
    if !grid_revision_is_distributed(&grid) {
        add_blocked_check(
            report,
            "grid/recovery-revision",
            "Grid did not distribute the recovered overlay revision",
        );
        return Err("Grid recovery overlay was not fully distributed".to_owned());
    }
    verify_metric_recovery(report, baseline_metrics, &metrics);
    let recovery_score_contract = verify_grid_queue_score_contract(
        args,
        report,
        &metrics,
        grid.candidate_count,
        grid.candidate_identity_count,
        &grid.candidate_scores,
        &grid.candidate_ranks,
        &grid.candidate_queue_scores,
        "recovery",
    );
    if !recovery_score_contract {
        return Err("Grid recovery queue scores do not match recovered EPP metrics".to_owned());
    }
    wait_for_gateway_revision(args, report, evidence_dir, &grid, "recovery")?;
    let recovery = run_probe(args, report, evidence_dir, "recovery")?;
    if !recovery.passed {
        return Err("recovery request failed its route, ExtProc, or no-retry attribution checks".to_owned());
    }
    verify_pool_attribution(args, report, baseline, &recovery, "recovery");
    record_boolean_check(
        report,
        "grid/revision-restored",
        grid.distributed_revision == baseline_grid.distributed_revision,
        format!(
            "baseline={} recovery={}",
            baseline_grid.distributed_revision, grid.distributed_revision
        ),
    );
    Ok(())
}

fn request_auth_header_value(
    auth_token: Option<&str>,
    no_auth: bool,
    auth_header: &str,
) -> Result<Option<String>, String> {
    let token = auth_token.filter(|value| !value.is_empty());
    if no_auth {
        return if token.is_some() {
            Err("--no-auth cannot be combined with GRID_LLMD_AUTH_TOKEN".to_owned())
        } else {
            Ok(None)
        };
    }
    let token = token.ok_or_else(|| "GRID_LLMD_AUTH_TOKEN is required unless --no-auth is selected".to_owned())?;
    if token.contains(['\r', '\n']) || !valid_header_name(auth_header) {
        return Err("authentication header is invalid".to_owned());
    }
    Ok(Some(if auth_header.eq_ignore_ascii_case("authorization") {
        format!("Bearer {token}")
    } else {
        token.to_owned()
    }))
}

fn validate_run_inputs(args: &GridLlmdArgs, report: &mut Report) {
    let mut missing = Vec::new();
    if args.gateway_url.as_deref().is_none_or(str::is_empty) {
        missing.push("--gateway-url");
    }
    if args.gateway_connect_to.as_deref().is_some_and(|mapping| {
        !args
            .gateway_url
            .as_deref()
            .is_some_and(|url| gateway_connect_to_matches(url, mapping))
    }) {
        missing.push("--gateway-connect-to matching the HTTPS URL host and port");
    }
    if args.httproute_resource.is_none() {
        missing.push("--httproute-resource for the direct Grid Gateway route");
    }
    if args.grid_gateway_service.is_none() {
        missing.push("--grid-gateway-service");
    }
    if args.grid_operator_deployment.is_none() {
        missing.push("--grid-operator-deployment for the downstream ODH operator");
    }
    if args.model.is_empty() {
        missing.push("--model");
    }
    if request_model_header(&args.model_header, &args.model).is_err() {
        missing.push("--model-header with a valid header name and model value");
    }
    if configured_pool_attribution(
        args.pool_a_provider_attribution.as_deref(),
        args.pool_a_backend_attribution.as_deref(),
        args.pool_a_attribution.as_deref(),
    )
    .is_none()
    {
        missing.push("--pool-a-provider-attribution and --pool-a-backend-attribution for exact baseline attribution");
    }
    if args.pool_route_contract_hook.is_none() {
        missing.push(
            "--pool-route-contract-hook to assert mounted Praxis and pool HTTPRoute compatibility before traffic",
        );
    }
    if request_auth_header_value(args.auth_token.as_deref(), args.no_auth, &args.auth_header).is_err() {
        missing.push("a valid GRID_LLMD_AUTH_TOKEN or explicit --no-auth selection");
    }
    if args.extproc_headers.is_empty() && args.extproc_body_oracle.is_none() {
        missing.push("--extproc-header or --extproc-body-oracle");
    }
    if args
        .extproc_body_oracle
        .as_deref()
        .is_some_and(|header| !valid_header_name(header))
    {
        missing.push("--extproc-body-oracle as a valid response header name");
    }
    validate_transition_request_inputs(args, &mut missing);
    if args.extproc_filter_resources.len() < args.expected_extproc_hops {
        missing.push("one --extproc-filter-resource per configured ExtProc hop");
    }
    let marker_names = args
        .extproc_headers
        .iter()
        .filter_map(|expectation| parse_extproc_header_expectation(expectation))
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let unique_marker_count = marker_names.iter().collect::<std::collections::BTreeSet<_>>().len();
    if marker_names.len() != args.extproc_headers.len() {
        missing.push("--extproc-header values as valid header names or Header=ExpectedValue assertions");
    }
    let body_oracle_covers_single_hop =
        args.expected_extproc_hops == 1 && args.extproc_headers.is_empty() && args.extproc_body_oracle.is_some();
    if args.expected_extproc_hops == 0
        || (unique_marker_count < args.expected_extproc_hops && !body_oracle_covers_single_hop)
    {
        missing.push("one distinct --extproc-header per configured ExtProc hop");
    }
    if args.source_revisions.is_empty() {
        missing.push("--source-revision inputs");
    }
    if args.image_pins.is_empty() {
        missing.push("--image-pin inputs");
    }
    if args.runtime_image_digests.len() < args.image_pins.len() {
        missing.push("one --runtime-image-digest per --image-pin");
    }
    if args.pool_model_resources.len() != args.pool_resources.len() {
        missing.push("one --pool-model-resource mapping per --pool-resource");
    }
    if args.pool_backend_services.len() != args.pool_resources.len() {
        missing.push("one --pool-backend-service mapping per --pool-resource");
    }
    validate_ipp_inputs(args, &mut missing);
    validate_signal_transition_inputs(args, &mut missing);
    report.checks.push(Check {
        name: "run-inputs".to_owned(),
        status: if missing.is_empty() { "pass" } else { "blocked" }.to_owned(),
        detail: if missing.is_empty() {
            "all mode-specific inputs are configured; credential value omitted".to_owned()
        } else {
            format!("missing required inputs: {}", missing.join(", "))
        },
        observed_epoch_seconds: epoch_seconds(),
    });
}

fn validate_ipp_inputs(args: &GridLlmdArgs, missing: &mut Vec<&'static str>) {
    match args.ipp_state {
        IppState::Unknown => missing.push("--ipp-state present|absent after checking the actual serving path"),
        IppState::Present if args.ipp_resources.is_empty() => {
            missing.push("--ipp-resource for each relevant IPP configuration/attachment resource");
        },
        IppState::Absent if !args.ipp_resources.is_empty() => {
            missing.push("remove --ipp-resource entries when --ipp-state=absent");
        },
        IppState::Present | IppState::Absent => {},
    }
}

fn validate_transition_request_inputs(args: &GridLlmdArgs, missing: &mut Vec<&'static str>) {
    if !args.require_signal_transition {
        return;
    }
    let body_oracle = args.extproc_body_oracle.as_deref();
    let request_oracle = args.extproc_request_oracle.as_deref();
    if !transition_request_oracles_ready(body_oracle, request_oracle, &args.model, &args.prompt) {
        if body_oracle.is_none() {
            missing.push("--extproc-body-oracle response digest header");
        } else if body_oracle.is_some_and(|header| !valid_header_name(header)) {
            missing.push("--extproc-body-oracle as a valid response header name");
        }
        if request_oracle.is_some() && !mutation_oracle_is_distinct(request_oracle, &args.model, &args.prompt) {
            missing.push("a non-empty request-mutation oracle absent from the original model/prompt");
        }
    }
    if args.pool_a_response_oracle.is_none() || args.pool_b_response_oracle.is_none() {
        missing.push("--pool-a-response-oracle and --pool-b-response-oracle");
    }
    if !args.request_id_header.eq_ignore_ascii_case("x-request-id") {
        missing.push("--request-id-header=x-request-id for independent simulator receipt correlation");
    }
}

fn validate_signal_transition_inputs(args: &GridLlmdArgs, missing: &mut Vec<&'static str>) {
    if !args.require_signal_transition {
        validate_epp_scoring_inputs(args, missing);
        return;
    }
    for (is_missing, name) in [
        (args.pressure_hook.is_none(), "--pressure-hook"),
        (args.recovery_hook.is_none(), "--recovery-hook"),
        (args.gateway_revision_hook.is_none(), "--gateway-revision-hook"),
        (
            args.extproc_failure_closed_hook.is_none(),
            "--extproc-failure-closed-hook for isolated fail-closed verification",
        ),
    ] {
        if is_missing {
            missing.push(name);
        }
    }
    validate_epp_scoring_inputs(args, missing);
    if args.simulator_control_resources.is_empty() {
        missing.push("--simulator-control-resource");
    }
    validate_pressure_pool_input(args, missing);
    if args.pool_resources.len() != 2 {
        missing.push("exactly two --pool-resource values");
    }
    if configured_pool_attribution(
        args.pool_a_provider_attribution.as_deref(),
        args.pool_a_backend_attribution.as_deref(),
        args.pool_a_attribution.as_deref(),
    )
    .is_none()
        || configured_pool_attribution(
            args.pool_b_provider_attribution.as_deref(),
            args.pool_b_backend_attribution.as_deref(),
            args.pool_b_attribution.as_deref(),
        )
        .is_none()
    {
        missing.push("provider and backend attribution values for both pools");
    }
    if args.grid_site_resource.is_none() {
        missing.push("--grid-site-resource");
    }
    if args.grid_gateway_name.as_deref().is_none_or(str::is_empty) {
        missing.push("--grid-gateway-name for exact GridNetwork overlay binding");
    }
    if args.site_identity_secret.is_none() || args.grid_ca_secret.is_none() || args.expected_site_san.is_none() {
        missing.push("--site-identity-secret, --grid-ca-secret, and --expected-site-san");
    }
}

fn validate_epp_scoring_inputs(args: &GridLlmdArgs, missing: &mut Vec<&'static str>) {
    if args.epp_metric_name.as_deref().is_none_or(str::is_empty) || args.epp_metrics_services.len() != 2 {
        missing.push("--epp-metric-name and exactly two --epp-metrics-service values");
    }
    if args.epp_metrics_hook.is_none()
        || args.epp_metrics_service_account.as_deref().is_none_or(str::is_empty)
        || args
            .epp_metrics_service_account_namespace
            .as_deref()
            .is_none_or(str::is_empty)
    {
        missing.push("--epp-metrics-hook and dedicated EPP metrics scraper ServiceAccount inputs");
    }
    if args
        .epp_metrics_services
        .iter()
        .any(|service| parse_epp_metrics_ref(service, &args.namespace).is_ok_and(|parsed| parsed.pool.is_none()))
    {
        missing.push("each --epp-metrics-service with an explicit #InferencePool-name label");
    }
    if args
        .epp_queue_capacity
        .is_none_or(|capacity| !capacity.is_finite() || capacity <= 0.0)
    {
        missing.push("--epp-queue-capacity as a positive finite number");
    }
    if args.grid_provider_a.as_deref().is_none_or(str::is_empty)
        || args.grid_provider_b.as_deref().is_none_or(str::is_empty)
        || args.grid_provider_a == args.grid_provider_b
    {
        missing.push("distinct --grid-provider-a and --grid-provider-b names from the installed InferenceProvider CRs");
    }
}

fn transition_request_oracles_ready(
    body_oracle_header: Option<&str>,
    optional_request_oracle: Option<&str>,
    model: &str,
    prompt: &str,
) -> bool {
    body_oracle_header.is_some_and(valid_header_name)
        && optional_request_oracle.is_none_or(|oracle| mutation_oracle_is_distinct(Some(oracle), model, prompt))
}

fn validate_pressure_pool_input(args: &GridLlmdArgs, missing: &mut Vec<&'static str>) {
    let Some(pressure_pool) = args.pressure_pool.as_deref().filter(|value| !value.is_empty()) else {
        return;
    };
    let configured = args.pool_resources.iter().any(|reference| {
        reference
            .split('#')
            .next()
            .and_then(|resource| resource.rsplit('/').next())
            == Some(pressure_pool)
    });
    if !configured {
        missing.push("--pressure-pool matching one configured InferencePool name");
    }
}

fn mutation_oracle_is_distinct(oracle: Option<&str>, model: &str, prompt: &str) -> bool {
    let Some(oracle) = oracle.filter(|value| !value.is_empty()) else {
        return false;
    };
    !prompt.contains(oracle) && !oracle.contains(prompt) && !model.contains(oracle) && !oracle.contains(model)
}

fn render_request_body(model: &str, prompt: &str) -> Value {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    })
}

fn redacted_request_body(body: &Value) -> Value {
    let mut safe = body.clone();
    safe["messages"][0]["content"] = Value::String("[SYNTHETIC_TEST_PROMPT_OMITTED]".to_owned());
    safe
}

fn sample_epp_metrics(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    phase: &str,
) -> Result<BTreeMap<String, f64>, String> {
    let Some(metric_name) = args.epp_metric_name.as_deref() else {
        add_blocked_check(report, &format!("epp/{phase}"), "EPP metric name was not configured");
        return Ok(BTreeMap::new());
    };
    let hook = args
        .epp_metrics_hook
        .as_deref()
        .ok_or_else(|| "--epp-metrics-hook is required for authenticated EPP signal sampling".to_owned())?;
    let service_account = args
        .epp_metrics_service_account
        .as_deref()
        .ok_or_else(|| "--epp-metrics-service-account is required".to_owned())?;
    let service_account_namespace = args
        .epp_metrics_service_account_namespace
        .as_deref()
        .ok_or_else(|| "--epp-metrics-service-account-namespace is required".to_owned())?;
    let mut hook_args = vec![
        args.timeout_seconds.to_string(),
        hook.display().to_string(),
        "--context".to_owned(),
        args.context.clone().unwrap_or_default(),
        "--namespace".to_owned(),
        args.namespace.clone(),
        "--metrics-service-account".to_owned(),
        service_account.to_owned(),
        "--metrics-service-account-namespace".to_owned(),
        service_account_namespace.to_owned(),
        "--metric-name".to_owned(),
        metric_name.to_owned(),
        "--pool-label".to_owned(),
        args.epp_pool_label.clone(),
        "--evidence-dir".to_owned(),
        evidence_dir.display().to_string(),
        "--phase".to_owned(),
        phase.to_owned(),
    ];
    let mut expected_pools = std::collections::BTreeSet::new();
    for service in &args.epp_metrics_services {
        let metrics_ref = parse_epp_metrics_ref(service, &args.namespace)?;
        let metrics_service_name = metrics_ref.safe_name();
        let pool = metrics_ref
            .pool
            .ok_or_else(|| format!("EPP metrics Service {metrics_service_name} must include #<InferencePool-name>"))?;
        expected_pools.insert(pool.clone());
        hook_args.extend([
            "--service".to_owned(),
            format!(
                "{}={}/{}:{}",
                pool, metrics_ref.namespace, metrics_ref.name, metrics_ref.port
            ),
        ]);
    }
    let capture = run_command_owned("timeout", &hook_args)?;
    write_capture(evidence_dir, &format!("epp-metrics-hook-{phase}"), &capture)?;
    if !capture.success() {
        add_blocked_check(
            report,
            &format!("epp/{phase}"),
            "authenticated EPP metrics hook failed; token value was not recorded",
        );
        return Ok(BTreeMap::new());
    }
    let hook_report: EppMetricsHookReport = serde_json::from_str(&capture.stdout())
        .map_err(|error| format!("EPP metrics hook returned invalid JSON: {error}"))?;
    let expected_service_account = format!("{service_account_namespace}/{service_account}");
    let valid = hook_report.schema == "praxis-extproc.grid-llmd-epp-metrics/v1"
        && hook_report.phase == phase
        && hook_report.metrics_scraper_service_account == expected_service_account
        && api_access_acceptable(args, hook_report.api_ca_verified)
        && hook_report
            .values
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            == expected_pools
        && hook_report.values.values().all(|value| value.is_finite());
    if !valid {
        add_blocked_check(
            report,
            &format!("epp/{phase}"),
            "authenticated EPP metrics hook identity, pool coverage, API mode, or sample values failed",
        );
        return Ok(BTreeMap::new());
    }
    let values = hook_report.values;
    let sample = SignalSample {
        phase: phase.to_owned(),
        observed_epoch_seconds: epoch_seconds(),
        values: values.clone(),
    };
    let file_name = format!(
        "epp-metrics-{}-{}.json",
        safe_name(phase),
        report.signal_samples.len() + 1
    );
    write_json_private(&evidence_dir.join(file_name), &sample)?;
    report.signal_samples.push(sample);
    if values.is_empty() {
        add_blocked_check(
            report,
            &format!("epp/{phase}"),
            "no valid EPP metric samples were collected",
        );
    }
    Ok(values)
}

struct EppMetricsRef {
    namespace: String,
    name: String,
    port: String,
    pool: Option<String>,
}

impl EppMetricsRef {
    fn safe_name(&self) -> String {
        format!("{}/{}:{}", self.namespace, self.name, self.port)
    }
}

fn parse_epp_metrics_ref(value: &str, default_namespace: &str) -> Result<EppMetricsRef, String> {
    let (reference, pool) = value
        .split_once('#')
        .map_or((value, None), |(r, p)| (r, Some(p.to_owned())));
    let (service_ref, port) = reference
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid EPP metrics service {value:?}; expected [namespace/]name:port[#pool]"))?;
    let (namespace, name) = service_ref
        .split_once('/')
        .map_or((default_namespace, service_ref), |(ns, name)| (ns, name));
    if namespace.is_empty() || name.is_empty() || port.is_empty() {
        return Err(format!(
            "invalid EPP metrics service {value:?}; namespace, name, and port are required"
        ));
    }
    Ok(EppMetricsRef {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        port: port.to_owned(),
        pool: pool.filter(|value| !value.is_empty()),
    })
}

#[cfg(test)]
type EppMetricSample = (Option<String>, f64);

#[cfg(test)]
fn parse_prometheus_metric(metrics: &str, metric_name: &str, pool_label: &str) -> Result<Vec<EppMetricSample>, String> {
    let mut samples = Vec::new();
    for line in metrics.lines().filter(|line| !line.starts_with('#')) {
        let Some((name_and_labels, value_text)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let name_end = name_and_labels.find('{').unwrap_or(name_and_labels.len());
        if name_and_labels.get(..name_end) != Some(metric_name) {
            continue;
        }
        let value = value_text
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .ok_or_else(|| format!("EPP metric {metric_name:?} has a non-finite or invalid sample"))?;
        let pool = name_and_labels
            .find('{')
            .and_then(|start| {
                name_and_labels
                    .rfind('}')
                    .and_then(|end| name_and_labels.get(start + 1..end))
            })
            .and_then(|labels| parse_prometheus_label(labels, pool_label));
        samples.push((pool, value));
    }
    if samples.is_empty() {
        return Err(format!(
            "configured EPP metric {metric_name:?} was not present in the metrics response"
        ));
    }
    Ok(samples)
}

#[cfg(test)]
fn parse_prometheus_label(labels: &str, wanted: &str) -> Option<String> {
    labels.split(',').find_map(|label| {
        let (name, value) = label.trim().split_once('=')?;
        (name == wanted).then(|| value.trim().trim_matches('"').to_owned())
    })
}

fn read_simulator_controls(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    phase: &str,
    baseline: Option<&BTreeMap<String, SimulatorControlState>>,
) -> Result<BTreeMap<String, SimulatorControlState>, String> {
    let mut controls = BTreeMap::new();
    for raw in &args.simulator_control_resources {
        let parsed = parse_simulator_control_ref(raw, &args.namespace)?;
        let capture = run_command(
            "kubectl",
            &[
                "--context",
                args.context.as_deref().unwrap_or_default(),
                "--namespace",
                &parsed.namespace,
                "get",
                "configmap",
                &parsed.name,
                "-o",
                "json",
            ],
        )?;
        if !capture.success() {
            add_blocked_check(
                report,
                &format!("simulator/{phase}"),
                &format!(
                    "could not read simulator control ConfigMap {}/{}",
                    parsed.namespace, parsed.name
                ),
            );
            continue;
        }
        let object = parse_capture_json(&capture, "simulator control ConfigMap")?;
        let Some(value) = object["data"][&parsed.data_key].as_str().map(str::to_owned) else {
            add_blocked_check(
                report,
                &format!("simulator/{phase}"),
                &format!(
                    "simulator ConfigMap {}/{} has no configured data key",
                    parsed.namespace, parsed.name
                ),
            );
            continue;
        };
        let identity = format!("{}/configmap/{}#{}", parsed.namespace, parsed.name, parsed.data_key);
        let baseline_state = baseline.and_then(|items| items.get(&identity));
        let (waiting_requests, running_requests, kv_cache_usage) = simulator_metric_fields(&value);
        let request_id_headers_enabled = value
            .lines()
            .any(|line| line.trim().eq_ignore_ascii_case("enable-request-id-headers: true"));
        let state = SimulatorControlState {
            pool: parsed.pool.clone(),
            value: value.clone(),
            sha256: sha256_digest(value.as_bytes()),
            safe_metrics: (waiting_requests, running_requests, kv_cache_usage),
            request_id_headers_enabled,
        };
        let record = SimulatorControlRecord {
            phase: phase.to_owned(),
            observed_epoch_seconds: epoch_seconds(),
            resource: identity.clone(),
            data_key: parsed.data_key.clone(),
            pool: parsed.pool.clone(),
            value_bytes: value.len(),
            control_sha256: state.sha256.clone(),
            waiting_requests,
            running_requests,
            kv_cache_usage,
            changed_from_baseline: baseline_state.map(|old| old.value != value),
            restored_to_baseline: if phase == "recovery" {
                baseline_state.map(|old| old.value == value)
            } else {
                None
            },
        };
        let file_name = format!(
            "simulator-control-{}-{}.json",
            safe_name(phase),
            report.simulator_controls.len() + 1
        );
        write_json_private(&evidence_dir.join(file_name), &record)?;
        report.simulator_controls.push(record);
        controls.insert(identity, state);
    }
    if controls.is_empty() && args.require_signal_transition {
        add_blocked_check(
            report,
            &format!("simulator/{phase}"),
            "no simulator control values were read",
        );
    }
    Ok(controls)
}

fn verify_simulator_hook_report(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    phase: &str,
    hook_stdout: &str,
    controls: &BTreeMap<String, SimulatorControlState>,
) -> Result<(), String> {
    let hook: SimulatorHookReport = serde_json::from_str(hook_stdout)
        .map_err(|error| format!("{phase} hook did not emit the required simulator-pod JSON report: {error}"))?;
    if hook.schema != "praxis-extproc.grid-llmd-simulator-hook/v1"
        || hook.phase != phase
        || !api_access_acceptable(args, hook.api_ca_verified)
    {
        add_blocked_check(
            report,
            &format!("simulator/{phase}-hook-report"),
            "hook schema/phase or API access mode did not match; see report API TLS mode",
        );
        return Err(format!(
            "{phase} hook report schema, phase, or API CA verification did not match"
        ));
    }

    let mut controls_by_identity = BTreeMap::new();
    for raw in &args.simulator_control_resources {
        let parsed = parse_simulator_control_ref(raw, &args.namespace)?;
        let identity = format!("{}/configmap/{}#{}", parsed.namespace, parsed.name, parsed.data_key);
        controls_by_identity.insert(identity, parsed);
    }
    let mut pods_by_namespace = BTreeMap::new();
    for parsed in controls_by_identity.values() {
        if pods_by_namespace.contains_key(&parsed.namespace) {
            continue;
        }
        let capture = run_command(
            "kubectl",
            &[
                "--context",
                args.context.as_deref().unwrap_or_default(),
                "--namespace",
                &parsed.namespace,
                "get",
                "pods",
                "-o",
                "json",
            ],
        )?;
        write_capture(
            evidence_dir,
            &format!("simulator-pods-{phase}-{}", safe_name(&parsed.namespace)),
            &capture,
        )?;
        if !capture.success() {
            add_blocked_check(
                report,
                &format!("simulator/{phase}-pod-consumption"),
                "simulator Pods could not be read after the control hook",
            );
            return Err(format!(
                "could not list simulator Pods in namespace {}",
                parsed.namespace
            ));
        }
        pods_by_namespace.insert(
            parsed.namespace.clone(),
            parse_capture_json(&capture, "simulator Pods")?,
        );
    }

    let mut observations = BTreeMap::new();
    let mut all_valid = !controls.is_empty() && api_access_acceptable(args, hook.api_ca_verified);
    for observation in hook.observations {
        let observation_key = format!("{}#{}", observation.control_resource, observation.pod_uid);
        if observations.insert(observation_key, observation.clone()).is_some() {
            all_valid = false;
            continue;
        }
        let Some(control) = controls.get(&observation.control_resource) else {
            all_valid = false;
            continue;
        };
        let Some(control_ref) = controls_by_identity.get(&observation.control_resource) else {
            all_valid = false;
            continue;
        };
        let Some(pods) = pods_by_namespace.get(&control_ref.namespace) else {
            all_valid = false;
            continue;
        };
        let pod = pods["items"].as_array().and_then(|items| {
            items.iter().find(|pod| {
                pod["metadata"]["name"].as_str() == Some(observation.pod_name.as_str())
                    && pod["metadata"]["uid"].as_str() == Some(observation.pod_uid.as_str())
            })
        });
        let container = pod.and_then(|pod| {
            pod["status"]["containerStatuses"].as_array().and_then(|statuses| {
                statuses
                    .iter()
                    .find(|status| status["name"].as_str() == Some(observation.container.as_str()))
            })
        });
        let pod_ready = pod.is_some_and(|pod| {
            pod["status"]["phase"].as_str() == Some("Running")
                && pod["status"]["conditions"].as_array().is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"].as_str() == Some("Ready") && condition["status"].as_str() == Some("True")
                    })
                })
        });
        let image_id_matches = container.is_some_and(|status| {
            status["ready"].as_bool() == Some(true) && status["imageID"].as_str() == Some(observation.image_id.as_str())
        });
        let config_is_mounted =
            pod.is_some_and(|pod| pod_mounts_configmap(pod, &observation.container, &control_ref.name));
        let digest_matches = control.sha256 == observation.control_sha256;
        let ready = observation.ready
            && pod_ready
            && image_id_matches
            && config_is_mounted
            && digest_matches
            && !observation.pod_uid.is_empty()
            && !observation.image_id.is_empty();
        all_valid &= ready;
        if ready {
            report.simulator_pods.push(SimulatorPodRecord {
                phase: phase.to_owned(),
                control_resource: observation.control_resource.clone(),
                namespace: control_ref.namespace.clone(),
                pod_name: observation.pod_name.clone(),
                pod_uid: observation.pod_uid.clone(),
                container: observation.container.clone(),
                image_id: observation.image_id.clone(),
                control_sha256: observation.control_sha256.clone(),
                ready,
                observed_epoch_seconds: epoch_seconds(),
            });
        }
    }

    for (identity, control_ref) in &controls_by_identity {
        let Some(pods) = pods_by_namespace.get(&control_ref.namespace) else {
            all_valid = false;
            continue;
        };
        let ready_mounted_pods = pods["items"].as_array().into_iter().flatten().filter(|pod| {
            pod_mounts_configmap(pod, "", &control_ref.name)
                && pod["status"]["phase"].as_str() == Some("Running")
                && pod["status"]["conditions"].as_array().is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"].as_str() == Some("Ready") && condition["status"].as_str() == Some("True")
                    })
                })
        });
        let ready_uids = ready_mounted_pods
            .filter_map(|pod| pod["metadata"]["uid"].as_str())
            .collect::<Vec<_>>();
        if ready_uids.is_empty()
            || ready_uids
                .iter()
                .any(|uid| !observations.keys().any(|key| key == &format!("{identity}#{uid}")))
        {
            all_valid = false;
        }
    }

    record_boolean_check(
        report,
        &format!("simulator/{phase}-pod-consumption"),
        all_valid,
        format!(
            "hook observations={}, ready simulator Pods={}; ConfigMap content digests, mounted volume, live Pod UIDs, readiness, and image IDs were checked",
            observations.len(),
            report.simulator_pods.iter().filter(|pod| pod.phase == phase).count()
        ),
    );
    if !all_valid {
        add_blocked_check(
            report,
            &format!("simulator/{phase}-pod-consumption"),
            "hook report did not prove that each ready simulator Pod consumed the current control digest",
        );
        return Err(format!("{phase} hook did not prove simulator Pod control consumption"));
    }
    let safe_report = serde_json::json!({
        "schema": hook.schema,
        "phase": hook.phase,
        "observations": report.simulator_pods.iter().filter(|pod| pod.phase == phase).collect::<Vec<_>>(),
    });
    write_json_private(&evidence_dir.join(format!("simulator-hook-{phase}.json")), &safe_report)
}

fn pod_mounts_configmap(pod: &Value, container_name: &str, config_map_name: &str) -> bool {
    let volume_names = pod["spec"]["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|volume| {
            (volume["configMap"]["name"].as_str() == Some(config_map_name))
                .then(|| volume["name"].as_str())
                .flatten()
        });
    let volume_names = volume_names.collect::<std::collections::BTreeSet<_>>();
    if volume_names.is_empty() {
        return false;
    }
    if container_name.is_empty() {
        return pod["spec"]["containers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|container| {
                container["volumeMounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|mount| mount["name"].as_str().is_some_and(|name| volume_names.contains(name)))
            });
    }
    pod["spec"]["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|container| {
            container["name"].as_str() == Some(container_name)
                && container["volumeMounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|mount| mount["name"].as_str().is_some_and(|name| volume_names.contains(name)))
        })
}

struct SimulatorControlRef {
    namespace: String,
    name: String,
    data_key: String,
    pool: Option<String>,
}

fn parse_simulator_control_ref(value: &str, default_namespace: &str) -> Result<SimulatorControlRef, String> {
    let (reference, data_key_pool) = value
        .split_once('#')
        .ok_or_else(|| format!("invalid simulator control {value:?}; expected configmap/name#data-key[@pool]"))?;
    let (data_key, pool) = data_key_pool
        .split_once('@')
        .map_or((data_key_pool, None), |(key, pool)| (key, Some(pool.to_owned())));
    let parts = reference.split('/').collect::<Vec<_>>();
    let (namespace, kind, name) = match parts.as_slice() {
        [kind, name] => (default_namespace, *kind, *name),
        [namespace, kind, name] => (*namespace, *kind, *name),
        _ => {
            return Err(format!(
                "invalid simulator control {value:?}; expected [namespace/]configmap/name#data-key[@pool]"
            ));
        },
    };
    if !is_resource_kind(kind, &["configmap", "configmaps"]) || name.is_empty() || data_key.is_empty() {
        return Err(format!(
            "invalid simulator control {value:?}; resource must be a ConfigMap with a data key"
        ));
    }
    Ok(SimulatorControlRef {
        namespace: namespace.to_owned(),
        name: name.to_owned(),
        data_key: data_key.to_owned(),
        pool: pool.filter(|value| !value.is_empty()),
    })
}

fn simulator_metric_fields(value: &str) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mut waiting = None;
    let mut running = None;
    let mut kv_cache = None;
    for line in value.lines() {
        let Some((key, raw)) = line.trim().split_once(':') else {
            continue;
        };
        let parsed = raw
            .trim()
            .trim_matches('"')
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite());
        match key.trim() {
            "waiting-requests" => waiting = parsed,
            "running-requests" => running = parsed,
            "kv-cache-usage" => kv_cache = parsed,
            _ => {},
        }
    }
    (waiting, running, kv_cache)
}

fn verify_simulator_pressure(
    report: &mut Report,
    baseline: &BTreeMap<String, SimulatorControlState>,
    pressure: &BTreeMap<String, SimulatorControlState>,
    expected_pool: &str,
) {
    let mut changed = false;
    for (identity, after) in pressure {
        let Some(before) = baseline.get(identity) else { continue };
        let pool_matches = after.pool.as_deref() == Some(expected_pool);
        let signal_increased = matches!((before.safe_metrics.0, after.safe_metrics.0), (Some(a), Some(b)) if b > a)
            || matches!((before.safe_metrics.2, after.safe_metrics.2), (Some(a), Some(b)) if b > a);
        if pool_matches && before.value != after.value && signal_increased {
            changed = true;
        }
    }
    record_boolean_check(
        report,
        "simulator/pressure-applied",
        changed,
        format!(
            "a supported fake-metrics queue/KV value increased for pressure pool {expected_pool:?}; ConfigMap contents omitted"
        ),
    );
    if !changed {
        add_blocked_check(
            report,
            "simulator/pressure-control",
            "pressure did not increase a queue or KV metric in a control assigned to the requested pool",
        );
    }
}

fn verify_simulator_recovery(
    report: &mut Report,
    baseline: &BTreeMap<String, SimulatorControlState>,
    recovery: &BTreeMap<String, SimulatorControlState>,
) {
    let restored = !baseline.is_empty()
        && baseline
            .iter()
            .all(|(identity, before)| recovery.get(identity).is_some_and(|after| before.value == after.value));
    record_boolean_check(
        report,
        "simulator/pressure-removed",
        restored,
        "every configured simulator control value returned byte-for-byte to its baseline; values omitted".to_owned(),
    );
    if !restored {
        add_blocked_check(
            report,
            "simulator/recovery-control",
            "simulator control values did not exactly restore to baseline",
        );
    }
}

fn wait_for_metric_change(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    baseline: &BTreeMap<String, f64>,
    phase: &str,
    pool: &str,
) -> Result<BTreeMap<String, f64>, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout_seconds);
    let mut last = BTreeMap::new();
    while std::time::Instant::now() < deadline {
        last = sample_epp_metrics(args, report, evidence_dir, phase)?;
        if metric_pool_changed(baseline, &last, pool) {
            record_boolean_check(
                report,
                "epp/pressure-metric-changed",
                true,
                format!("configured EPP metric changed for pool {pool:?}; samples recorded with timestamps"),
            );
            return Ok(last);
        }
        std::thread::sleep(Duration::from_secs(args.poll_interval_seconds.max(1)));
    }
    add_blocked_check(
        report,
        "epp/pressure-metric-changed",
        &format!("EPP metric did not change for pressure pool {pool:?} before timeout; last values={last:?}"),
    );
    Err("timed out waiting for a live EPP metric change".to_owned())
}

fn validate_metric_pool_coverage(args: &GridLlmdArgs, report: &mut Report, values: &BTreeMap<String, f64>) {
    let pool_names = args
        .pool_resources
        .iter()
        .filter_map(|reference| reference.split('#').next())
        .filter_map(|reference| reference.rsplit('/').next())
        .collect::<Vec<_>>();
    if pool_names.len() < 2 {
        add_blocked_check(
            report,
            "epp/pool-coverage",
            "two pool resources are required for signal routing",
        );
        return;
    }
    let missing = pool_names
        .iter()
        .filter(|name| !values.keys().any(|series| series == **name))
        .copied()
        .collect::<Vec<_>>();
    record_boolean_check(
        report,
        "epp/pool-coverage",
        missing.is_empty(),
        if missing.is_empty() {
            format!("EPP metric samples identify both pools: {}", pool_names.join(", "))
        } else {
            format!("metric series did not identify pool(s): {}", missing.join(", "))
        },
    );
    if !missing.is_empty() {
        add_blocked_check(
            report,
            "epp/pool-coverage",
            "supply EPP service pool suffixes or a metric label that identifies both pool resources",
        );
    }
}

fn metric_pool_changed(before: &BTreeMap<String, f64>, after: &BTreeMap<String, f64>, pool: &str) -> bool {
    before
        .iter()
        .any(|(series, old)| series == pool && after.get(series).is_some_and(|new| (new - old).abs() > f64::EPSILON))
}

fn wait_for_metric_recovery(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    baseline: &BTreeMap<String, f64>,
    phase: &str,
    pool: &str,
) -> Result<BTreeMap<String, f64>, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout_seconds);
    let mut last = BTreeMap::new();
    while std::time::Instant::now() < deadline {
        last = sample_epp_metrics(args, report, evidence_dir, phase)?;
        if metric_pool_matches(baseline, &last, pool) {
            return Ok(last);
        }
        std::thread::sleep(Duration::from_secs(args.poll_interval_seconds.max(1)));
    }
    add_blocked_check(
        report,
        "epp/recovery-metric",
        &format!("EPP metric did not return to baseline for pool {pool:?}; last values={last:?}"),
    );
    Err("timed out waiting for EPP metric recovery".to_owned())
}

fn metric_pool_matches(before: &BTreeMap<String, f64>, after: &BTreeMap<String, f64>, pool: &str) -> bool {
    let matching = before
        .iter()
        .filter(|(series, _)| series.as_str() == pool)
        .collect::<Vec<_>>();
    !matching.is_empty()
        && matching
            .iter()
            .all(|(series, old)| after.get(*series).is_some_and(|new| (new - *old).abs() <= 1e-6))
}

fn verify_metric_recovery(report: &mut Report, baseline: &BTreeMap<String, f64>, recovery: &BTreeMap<String, f64>) {
    let recovered = baseline
        .iter()
        .all(|(series, old)| recovery.get(series).is_some_and(|new| (new - old).abs() <= 1e-6));
    record_boolean_check(
        report,
        "epp/recovery-values",
        recovered,
        "all baseline EPP series returned within 1e-6; exact values are in timestamped metric samples".to_owned(),
    );
}

fn observe_grid_revision(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    phase: &str,
) -> Result<GridRevisionRecord, String> {
    let network_ref = gate_resource_ref(args, "grid")?;
    let expected_gateway = gate_gateway_name(args)?;
    let network = get_resource(
        evidence_dir,
        args.context.as_deref().unwrap_or_default(),
        Some(&network_ref.namespace),
        &network_ref.kind,
        &network_ref.name,
        &format!("gridnetwork-{phase}-{}", report.grid_revisions.len() + 1),
    )?;
    if !network.success() {
        add_blocked_check(
            report,
            &format!("grid/{phase}-overlay"),
            "GridNetwork could not be read",
        );
        return Err("could not read GridNetwork status".to_owned());
    }
    let network_json = parse_capture_json(&network, "GridNetwork")?;
    let (status_field, statuses) = grid_routing_map_status_entries(&network_json);
    let status = statuses.and_then(|items| {
        items
            .iter()
            .find(|entry| entry["gatewayName"].as_str() == Some(&expected_gateway))
    });
    let Some(status) = status else {
        add_blocked_check(
            report,
            &format!("grid/{phase}-overlay"),
            &format!("GridNetwork.status.{status_field} has no entry for the serving Gateway"),
        );
        return Err("GridNetwork overlay status does not identify the serving Gateway".to_owned());
    };
    let namespace = status["namespace"]
        .as_str()
        .unwrap_or(&network_ref.namespace)
        .to_owned();
    let config_map_name = status["configMapName"].as_str().unwrap_or_default().to_owned();
    let rendered_revision = status["renderedRevision"].as_str().unwrap_or_default().to_owned();
    let distributed_revision = status["distributedRevision"].as_str().unwrap_or_default().to_owned();
    let content_digest = status["contentDigest"].as_str().unwrap_or_default().to_owned();
    let network_generation = network_json["metadata"]["generation"].as_u64();
    let observed_generation = status["observedGeneration"].as_u64();
    if config_map_name.is_empty() {
        add_blocked_check(
            report,
            &format!("grid/{phase}-overlay"),
            "GridNetwork overlay status has no ConfigMap reference",
        );
        return Err("GridNetwork status omitted the overlay ConfigMap name".to_owned());
    }
    let overlay = get_resource(
        evidence_dir,
        args.context.as_deref().unwrap_or_default(),
        Some(&namespace),
        "configmap",
        &config_map_name,
        &format!("grid-overlay-{phase}-{}", report.grid_revisions.len() + 1),
    )?;
    if !overlay.success() {
        add_blocked_check(
            report,
            &format!("grid/{phase}-overlay"),
            "GridNetwork status references a missing overlay ConfigMap",
        );
        return Err("Grid overlay ConfigMap could not be read".to_owned());
    }
    let overlay_json = parse_capture_json(&overlay, "Grid overlay ConfigMap")?;
    let envelope_text = overlay_json["data"]["routing-overlay.json"]
        .as_str()
        .unwrap_or_default();
    let envelope = serde_json::from_str::<Value>(envelope_text).unwrap_or(Value::Null);
    let revision = envelope["revision"]["value"].as_str().unwrap_or_default();
    let digest = envelope["content_digest"]["value"].as_str().unwrap_or_default();
    let producer = envelope["provenance"]["producer"].as_str().unwrap_or_default();
    let annotations = &overlay_json["metadata"]["annotations"];
    let consistent = network_generation.is_some()
        && observed_generation == network_generation
        && status["phase"].as_str() == Some("Distributed")
        && !rendered_revision.is_empty()
        && rendered_revision == distributed_revision
        && revision == distributed_revision
        && digest == content_digest
        && producer == "grid-operator"
        && annotations["grid.praxis.fast/overlay-revision"].as_str() == Some(distributed_revision.as_str())
        && annotations["grid.praxis.fast/overlay-content-digest"].as_str() == Some(content_digest.as_str())
        && overlay_json["data"]["routing-config.json"].is_string();
    let (candidate_scores, candidate_ranks, candidate_queue_scores) = overlay_candidate_state(&envelope);
    let (candidate_count, candidate_identity_count) = overlay_candidate_counts(&envelope);
    let record = GridRevisionRecord {
        phase: phase.to_owned(),
        observed_epoch_seconds: epoch_seconds(),
        status_field: status_field.to_owned(),
        gateway: expected_gateway,
        overlay_phase: status["phase"].as_str().unwrap_or("unknown").to_owned(),
        network_generation,
        observed_generation,
        config_map_namespace: namespace,
        config_map_name,
        rendered_revision,
        distributed_revision,
        content_digest,
        config_consistent: consistent,
        candidate_count,
        candidate_identity_count,
        candidate_scores,
        candidate_ranks,
        candidate_queue_scores,
    };
    let file_name = format!(
        "grid-revision-{}-{}.json",
        safe_name(phase),
        report.grid_revisions.len() + 1
    );
    write_json_private(&evidence_dir.join(file_name), &record)?;
    report.grid_revisions.push(record.clone());
    Ok(record)
}

fn overlay_candidate_state(envelope: &Value) -> OverlayCandidateState {
    let mut scores = BTreeMap::new();
    let mut ranks = BTreeMap::new();
    let mut queue_scores = BTreeMap::new();
    if let Some(candidates) = envelope["overlay"]["candidates"].as_array() {
        for candidate in candidates {
            let identity = format!(
                "{}/{}/{}/{}",
                candidate["kind"].as_str().unwrap_or(""),
                candidate["name"].as_str().unwrap_or(""),
                candidate["site"].as_str().unwrap_or(""),
                candidate["cluster"].as_str().unwrap_or("")
            );
            if let Some(score) = candidate["score"].as_f64() {
                scores.insert(identity.clone(), score);
            }
            if let Some(rank) = candidate["rank"].as_u64() {
                ranks.insert(identity.clone(), rank);
            }
            if let Some(queue_score) = candidate["score_breakdown"]["queue_depth"].as_f64() {
                queue_scores.insert(identity, queue_score);
            }
        }
    }
    (scores, ranks, queue_scores)
}

fn overlay_candidate_counts(envelope: &Value) -> (usize, usize) {
    let Some(candidates) = envelope["overlay"]["candidates"].as_array() else {
        return (0, 0);
    };
    let identities = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{}/{}/{}/{}",
                candidate["kind"].as_str().unwrap_or(""),
                candidate["name"].as_str().unwrap_or(""),
                candidate["site"].as_str().unwrap_or(""),
                candidate["cluster"].as_str().unwrap_or("")
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    (candidates.len(), identities.len())
}

fn expected_queue_depth_score(queue_depth: f64, queue_capacity: f64) -> Option<f64> {
    if !queue_depth.is_finite() || queue_depth < 0.0 || !queue_capacity.is_finite() || queue_capacity <= 0.0 {
        return None;
    }
    // Grid main c21ae729 resolves QueueDepth to weight 1.0; the operator
    // normalizes EPP queue counts by InferenceProvider.metricsConfig.queueCapacity.
    Some(1.0 - (queue_depth / queue_capacity).clamp(0.0, 1.0))
}

fn pool_resource_name(reference: &str) -> String {
    reference
        .split('#')
        .next()
        .unwrap_or(reference)
        .rsplit('/')
        .next()
        .unwrap_or(reference)
        .to_owned()
}

fn candidate_matches_pool(candidate: &str, pool: &str) -> bool {
    let candidate = candidate.rsplit('/').next().unwrap_or(candidate);
    candidate == pool || candidate.strip_prefix("provider:") == Some(pool)
}

fn provider_candidate_value(values: &BTreeMap<String, f64>, model: &str, provider: &str) -> Option<f64> {
    let matches = values
        .iter()
        .filter(|(identity, _)| {
            let mut fields = identity.split('/');
            fields.next() == Some("inference_model")
                && fields.next() == Some(model)
                && candidate_matches_pool(identity, provider)
        })
        .map(|(_, value)| *value)
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0])
}

fn provider_candidate_rank(ranks: &BTreeMap<String, u64>, model: &str, provider: &str) -> Option<u64> {
    let matches = ranks
        .iter()
        .filter(|(identity, _)| {
            let mut fields = identity.split('/');
            fields.next() == Some("inference_model")
                && fields.next() == Some(model)
                && candidate_matches_pool(identity, provider)
        })
        .map(|(_, rank)| *rank)
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0])
}

fn rank_one_pool(args: &GridLlmdArgs, grid: &GridRevisionRecord) -> Result<String, String> {
    let pools = args
        .pool_resources
        .iter()
        .take(2)
        .map(|reference| pool_resource_name(reference))
        .collect::<Vec<_>>();
    let providers = [
        args.grid_provider_a.as_deref().unwrap_or(""),
        args.grid_provider_b.as_deref().unwrap_or(""),
    ];
    if pools.len() != 2 || providers.iter().any(|provider| provider.is_empty()) {
        return Err("two pool/provider mappings are required to derive the selected candidate".to_owned());
    }
    let ranks = [
        provider_candidate_rank(&grid.candidate_ranks, &args.model, providers[0]),
        provider_candidate_rank(&grid.candidate_ranks, &args.model, providers[1]),
    ];
    selected_pool_from_ranks([pools[0].as_str(), pools[1].as_str()], ranks)
}

fn selected_pool_from_ranks(pools: [&str; 2], ranks: [Option<u64>; 2]) -> Result<String, String> {
    match ranks {
        [Some(left), Some(right)] if left < right => Ok(pools[0].to_owned()),
        [Some(left), Some(right)] if right < left => Ok(pools[1].to_owned()),
        _ => Err(format!(
            "the current Grid overlay does not publish two distinct ranks for the configured providers: {ranks:?}"
        )),
    }
}

fn expected_probe_pool(
    args: &GridLlmdArgs,
    report: &Report,
    phase: &str,
    phase_grid: &GridRevisionRecord,
) -> Result<String, String> {
    let selection_grid = if phase == "pressure" {
        phase_grid
    } else {
        report
            .grid_revisions
            .iter()
            .find(|revision| revision.phase == "baseline")
            .ok_or_else(|| "the original baseline Grid overlay is missing".to_owned())?
    };
    rank_one_pool(args, selection_grid)
}

fn configured_pool_attribution_for_name<'a>(args: &'a GridLlmdArgs, pool: &str) -> Option<(&'a str, &'a str)> {
    let pools = args
        .pool_resources
        .iter()
        .take(2)
        .map(|reference| pool_resource_name(reference))
        .collect::<Vec<_>>();
    if pools.first().is_some_and(|name| name == pool) {
        configured_pool_attribution(
            args.pool_a_provider_attribution.as_deref(),
            args.pool_a_backend_attribution.as_deref(),
            args.pool_a_attribution.as_deref(),
        )
    } else if pools.get(1).is_some_and(|name| name == pool) {
        configured_pool_attribution(
            args.pool_b_provider_attribution.as_deref(),
            args.pool_b_backend_attribution.as_deref(),
            args.pool_b_attribution.as_deref(),
        )
    } else {
        None
    }
}

fn configured_pool_response_oracle<'a>(args: &'a GridLlmdArgs, pool: &str) -> Option<&'a str> {
    let pools = args
        .pool_resources
        .iter()
        .take(2)
        .map(|reference| pool_resource_name(reference))
        .collect::<Vec<_>>();
    if pools.first().is_some_and(|name| name == pool) {
        args.pool_a_response_oracle.as_deref()
    } else if pools.get(1).is_some_and(|name| name == pool) {
        args.pool_b_response_oracle.as_deref()
    } else {
        None
    }
}

fn verify_grid_queue_score_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    metric_values: &BTreeMap<String, f64>,
    candidate_count: usize,
    candidate_identity_count: usize,
    candidate_scores: &BTreeMap<String, f64>,
    candidate_ranks: &BTreeMap<String, u64>,
    candidate_queue_scores: &BTreeMap<String, f64>,
    phase: &str,
) -> bool {
    let capacity = args.epp_queue_capacity.unwrap_or(f64::NAN);
    let providers = [
        args.grid_provider_a.as_deref().unwrap_or(""),
        args.grid_provider_b.as_deref().unwrap_or(""),
    ];
    let pools = args
        .pool_resources
        .iter()
        .take(2)
        .map(|reference| pool_resource_name(reference))
        .collect::<Vec<_>>();
    let queue_metric = args.epp_metric_name.as_deref().unwrap_or("<unset>");
    if pools.len() != 2
        || providers.iter().any(|provider| provider.is_empty())
        || !capacity.is_finite()
        || capacity <= 0.0
    {
        record_boolean_check(
            report,
            &format!("grid/{phase}-queue-score-contract"),
            false,
            "two pool/provider mappings and a positive queue capacity are required".to_owned(),
        );
        return false;
    }

    let mut all_match = true;
    let mut resolved = Vec::new();
    let candidates_exactly_two = candidate_count == 2
        && candidate_identity_count == 2
        && candidate_scores.len() == 2
        && candidate_ranks.len() == 2
        && candidate_queue_scores.len() == 2;
    for (index, pool) in pools.iter().enumerate() {
        let provider = providers[index];
        let raw_queue = metric_values.get(pool).copied();
        let expected = raw_queue.and_then(|value| expected_queue_depth_score(value, capacity));
        let observed = provider_candidate_value(candidate_queue_scores, &args.model, provider);
        let total_score = provider_candidate_value(candidate_scores, &args.model, provider);
        let rank = provider_candidate_rank(candidate_ranks, &args.model, provider);
        let matches = matches!((expected, observed), (Some(expected), Some(observed)) if (expected-observed).abs() <= 0.02)
            && total_score.is_some()
            && rank.is_some();
        all_match &= matches;
        record_boolean_check(
            report,
            &format!("grid/{phase}-queue-score-{pool}"),
            matches,
            format!(
                "provider={provider}, metric={queue_metric}, EPP queue={raw_queue:?}, capacity={capacity}, expected weighted queue score={expected:?}, observed score breakdown={observed:?}, total score={total_score:?}, rank={rank:?}"
            ),
        );
        if let (Some(score), Some(rank)) = (total_score, rank) {
            resolved.push((score, rank));
        }
    }
    let ranks_valid = resolved.len() == 2
        && [resolved[0].1, resolved[1].1]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            == [0, 1].into_iter().collect()
        && (resolved[0].0 == resolved[1].0
            || (resolved[0].0 > resolved[1].0 && resolved[0].1 < resolved[1].1)
            || (resolved[1].0 > resolved[0].0 && resolved[1].1 < resolved[0].1));
    record_boolean_check(
        report,
        &format!("grid/{phase}-queue-score-contract"),
        all_match && candidates_exactly_two && ranks_valid,
        format!(
            "queue metric samples match score_breakdown.queue_depth for both task providers; exactly two candidates={candidates_exactly_two}; contiguous score-ordered ranks={ranks_valid}"
        ),
    );
    all_match && candidates_exactly_two && ranks_valid
}

fn check_grid_providers_current(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    phase: &str,
) -> Result<bool, String> {
    let context = args.context.as_deref().unwrap_or_default();
    let providers = [
        ("pool-a", args.grid_provider_a.as_deref().unwrap_or_default()),
        ("pool-b", args.grid_provider_b.as_deref().unwrap_or_default()),
    ];
    let mut all_current = true;

    for (pool, name) in providers {
        if name.is_empty() {
            all_current = false;
            record_boolean_check(
                report,
                &format!("grid/{phase}-provider-{pool}-current"),
                false,
                "provider name is not configured".to_owned(),
            );
            continue;
        }
        let capture = run_command_owned(
            "kubectl",
            &[
                "--context".to_owned(),
                context.to_owned(),
                "--namespace".to_owned(),
                args.namespace.clone(),
                "get".to_owned(),
                "inferenceprovider".to_owned(),
                name.to_owned(),
                "-o".to_owned(),
                "json".to_owned(),
            ],
        )?;
        if !capture.success() {
            all_current = false;
            write_capture(evidence_dir, &format!("grid-provider-{phase}-{pool}"), &capture)?;
            record_boolean_check(
                report,
                &format!("grid/{phase}-provider-{pool}-current"),
                false,
                "provider could not be read in the task namespace".to_owned(),
            );
            continue;
        }

        let provider: Value = serde_json::from_str(&capture.stdout())
            .map_err(|error| format!("parse {pool} InferenceProvider status: {error}"))?;
        let generation = provider["metadata"]["generation"].as_u64();
        let observed_generation = provider["status"]["observedGeneration"].as_u64();
        let condition_current = |condition_type: &str| {
            provider["status"]["conditions"].as_array().is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition["type"].as_str() == Some(condition_type)
                        && condition["status"].as_str() == Some("True")
                        && condition["observedGeneration"].as_u64() == generation
                })
            })
        };
        let ready = generation.is_some_and(|generation| observed_generation == Some(generation))
            && provider["status"]["phase"].as_str() == Some("Available")
            && condition_current("Ready")
            && condition_current("MetricsAvailable");
        all_current &= ready;
        write_json_private(
            &evidence_dir.join(format!("grid-provider-{phase}-{pool}.json")),
            &serde_json::json!({
                "name": name,
                "generation": generation,
                "observedGeneration": observed_generation,
                "phase": provider["status"]["phase"],
                "readyConditionCurrent": condition_current("Ready"),
                "metricsAvailableConditionCurrent": condition_current("MetricsAvailable"),
            }),
        )?;
        record_boolean_check(
            report,
            &format!("grid/{phase}-provider-{pool}-current"),
            ready,
            format!(
                "provider={name}, phase={:?}, generation={generation:?}, observedGeneration={observed_generation:?}, current Ready and MetricsAvailable conditions={ready}",
                provider["status"]["phase"].as_str()
            ),
        );
    }
    Ok(all_current)
}

fn check_grid_scoring_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    network: &Value,
    envelope: &Value,
    phase: &str,
) -> Result<(), String> {
    if args.mode != Mode::Run {
        return Ok(());
    }
    let policy_ok = network["spec"]["routingPolicy"].as_str() == Some("scoreFirst")
        && network["spec"]["scoringPolicy"]["strategy"].as_str() == Some("queueDepth")
        && network["spec"]["selectionPolicy"]["mode"].as_str() == Some("deterministic")
        && network["spec"]["metricsRefreshInterval"].as_str() == Some("10s");
    record_boolean_check(
        report,
        &format!("grid/{phase}-scoring-policy"),
        policy_ok,
        format!(
            "live GridNetwork policy routing={:?}, scoring={:?}, selection={:?}, refresh={:?}",
            network["spec"]["routingPolicy"].as_str(),
            network["spec"]["scoringPolicy"]["strategy"].as_str(),
            network["spec"]["selectionPolicy"]["mode"].as_str(),
            network["spec"]["metricsRefreshInterval"].as_str()
        ),
    );

    if report
        .checks
        .iter()
        .any(|check| check.name == "run-inputs" && check.status != "pass")
    {
        record_boolean_check(
            report,
            &format!("grid/{phase}-signal-scoring-preflight"),
            false,
            "baseline scoring inputs are incomplete; authenticated EPP metrics were not sampled".to_owned(),
        );
        return Ok(());
    }

    let providers_current = check_grid_providers_current(args, report, evidence_dir, phase)?;
    let metrics = sample_epp_metrics(args, report, evidence_dir, phase)?;
    validate_metric_pool_coverage(args, report, &metrics);
    let (candidate_scores, candidate_ranks, candidate_queue_scores) = overlay_candidate_state(envelope);
    let (candidate_count, candidate_identity_count) = overlay_candidate_counts(envelope);
    let score_contract_ok = verify_grid_queue_score_contract(
        args,
        report,
        &metrics,
        candidate_count,
        candidate_identity_count,
        &candidate_scores,
        &candidate_ranks,
        &candidate_queue_scores,
        phase,
    );
    if !policy_ok || !providers_current || !score_contract_ok {
        record_boolean_check(
            report,
            &format!("grid/{phase}-signal-scoring-preflight"),
            false,
            "signal-mode preflight requires the explicit queueDepth policy and both metric-matched scored candidates"
                .to_owned(),
        );
    }
    Ok(())
}

fn grid_routing_map_status_entries(network: &Value) -> (&'static str, Option<&Vec<Value>>) {
    ("routingMapStatus", network["status"]["routingMapStatus"].as_array())
}

fn grid_praxis_config_status_entries(network: &Value) -> Option<&Vec<Value>> {
    network["status"]["praxisConfigStatus"].as_array()
}

fn grid_revision_is_distributed(record: &GridRevisionRecord) -> bool {
    record.config_consistent
        && record.overlay_phase == "Distributed"
        && record.network_generation.is_some()
        && record.observed_generation == record.network_generation
        && !record.rendered_revision.is_empty()
        && record.rendered_revision == record.distributed_revision
        && !record.content_digest.is_empty()
}

fn same_distributed_grid_revision(expected: &GridRevisionRecord, observed: &GridRevisionRecord) -> bool {
    grid_revision_is_distributed(expected)
        && grid_revision_is_distributed(observed)
        && expected.gateway == observed.gateway
        && expected.network_generation == observed.network_generation
        && expected.observed_generation == observed.observed_generation
        && expected.config_map_namespace == observed.config_map_namespace
        && expected.config_map_name == observed.config_map_name
        && expected.rendered_revision == observed.rendered_revision
        && expected.distributed_revision == observed.distributed_revision
        && expected.content_digest == observed.content_digest
        && expected.candidate_count == observed.candidate_count
        && expected.candidate_identity_count == observed.candidate_identity_count
        && expected.candidate_scores == observed.candidate_scores
        && expected.candidate_ranks == observed.candidate_ranks
        && expected.candidate_queue_scores == observed.candidate_queue_scores
}

/// Recheck the score inputs against the exact current Distributed revision
/// after the serving Gateway has acknowledged that revision.
fn check_baseline_score_overlay_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    grid: &GridRevisionRecord,
    phase: &str,
) -> Result<bool, String> {
    let providers_current = check_grid_providers_current(args, report, evidence_dir, phase)?;
    let metrics = sample_epp_metrics(args, report, evidence_dir, phase)?;
    validate_metric_pool_coverage(args, report, &metrics);
    let observed_grid = observe_grid_revision(args, report, evidence_dir, &format!("{phase}-score-overlay"))?;
    let revision_stable = same_distributed_grid_revision(grid, &observed_grid);
    record_boolean_check(
        report,
        "grid/baseline-revision-stable",
        revision_stable,
        format!(
            "Grid remained at the exact Distributed revision consumed by the Gateway after the EPP scrape: expected={}, observed={}",
            grid.distributed_revision, observed_grid.distributed_revision
        ),
    );
    if !revision_stable {
        add_blocked_check(
            report,
            "grid/baseline-revision-stable",
            "Grid revision or candidate state changed while the fresh EPP metric sample was collected",
        );
    }
    let score_contract = verify_grid_queue_score_contract(
        args,
        report,
        &metrics,
        observed_grid.candidate_count,
        observed_grid.candidate_identity_count,
        &observed_grid.candidate_scores,
        &observed_grid.candidate_ranks,
        &observed_grid.candidate_queue_scores,
        phase,
    );
    let metric_coverage = metrics.len() == 2;
    let passed = revision_stable && providers_current && metric_coverage && score_contract;
    record_boolean_check(
        report,
        &format!("grid/{phase}-score-overlay-contract"),
        passed,
        format!(
            "current providers={providers_current}, authenticated EPP pool samples={}/{}, exact current Distributed Gateway-consumed revision={revision_stable}, revision={}, queue score/rank contract={score_contract}",
            metrics.len(),
            2,
            observed_grid.distributed_revision
        ),
    );
    Ok(passed)
}

fn wait_for_grid_revision_change(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    baseline: &GridRevisionRecord,
    phase: &str,
) -> Result<GridRevisionRecord, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout_seconds);
    while std::time::Instant::now() < deadline {
        let current = observe_grid_revision(args, report, evidence_dir, phase)?;
        if grid_revision_is_distributed(&current) && current.distributed_revision != baseline.distributed_revision {
            record_grid_revision_check(report, phase, &current);
            return Ok(current);
        }
        std::thread::sleep(Duration::from_secs(args.poll_interval_seconds.max(1)));
    }
    add_blocked_check(
        report,
        "grid/pressure-revision",
        "Grid did not publish a new distributed overlay revision after the EPP signal changed",
    );
    Err("timed out waiting for Grid overlay revision convergence".to_owned())
}

fn wait_for_grid_revision_recovery(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    baseline: &GridRevisionRecord,
    phase: &str,
) -> Result<GridRevisionRecord, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout_seconds);
    while std::time::Instant::now() < deadline {
        let current = observe_grid_revision(args, report, evidence_dir, phase)?;
        if grid_revision_is_distributed(&current) && current.distributed_revision == baseline.distributed_revision {
            record_grid_revision_check(report, phase, &current);
            return Ok(current);
        }
        std::thread::sleep(Duration::from_secs(args.poll_interval_seconds.max(1)));
    }
    add_blocked_check(
        report,
        "grid/recovery-revision",
        "Grid did not restore the baseline distributed overlay revision after pressure removal",
    );
    Err("timed out waiting for Grid overlay recovery".to_owned())
}

fn record_grid_revision_check(report: &mut Report, phase: &str, record: &GridRevisionRecord) {
    record_boolean_check(
        report,
        &format!("grid/{phase}-overlay"),
        grid_revision_is_distributed(record),
        format!(
            "phase={}, generation={:?}/{:?}, ConfigMap={}/{}, renderedRevision={}, distributedRevision={}, candidateCount={}, uniqueCandidateIdentities={}",
            record.overlay_phase,
            record.observed_generation,
            record.network_generation,
            record.config_map_namespace,
            record.config_map_name,
            record.rendered_revision,
            record.distributed_revision,
            record.candidate_count,
            record.candidate_identity_count
        ),
    );
}

fn verify_grid_metric_signal(
    args: &GridLlmdArgs,
    report: &mut Report,
    pressure_metrics: &BTreeMap<String, f64>,
    baseline_metrics: &BTreeMap<String, f64>,
    pressure_grid: &GridRevisionRecord,
    expected_pressure_pool: &str,
) {
    let baseline_grid = report.grid_revisions.iter().find(|entry| entry.phase == "baseline");
    let score_changed =
        baseline_grid.is_some_and(|baseline| baseline.candidate_queue_scores != pressure_grid.candidate_queue_scores);
    let ranks_reordered = baseline_grid.is_some_and(|baseline| {
        let pools = args.pool_resources.iter().take(2).map(|reference| pool_resource_name(reference)).collect::<Vec<_>>();
        let providers = [args.grid_provider_a.as_deref().unwrap_or(""), args.grid_provider_b.as_deref().unwrap_or("")];
        let expected_index = pools.iter().position(|pool| pool == expected_pressure_pool);
        let Some(expected_index) = expected_index.filter(|_| pools.len() == 2) else {
            return false;
        };
        let other_index = 1 - expected_index;
        let baseline_a = provider_candidate_rank(&baseline.candidate_ranks, &args.model, providers[expected_index]);
        let baseline_b = provider_candidate_rank(&baseline.candidate_ranks, &args.model, providers[other_index]);
        let pressure_a = provider_candidate_rank(&pressure_grid.candidate_ranks, &args.model, providers[expected_index]);
        let pressure_b = provider_candidate_rank(&pressure_grid.candidate_ranks, &args.model, providers[other_index]);
        matches!((baseline_a, baseline_b, pressure_a, pressure_b), (Some(a0), Some(b0), Some(a1), Some(b1)) if a0 < b0 && a1 > b1)
    });
    let metric_changed = metric_pool_changed(baseline_metrics, pressure_metrics, expected_pressure_pool);
    let queue_scores_match = verify_grid_queue_score_contract(
        args,
        report,
        pressure_metrics,
        pressure_grid.candidate_count,
        pressure_grid.candidate_identity_count,
        &pressure_grid.candidate_scores,
        &pressure_grid.candidate_ranks,
        &pressure_grid.candidate_queue_scores,
        "pressure",
    );
    record_boolean_check(
        report,
        "grid/signal-score-and-rank",
        score_changed && ranks_reordered && metric_changed && queue_scores_match,
        format!(
            "EPP changed={metric_changed}, queue-depth score changed={score_changed}, rank reordered for pressure pool {expected_pressure_pool}, pressure scores match current EPP samples={queue_scores_match}"
        ),
    );
}

fn wait_for_gateway_revision(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    grid: &GridRevisionRecord,
    phase: &str,
) -> Result<GatewayRevisionRecord, String> {
    let hook = args.gateway_revision_hook.as_deref().ok_or_else(|| {
        "--gateway-revision-hook is required to prove the serving Gateway consumed a revision".to_owned()
    })?;
    let route = parse_resource_ref(
        args.httproute_resource
            .as_deref()
            .ok_or_else(|| "--httproute-resource is required".to_owned())?,
        &args.namespace,
    )?;
    let service = parse_resource_ref(
        args.grid_gateway_service
            .as_deref()
            .ok_or_else(|| "--grid-gateway-service is required".to_owned())?,
        &args.namespace,
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(args.timeout_seconds);
    let mut attempt = 0_usize;
    while std::time::Instant::now() < deadline {
        attempt += 1;
        // This probe performs several API reads and two Gateway exec requests.
        // The polling interval controls retry cadence, not its per-attempt budget.
        let timeout = gateway_revision_hook_timeout_seconds(args.timeout_seconds);
        let hook_args = vec![
            timeout.to_string(),
            hook.display().to_string(),
            "--context".to_owned(),
            args.context.clone().unwrap_or_default(),
            "--namespace".to_owned(),
            args.namespace.clone(),
            "--expected-revision".to_owned(),
            grid.distributed_revision.clone(),
            "--config-map-namespace".to_owned(),
            grid.config_map_namespace.clone(),
            "--config-map".to_owned(),
            grid.config_map_name.clone(),
            "--evidence-dir".to_owned(),
            evidence_dir.display().to_string(),
            "--http-route-name".to_owned(),
            route.name.clone(),
            "--grid-gateway-service".to_owned(),
            format!("{}/{}", service.namespace, service.name),
            "--phase".to_owned(),
            phase.to_owned(),
        ];
        let capture = run_command_owned("timeout", &hook_args)?;
        write_capture(
            evidence_dir,
            &format!("gateway-revision-hook-{phase}-{attempt}"),
            &capture,
        )?;
        if capture.success()
            && let Ok(value) = serde_json::from_str::<Value>(&capture.stdout())
            && let Ok(observation) = serde_json::from_value::<GatewayRevisionHookObservation>(value)
        {
            let valid = observation.revision == grid.distributed_revision
                && !observation.pod_uid.is_empty()
                && !observation.proxy_pod_uid.is_empty()
                && api_access_acceptable(args, observation.api_ca_verified)
                && observation.route_retry_limit <= 2
                && observation.route_backend_count == 1
                && observation.route_cluster_sha256.len() == 64
                && observation
                    .route_cluster_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && observation.route_backend_service_matches
                && observation.serving_config_digest.len() == 64
                && observation
                    .serving_config_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                && observation.mounted_serving_config_matches
                && observation.overlay_candidate_identities_match
                && observation.overlay_candidate_count == 2
                && observation.request_counter_present
                && observation.counter_source == "envoy_clusters_per_host_rq_total"
                && (phase.ends_with("-after-request") || observation.retry_attempts.is_none_or(|count| count == 0));
            if valid {
                let record = GatewayRevisionRecord {
                    phase: phase.to_owned(),
                    revision: observation.revision,
                    pod_uid: observation.pod_uid,
                    proxy_pod_uid: observation.proxy_pod_uid,
                    api_ca_verified: observation.api_ca_verified,
                    route_retry_limit: observation.route_retry_limit,
                    route_backend_count: observation.route_backend_count,
                    route_cluster_sha256: observation.route_cluster_sha256,
                    route_backend_service_matches: observation.route_backend_service_matches,
                    serving_config_digest: observation.serving_config_digest,
                    mounted_serving_config_matches: observation.mounted_serving_config_matches,
                    overlay_candidate_identities_match: observation.overlay_candidate_identities_match,
                    overlay_candidate_count: observation.overlay_candidate_count,
                    retry_attempts: observation.retry_attempts,
                    retry_counter_present: observation.retry_counter_present,
                    upstream_requests: observation.upstream_requests,
                    request_counter_present: observation.request_counter_present,
                    counter_source: observation.counter_source,
                    observed_epoch_seconds: epoch_seconds(),
                };
                let file_name = format!("gateway-revision-{phase}.json");
                write_json_private(&evidence_dir.join(file_name), &record)?;
                report.gateway_revisions.push(record.clone());
                record_boolean_check(
                    report,
                    &format!("gateway/{phase}-revision-consumed"),
                    true,
                    format!(
                        "serving Gateway consumed {}, serving digest={} (mounted match={}), overlay candidate identity match={} (count={}), task backend match={}, route cluster hash recorded, hook API CA verified={}, runner API mode={}, one backend={}, retry limit={}, retry counter present={}, retry attempts={:?}, upstream attempts={} (counter source={}, request counter present={}), Gateway and proxy Pod identities recorded",
                        record.revision,
                        record.serving_config_digest,
                        record.mounted_serving_config_matches,
                        record.overlay_candidate_identities_match,
                        record.overlay_candidate_count,
                        record.route_backend_service_matches,
                        record.api_ca_verified,
                        report.api_tls_mode,
                        record.route_backend_count,
                        record.route_retry_limit,
                        record.retry_counter_present,
                        record.retry_attempts,
                        record.upstream_requests,
                        record.counter_source,
                        record.request_counter_present
                    ),
                );
                return Ok(record);
            }
        }
        std::thread::sleep(Duration::from_secs(args.poll_interval_seconds.max(1)));
    }
    add_blocked_check(
        report,
        &format!("gateway/{phase}-revision-consumed"),
        &format!(
            "serving Gateway did not report distributed revision {} before timeout",
            grid.distributed_revision
        ),
    );
    Err("timed out waiting for the serving Gateway to consume the Grid revision with a single task backend and zero observed retries".to_owned())
}

fn gateway_revision_hook_timeout_seconds(overall_timeout_seconds: u64) -> u64 {
    overall_timeout_seconds.clamp(1, GATEWAY_REVISION_HOOK_TIMEOUT_CAP_SECONDS)
}

fn gateway_retry_attribution(
    before: &GatewayRevisionRecord,
    after: &GatewayRevisionRecord,
) -> (bool, Option<u64>, Option<u64>) {
    let upstream_request_delta = after.upstream_requests.checked_sub(before.upstream_requests);
    let retry_attempt_delta = after
        .retry_attempts
        .zip(before.retry_attempts)
        .and_then(|(after, before)| after.checked_sub(before));
    let same_instances = before.pod_uid == after.pod_uid
        && before.proxy_pod_uid == after.proxy_pod_uid
        && before.route_cluster_sha256 == after.route_cluster_sha256;
    let no_retry = if before.retry_counter_present && after.retry_counter_present {
        retry_attempt_delta == Some(0)
    } else {
        // Envoy's /clusters endpoint exposes per-host rq_total even when the
        // stats matcher hides cluster.upstream_rq_total. Exactly one upstream
        // attempt for the unique request proves that no retry occurred.
        !before.retry_counter_present && !after.retry_counter_present
    };
    (
        same_instances && upstream_request_delta == Some(1) && no_retry,
        upstream_request_delta,
        retry_attempt_delta,
    )
}

fn gate_resource_ref(args: &GridLlmdArgs, name: &str) -> Result<ResourceRef, String> {
    let raw = args
        .gate_resources
        .iter()
        .find(|value| value.starts_with(&format!("{name}=")))
        .ok_or_else(|| format!("missing {name} integration gate"))?;
    let (_, resource) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid gate value {raw:?}"))?;
    let (resource, _) = resource.split_once('#').map_or((resource, None), |(r, c)| (r, Some(c)));
    parse_resource_ref(resource, &args.namespace)
}

fn gate_gateway_name(args: &GridLlmdArgs) -> Result<String, String> {
    if let Some(name) = args.grid_gateway_name.as_deref().filter(|name| !name.is_empty()) {
        return Ok(name.to_owned());
    }
    let gateway = gate_resource_ref(args, "gateway")?;
    if !is_resource_kind(&gateway.kind, &["gateway", "gateways"]) {
        return Err("gateway gate must reference a Gateway API resource".to_owned());
    }
    Ok(gateway.name)
}

fn verify_pool_attribution(args: &GridLlmdArgs, report: &mut Report, baseline: &Probe, current: &Probe, phase: &str) {
    let expected = configured_pool_attribution_for_name(args, &current.expected_pool).unwrap_or_default();
    let baseline_expected = configured_pool_attribution_for_name(args, &baseline.expected_pool).unwrap_or_default();
    let current_ok = response_attribution_matches(
        current.provider_attribution.as_deref(),
        current.backend_attribution.as_deref(),
        expected,
    );
    let baseline_ok = response_attribution_matches(
        baseline.provider_attribution.as_deref(),
        baseline.backend_attribution.as_deref(),
        baseline_expected,
    );
    let route_transition_ok = match phase {
        "pressure" => current.expected_pool != baseline.expected_pool,
        _ => current.expected_pool == baseline.expected_pool,
    };
    let passed = current_ok && baseline_ok && route_transition_ok;
    record_boolean_check(
        report,
        &format!("route/{phase}-pool-attribution"),
        passed,
        format!(
            "overlay-ranked expected pool={}, baseline pool={}, provider/site={:?}, backend/cluster={:?}, actual provider/site={:?}, backend/cluster={:?}, transition matches phase={route_transition_ok}",
            current.expected_pool,
            baseline.expected_pool,
            expected.0,
            expected.1,
            current.provider_attribution,
            current.backend_attribution
        ),
    );
}

fn configured_pool_attribution<'a>(
    provider: Option<&'a str>,
    backend: Option<&'a str>,
    legacy: Option<&'a str>,
) -> Option<(&'a str, &'a str)> {
    let provider = provider
        .filter(|value| !value.is_empty())
        .or_else(|| legacy.filter(|value| !value.is_empty()))?;
    let backend = backend
        .filter(|value| !value.is_empty())
        .or_else(|| legacy.filter(|value| !value.is_empty()))?;
    Some((provider, backend))
}

fn response_attribution_matches(
    actual_provider: Option<&str>,
    actual_backend: Option<&str>,
    expected: (&str, &str),
) -> bool {
    actual_provider == Some(expected.0) && actual_backend == Some(expected.1)
}

fn collect(args: &GridLlmdArgs, report: &mut Report, evidence_dir: &Path) -> Result<(), String> {
    let context = args.context.clone().unwrap_or_default();
    collect_snapshots(
        report,
        evidence_dir,
        &context,
        &args.namespace,
        &args.deployments,
        args.grid_operator_deployment.as_deref(),
    )
}

fn check_cluster_capacity(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let nodes_capture = run_command("kubectl", &["--context", context, "get", "nodes", "-o", "json"])?;
    if !nodes_capture.success() {
        return Err(format!("read node capacity: {}", nodes_capture.stderr()));
    }
    let nodes: Value =
        serde_json::from_str(&nodes_capture.stdout()).map_err(|error| format!("parse node inventory: {error}"))?;
    let mut node_records = Vec::new();
    let mut ready_workers = 0_usize;
    let mut qualified_workers = 0_usize;
    let mut total_allocatable_worker_cpu_milli = 0_u64;
    let mut incomplete_worker_capacity = 0_usize;
    for node in nodes.get("items").and_then(Value::as_array).into_iter().flatten() {
        let labels = node.get("metadata").and_then(|value| value.get("labels"));
        let worker = labels.is_some_and(|value| value.get("node-role.kubernetes.io/worker").is_some());
        let control_plane = labels.is_some_and(|value| {
            value.get("node-role.kubernetes.io/control-plane").is_some()
                || value.get("node-role.kubernetes.io/master").is_some()
        });
        let ready = node
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            });
        let allocatable = node.pointer("/status/allocatable");
        let cpu = allocatable
            .and_then(|value| value.get("cpu"))
            .and_then(Value::as_str)
            .and_then(parse_cpu_millis);
        let memory = allocatable
            .and_then(|value| value.get("memory"))
            .and_then(Value::as_str)
            .and_then(parse_memory_bytes);
        let name = node
            .pointer("/metadata/name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let instance_type = labels
            .and_then(|value| value.get("node.kubernetes.io/instance-type"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let taints = node
            .pointer("/spec/taints")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        if worker {
            if ready {
                ready_workers += 1;
                total_allocatable_worker_cpu_milli =
                    total_allocatable_worker_cpu_milli.saturating_add(cpu.unwrap_or_default());
            }
            if ready
                && cpu.is_some_and(|value| value >= 8_000)
                && memory.is_some_and(|value| value >= 32 * 1024 * 1024 * 1024)
            {
                qualified_workers += 1;
            }
            if cpu.is_none() || memory.is_none() {
                incomplete_worker_capacity += 1;
            }
        }
        node_records.push(serde_json::json!({
            "name": name,
            "role": if worker { "worker" } else if control_plane { "control-plane" } else { "other" },
            "instanceType": instance_type,
            "ready": ready,
            "allocatableCpuMilli": cpu,
            "allocatableMemoryBytes": memory,
            "taints": taints,
        }));
    }
    let pending_capture = run_command(
        "kubectl",
        &[
            "--context",
            context,
            "get",
            "pods",
            "--all-namespaces",
            "--field-selector=status.phase=Pending",
            "-o",
            "json",
        ],
    )?;
    if !pending_capture.success() {
        return Err(format!("read Pending Pods: {}", pending_capture.stderr()));
    }
    let pending: Value = serde_json::from_str(&pending_capture.stdout())
        .map_err(|error| format!("parse Pending Pod inventory: {error}"))?;
    let pending_records = pending
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|pod| {
            serde_json::json!({
                "namespace": pod.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or("unknown"),
                "name": pod.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("unknown"),
                "node": pod.pointer("/spec/nodeName").and_then(Value::as_str),
                "reason": pod.pointer("/status/reason").and_then(Value::as_str),
                "message": pod.pointer("/status/message").and_then(Value::as_str),
            })
        })
        .collect::<Vec<_>>();
    let pending_count = pending_records.len();
    let control_plane_nodes_no_schedule_tainted = node_records
        .iter()
        .filter(|node| node["role"] == "control-plane")
        .all(|node| {
            node["taints"].as_array().is_some_and(|taints| {
                taints
                    .iter()
                    .any(|taint| taint.get("effect").and_then(Value::as_str) == Some("NoSchedule"))
            })
        });
    let accepted = capacity_preflight_allowed(
        args.resource_profile.is_reduced_dev(),
        ready_workers,
        qualified_workers,
        total_allocatable_worker_cpu_milli,
        incomplete_worker_capacity,
        pending_count,
        control_plane_nodes_no_schedule_tainted,
    );
    let disposition = if args.resource_profile.is_reduced_dev() {
        "WAIVED_FOR_EPHEMERAL_DEV_SMOKE"
    } else {
        "REQUIRED"
    };
    write_json_private(
        &evidence_dir.join("capacity-snapshot.json"),
        &serde_json::json!({
            "capacityBaseline": disposition,
            "resourceProfile": args.resource_profile.as_str(),
            "resultLabel": report.result_label,
            "fullBaselineRequirement": {
                "readyWorkers":2,
                "workersMeetingPerNodeBaseline":2,
                "cpuCoresPerWorker":8,
                "memoryGiBPerWorker":32,
                "totalAllocatableWorkerCpuCores":20
            },
            "observed": {
                "readyWorkers":ready_workers,
                "workersMeetingFullBaseline":qualified_workers,
                "totalAllocatableWorkerCpuMilli":total_allocatable_worker_cpu_milli,
                "incompleteWorkerCapacity":incomplete_worker_capacity,
                "pendingPods":pending_records,
                "controlPlaneNodesNoScheduleTainted":control_plane_nodes_no_schedule_tainted
            },
            "nodes":node_records,
        }),
    )?;
    report.checks.push(Check {
        name: if args.resource_profile.is_reduced_dev() { "capacity-readiness" } else { "capacity-baseline" }.to_owned(),
        status: if accepted { "pass" } else { "blocked" }.to_owned(),
        detail: if args.resource_profile.is_reduced_dev() {
            format!("DEV-SMOKE / REDUCED-CAPACITY; capacity-baseline=WAIVED_FOR_EPHEMERAL_DEV_SMOKE; {qualified_workers}/2 workers meet the strict per-node baseline; {total_allocatable_worker_cpu_milli}m aggregate allocatable worker CPU; {ready_workers} Ready workers; {pending_count} Pending Pods; control-plane NoSchedule taints verified={control_plane_nodes_no_schedule_tainted}; pending and worker readiness gates remain enforced")
        } else {
            format!("full baseline requires at least 2 Ready workers, at least 2 workers with 8 CPU/32GiB allocatable each, and at least 20000m aggregate allocatable worker CPU; {qualified_workers}/2 meet per-node baseline, {total_allocatable_worker_cpu_milli}m aggregate CPU, {ready_workers} workers Ready, {pending_count} Pending Pods, control-plane NoSchedule taints verified={control_plane_nodes_no_schedule_tainted}")
        },
        observed_epoch_seconds: epoch_seconds(),
    });
    Ok(())
}

fn parse_cpu_millis(value: &str) -> Option<u64> {
    if let Some(millis) = value.strip_suffix('m') {
        if millis.is_empty() || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        return millis.parse().ok();
    }
    let (whole, fractional) = value
        .split_once('.')
        .map_or((value, None), |(whole, fractional)| (whole, Some(fractional)));
    if (!whole.is_empty() && !whole.bytes().all(|byte| byte.is_ascii_digit()))
        || (whole.is_empty() && fractional.is_none())
    {
        return None;
    }
    let whole_cores = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().ok()?
    };
    let mut milli_cores = whole_cores.checked_mul(1000)?;
    if let Some(fractional) = fractional {
        if fractional.is_empty() || fractional.len() > 3 || !fractional.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let decimal_places = u32::try_from(3 - fractional.len()).ok()?;
        let fractional_millis = fractional.parse::<u64>().ok()? * 10_u64.pow(decimal_places);
        milli_cores = milli_cores.checked_add(fractional_millis)?;
    }
    Some(milli_cores)
}

fn parse_memory_bytes(value: &str) -> Option<u64> {
    for (suffix, multiplier) in [
        ("Ki", 1_u64 << 10),
        ("Mi", 1_u64 << 20),
        ("Gi", 1_u64 << 30),
        ("Ti", 1_u64 << 40),
        ("Pi", 1_u64 << 50),
        ("Ei", 1_u64 << 60),
        ("K", 1_000_u64),
        ("M", 1_000_000_u64),
        ("G", 1_000_000_000_u64),
        ("T", 1_000_000_000_000_u64),
        ("P", 1_000_000_000_000_000_u64),
        ("E", 1_000_000_000_000_000_000_u64),
    ] {
        if let Some(amount) = value.strip_suffix(suffix) {
            return amount.parse::<u64>().ok()?.checked_mul(multiplier);
        }
    }
    value.parse().ok()
}

fn check_kubectl(report: &mut Report, evidence_dir: &Path, context: &str) -> Result<(), String> {
    let capture = run_command("kubectl", &["--context", context, "version", "-o", "yaml"])?;
    write_capture(evidence_dir, "kubectl-version", &capture)?;
    report.checks.push(Check {
        name: "kubectl".to_owned(),
        status: if capture.success() { "pass" } else { "blocked" }.to_owned(),
        detail: format!("exit status {}", capture.status()),
        observed_epoch_seconds: epoch_seconds(),
    });
    Ok(())
}

fn check_openshift_prerequisites(report: &mut Report, evidence_dir: &Path, context: &str) -> Result<(), String> {
    let readyz = run_command("kubectl", &["--context", context, "get", "--raw=/readyz"])?;
    write_capture(evidence_dir, "api-readyz", &readyz)?;
    let readyz_ok = readyz.success() && readyz.stdout().trim() == "ok";
    report.checks.push(Check {
        name: "api-readyz".to_owned(),
        status: if readyz_ok { "pass" } else { "blocked" }.to_owned(),
        detail: if readyz_ok {
            "Kubernetes API /readyz returned ok".to_owned()
        } else {
            format!(
                "Kubernetes API /readyz did not return ok; exit status {}",
                readyz.status()
            )
        },
        observed_epoch_seconds: epoch_seconds(),
    });

    let version = run_command(
        "kubectl",
        &["--context", context, "get", "clusterversion", "version", "-o", "json"],
    )?;
    write_capture(evidence_dir, "cluster-version", &version)?;
    if version.success() {
        let document = parse_capture_json(&version, "ClusterVersion")?;
        let conditions = document.pointer("/status/conditions").and_then(Value::as_array);
        let healthy = resource_conditions_match(
            &document,
            &[("Available", "True"), ("Progressing", "False"), ("Failing", "False")],
        );
        write_json_private(
            &evidence_dir.join("cluster-version-health.json"),
            &serde_json::json!({
                "name": document.pointer("/metadata/name").and_then(Value::as_str),
                "version": document.pointer("/status/history/0/version").and_then(Value::as_str),
                "healthy": healthy,
                "conditions": conditions.map(|conditions| conditions.iter().filter_map(|condition| {
                    Some(serde_json::json!({
                        "type":condition.get("type")?.as_str()?,
                        "status":condition.get("status")?.as_str()?,
                        "reason":condition.get("reason").and_then(Value::as_str),
                    }))
                }).collect::<Vec<_>>()).unwrap_or_default(),
            }),
        )?;
        report.checks.push(Check {
            name: "cluster-version-health".to_owned(),
            status: if healthy { "pass" } else { "blocked" }.to_owned(),
            detail: if healthy {
                "ClusterVersion is Available, not Progressing, and not Failing".to_owned()
            } else {
                "ClusterVersion did not meet Available=True, Progressing=False, Failing=False".to_owned()
            },
            observed_epoch_seconds: epoch_seconds(),
        });
    } else {
        report.checks.push(Check {
            name: "cluster-version-health".to_owned(),
            status: "blocked".to_owned(),
            detail: format!("read ClusterVersion: exit status {}", version.status()),
            observed_epoch_seconds: epoch_seconds(),
        });
    }

    let operators = run_command(
        "kubectl",
        &["--context", context, "get", "clusteroperators", "-o", "json"],
    )?;
    write_capture(evidence_dir, "cluster-operators", &operators)?;
    if operators.success() {
        let document = parse_capture_json(&operators, "ClusterOperators")?;
        let items = document.get("items").and_then(Value::as_array);
        let healthy = cluster_operators_are_healthy(&document);
        let summary = items
            .into_iter()
            .flatten()
            .map(|operator| {
                let conditions = operator.pointer("/status/conditions").and_then(Value::as_array);
                serde_json::json!({
                    "name":operator.pointer("/metadata/name").and_then(Value::as_str),
                    "healthy":cluster_operator_is_healthy(operator),
                    "conditions":conditions.into_iter().flatten().filter_map(|condition| {
                        Some(serde_json::json!({
                            "type":condition.get("type")?.as_str()?,
                            "status":condition.get("status")?.as_str()?,
                            "reason":condition.get("reason").and_then(Value::as_str),
                        }))
                    }).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        write_json_private(
            &evidence_dir.join("cluster-operator-health.json"),
            &serde_json::json!({"healthy":healthy,"operators":summary}),
        )?;
        report.checks.push(Check {
            name: "cluster-operators-healthy".to_owned(),
            status: if healthy { "pass" } else { "blocked" }.to_owned(),
            detail: format!(
                "{} ClusterOperators; all must be Available=True, Progressing=False, Degraded=False",
                summary.len()
            ),
            observed_epoch_seconds: epoch_seconds(),
        });
    } else {
        report.checks.push(Check {
            name: "cluster-operators-healthy".to_owned(),
            status: "blocked".to_owned(),
            detail: format!("read ClusterOperators: exit status {}", operators.status()),
            observed_epoch_seconds: epoch_seconds(),
        });
    }

    let storage = run_command(
        "kubectl",
        &["--context", context, "get", "storageclasses", "-o", "json"],
    )?;
    write_capture(evidence_dir, "storage-classes", &storage)?;
    if storage.success() {
        let document = parse_capture_json(&storage, "StorageClasses")?;
        let items = document.get("items").and_then(Value::as_array);
        let defaults = items
            .into_iter()
            .flatten()
            .filter(|class| is_default_dynamic_storage_class(class))
            .map(|class| {
                serde_json::json!({
                    "name":class.pointer("/metadata/name").and_then(Value::as_str),
                    "provisioner":class.pointer("/provisioner").and_then(Value::as_str),
                })
            })
            .collect::<Vec<_>>();
        write_json_private(&evidence_dir.join("default-dynamic-storage-classes.json"), &defaults)?;
        report.checks.push(Check {
            name: "default-dynamic-storage-class".to_owned(),
            status: if defaults.is_empty() { "blocked" } else { "pass" }.to_owned(),
            detail: if defaults.is_empty() {
                "no default StorageClass with a dynamic provisioner is available".to_owned()
            } else {
                format!(
                    "{} default StorageClass(es) with dynamic provisioners are available",
                    defaults.len()
                )
            },
            observed_epoch_seconds: epoch_seconds(),
        });
    } else {
        report.checks.push(Check {
            name: "default-dynamic-storage-class".to_owned(),
            status: "blocked".to_owned(),
            detail: format!("read StorageClasses: exit status {}", storage.status()),
            observed_epoch_seconds: epoch_seconds(),
        });
    }
    Ok(())
}

fn resource_conditions_match(resource: &Value, expected: &[(&str, &str)]) -> bool {
    resource
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            expected.iter().all(|(kind, status)| {
                conditions.iter().any(|condition| {
                    condition.get("type").and_then(Value::as_str) == Some(kind)
                        && condition.get("status").and_then(Value::as_str) == Some(status)
                })
            })
        })
}

fn cluster_operator_is_healthy(operator: &Value) -> bool {
    resource_conditions_match(
        operator,
        &[("Available", "True"), ("Progressing", "False"), ("Degraded", "False")],
    )
}

fn cluster_operators_are_healthy(document: &Value) -> bool {
    document
        .get("items")
        .and_then(Value::as_array)
        .is_some_and(|operators| !operators.is_empty() && operators.iter().all(cluster_operator_is_healthy))
}

fn is_default_dynamic_storage_class(storage_class: &Value) -> bool {
    let annotations = storage_class.pointer("/metadata/annotations");
    let is_default = annotations
        .and_then(|value| value.get("storageclass.kubernetes.io/is-default-class"))
        .and_then(Value::as_str)
        == Some("true")
        || annotations
            .and_then(|value| value.get("storageclass.beta.kubernetes.io/is-default-class"))
            .and_then(Value::as_str)
            == Some("true");
    is_default
        && storage_class
            .pointer("/provisioner")
            .and_then(Value::as_str)
            .is_some_and(|p| p != "kubernetes.io/no-provisioner")
}

/// Read one Kubernetes object as JSON and retain the raw command output.
fn get_resource(
    evidence_dir: &Path,
    context: &str,
    namespace: Option<&str>,
    kind: &str,
    name: &str,
    evidence_name: &str,
) -> Result<CommandCapture, String> {
    let mut args = vec!["--context".to_owned(), context.to_owned()];
    if let Some(namespace) = namespace {
        args.extend(["--namespace".to_owned(), namespace.to_owned()]);
    }
    args.extend([
        "get".to_owned(),
        kind.to_owned(),
        name.to_owned(),
        "-o".to_owned(),
        "json".to_owned(),
    ]);
    let capture = run_command_owned("kubectl", &args)?;
    write_capture(evidence_dir, evidence_name, &capture)?;
    Ok(capture)
}

fn check_resource(
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    namespace: Option<&str>,
    kind: &str,
    name: &str,
    label: &str,
    condition: Option<&str>,
) -> Result<(), String> {
    let mut args = vec!["--context".to_owned(), context.to_owned()];
    if let Some(namespace) = namespace {
        args.extend(["--namespace".to_owned(), namespace.to_owned()]);
    }
    args.extend([
        "get".to_owned(),
        kind.to_owned(),
        name.to_owned(),
        "-o".to_owned(),
        "json".to_owned(),
    ]);
    let capture = run_command_owned("kubectl", &args)?;
    write_capture(evidence_dir, &format!("resource-{}", safe_name(label)), &capture)?;
    let (passed, detail) = if capture.success() {
        condition.map_or_else(
            || (true, "resource exists".to_owned()),
            |condition| resource_condition(&capture.stdout(), condition),
        )
    } else {
        (false, capture.stderr().trim().to_owned())
    };
    report.checks.push(Check {
        name: label.to_owned(),
        status: if passed { "pass" } else { "blocked" }.to_owned(),
        detail,
        observed_epoch_seconds: epoch_seconds(),
    });
    Ok(())
}

#[derive(Clone, Debug)]
struct ResourceRef {
    namespace: String,
    kind: String,
    name: String,
}

struct DeploymentRef {
    namespace: String,
    name: String,
}

fn parse_deployment_ref(value: &str, default_namespace: &str) -> Result<DeploymentRef, String> {
    match value.split('/').collect::<Vec<_>>().as_slice() {
        [name] if !name.is_empty() => Ok(DeploymentRef {
            namespace: default_namespace.to_owned(),
            name: (*name).to_owned(),
        }),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => Ok(DeploymentRef {
            namespace: (*namespace).to_owned(),
            name: (*name).to_owned(),
        }),
        _ => Err(format!(
            "invalid deployment reference {value:?}; expected name or namespace/name"
        )),
    }
}

fn parse_resource_ref(value: &str, default_namespace: &str) -> Result<ResourceRef, String> {
    let parts = value.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [kind, name] => Ok(ResourceRef {
            namespace: default_namespace.to_owned(),
            kind: (*kind).to_owned(),
            name: (*name).to_owned(),
        }),
        [namespace, kind, name] => Ok(ResourceRef {
            namespace: (*namespace).to_owned(),
            kind: (*kind).to_owned(),
            name: (*name).to_owned(),
        }),
        _ => Err(format!(
            "invalid resource reference {value:?}; expected kind/name or namespace/kind/name"
        )),
    }
}

fn check_integration_gates(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let required_gates = args.qualification_scope.required_gates();
    if args.gate_resources.is_empty() {
        return Err(format!(
            "non-plan modes require --gate-resource values for scope {}: {}",
            args.qualification_scope.as_str(),
            required_gates.join(", ")
        ));
    }
    let mut seen = Vec::new();
    let namespace = report.namespace.clone();
    for raw in &args.gate_resources {
        let (gate, resource) = raw
            .split_once('=')
            .ok_or_else(|| format!("invalid --gate-resource {raw:?}; expected gate=resource/name#condition"))?;
        let (resource, condition) = resource
            .split_once('#')
            .map_or((resource, None), |(name, condition)| (name, Some(condition)));
        let resource_ref = parse_resource_ref(resource, &namespace)?;
        let kind = resource_ref.kind.as_str();
        let name = resource_ref.name.as_str();
        if !FULL_REQUIRED_GATES.contains(&gate) {
            return Err(format!(
                "unknown integration gate {gate:?}; expected {}",
                FULL_REQUIRED_GATES.join(", ")
            ));
        }
        validate_gate_resource_contract(gate, kind, condition)?;
        if seen.iter().any(|seen_gate| seen_gate == &gate) {
            return Err(format!("duplicate --gate-resource for {gate:?}"));
        }
        seen.push(gate);
        let resource_namespace = if kind == "crd" || is_cluster_scoped_gate_resource_kind(kind) {
            None
        } else {
            Some(resource_ref.namespace.as_str())
        };
        check_resource(
            report,
            evidence_dir,
            context,
            resource_namespace,
            kind,
            name,
            &format!("gate/{gate}"),
            condition,
        )?;
    }
    for required in required_gates {
        if !seen.contains(required) {
            report.checks.push(Check {
                name: format!("gate/{required}"),
                status: "fail".to_owned(),
                detail: "no resource was supplied for this integration gate".to_owned(),
                observed_epoch_seconds: epoch_seconds(),
            });
        }
    }
    Ok(())
}

fn is_cluster_scoped_gate_resource_kind(kind: &str) -> bool {
    let short_kind = kind.split('.').next().unwrap_or(kind);
    is_resource_kind(short_kind, &["datasciencecluster", "datascienceclusters"])
}

fn validate_gate_resource_contract(gate: &str, kind: &str, condition: Option<&str>) -> Result<(), String> {
    match gate {
        "rhoai" if !is_resource_kind(kind, &["datasciencecluster", "datascienceclusters"]) => {
            Err("the RHOAI gate must identify a DataScienceCluster with #Ready".to_owned())
        },
        "rhoai" if condition != Some("Ready") => {
            Err("the RHOAI gate must use the current DataScienceCluster #Ready condition".to_owned())
        },
        "maas"
            if !is_resource_kind(
                kind,
                &[
                    "aitenant",
                    "aitenants",
                    "maastenantconfig",
                    "maastenantconfigs",
                    "maasconfig",
                    "maasconfigs",
                    "config.maas.opendatahub.io",
                    "configs.maas.opendatahub.io",
                ],
            ) || condition != Some("Ready") =>
        {
            Err(
                "the MaaS gate must identify a MaaS Config, MaaSTenantConfig, or AITenant with the current #Ready condition; use a separate RHOAI gate for DataScienceCluster and do not label AIGateway as MaaS".to_owned(),
            )
        }
        "grid" if !is_resource_kind(kind, &["gridnetwork", "gridnetworks"]) => Err(format!(
            "the grid gate must name a GridNetwork, not {kind:?}; use grid=gridnetwork/<name>#exists"
        )),
        "enrollment" if !is_resource_kind(kind, &["siteinvite", "siteinvites"]) || condition != Some("Ready") => {
            Err("the enrollment gate must identify a SiteInvite with #Ready".to_owned())
        },
        "gateway" if !is_resource_kind(kind, &["gateway", "gateways"]) || condition != Some("Programmed") => {
            Err("the Gateway gate must identify a Gateway with #Programmed".to_owned())
        },
        "llminferenceservice"
            if !is_resource_kind(
                kind,
                &[
                    "llminferenceservice",
                    "llminferenceservices",
                    "inferencepool",
                    "inferencepools",
                ],
            ) =>
        {
            Err(format!(
                "the llminferenceservice gate must name an LLMInferenceService or InferencePool, not {kind:?}"
            ))
        },
        _ => Ok(()),
    }
}

fn check_enrollment_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let (Some(site_raw), Some(identity_secret), Some(ca_secret), Some(expected_san)) = (
        args.grid_site_resource.as_deref(),
        args.site_identity_secret.as_deref(),
        args.grid_ca_secret.as_deref(),
        args.expected_site_san.as_deref(),
    ) else {
        add_blocked_check(
            report,
            "enrollment/identity",
            "a GridSite, site identity Secret, Grid CA Secret, and expected SAN are required",
        );
        return Ok(());
    };
    let site_ref = parse_resource_ref(site_raw, &args.namespace)?;
    if !is_resource_kind(&site_ref.kind, &["gridsite", "gridsites"]) {
        return Err("--grid-site-resource must identify a GridSite".to_owned());
    }
    let site = get_resource(
        evidence_dir,
        context,
        Some(&site_ref.namespace),
        &site_ref.kind,
        &site_ref.name,
        "enrollment-grid-site",
    )?;
    if !site.success() {
        add_blocked_check(report, "enrollment/grid-site", "enrolled GridSite could not be read");
        return Ok(());
    }
    let site_json = parse_capture_json(&site, "GridSite")?;
    let active = site_json["status"]["phase"].as_str() == Some("Active")
        && site_json["status"]["reason"].as_str() == Some("TlsVerified");
    report.checks.push(Check {
        name: "enrollment/grid-site".to_owned(),
        status: if active { "pass" } else { "blocked" }.to_owned(),
        detail: format!(
            "GridSite phase={:?}, reason={:?}; Active/TlsVerified required",
            site_json["status"]["phase"].as_str(),
            site_json["status"]["reason"].as_str()
        ),
        observed_epoch_seconds: epoch_seconds(),
    });
    let (identity_namespace, identity_name) = parse_namespace_name(identity_secret, &args.namespace)?;
    let (ca_namespace, ca_name) = parse_namespace_name(ca_secret, &args.namespace)?;
    let identity = read_secret_json(context, &identity_namespace, &identity_name)?;
    let ca = read_secret_json(context, &ca_namespace, &ca_name)?;
    let identity_keys = secret_key_names(&identity);
    let ca_keys = secret_key_names(&ca);
    let encoded_certificate = identity["data"]["tls.crt"].as_str().unwrap_or_default();
    let encoded_private_key = identity["data"]["tls.key"].as_str().unwrap_or_default();
    let encoded_ca = ca["data"]["ca.crt"].as_str().unwrap_or_default();
    let certificate_sans = decode_certificate_sans(encoded_certificate)?;
    let san_matches = certificate_sans.iter().any(|san| san == expected_san);
    let (certificate_key_match, certificate_chain_valid) =
        verify_enrollment_certificate_material(encoded_certificate, encoded_private_key, encoded_ca);
    let identity_ready = identity["metadata"]["name"].as_str() == Some(identity_name.as_str())
        && identity_keys.iter().any(|key| key == "tls.crt")
        && identity_keys.iter().any(|key| key == "tls.key")
        && san_matches
        && certificate_key_match;
    let ca_ready = ca["metadata"]["name"].as_str() == Some(ca_name.as_str())
        && ca_keys.iter().any(|key| key == "ca.crt")
        && certificate_chain_valid;
    let safe = serde_json::json!({
        "gridSite": format!("{}/{}", site_ref.namespace, site_ref.name),
        "identitySecret": format!("{identity_namespace}/{identity_name}"),
        "identityKeyNames": identity_keys,
        "identityCertificateSans": certificate_sans,
        "identityCertificateMatchesPrivateKey": certificate_key_match,
        "identityChainVerifiesAgainstGridCa": certificate_chain_valid,
        "identityValuesCaptured": false,
        "expectedSanMatched": san_matches,
        "caSecret": format!("{ca_namespace}/{ca_name}"),
        "caKeyNames": ca_keys,
        "caValuesCaptured": false,
    });
    write_json_private(&evidence_dir.join("enrollment-identity-shape.json"), &safe)?;
    record_boolean_check(
        report,
        "enrollment/identity",
        identity_ready && ca_ready,
        format!(
            "identity key match/SAN={identity_ready}; certificate verifies against Grid CA={certificate_chain_valid}; private key and CA values omitted"
        ),
    );
    Ok(())
}

fn parse_namespace_name(value: &str, default_namespace: &str) -> Result<(String, String), String> {
    let parts = value.split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        [name] if !name.is_empty() => Ok((default_namespace.to_owned(), (*name).to_owned())),
        [namespace, name] if !namespace.is_empty() && !name.is_empty() => {
            Ok(((*namespace).to_owned(), (*name).to_owned()))
        },
        _ => Err(format!(
            "invalid namespaced Secret reference {value:?}; expected name or namespace/name"
        )),
    }
}

fn read_secret_json(context: &str, namespace: &str, name: &str) -> Result<Value, String> {
    let capture = run_command(
        "kubectl",
        &[
            "--context",
            context,
            "--namespace",
            namespace,
            "get",
            "secret",
            name,
            "-o",
            "json",
        ],
    )?;
    if !capture.success() {
        return Err(format!(
            "required enrollment Secret {namespace}/{name} could not be read"
        ));
    }
    parse_capture_json(&capture, "enrollment Secret")
}

fn secret_key_names(secret: &Value) -> Vec<String> {
    let mut keys = secret["data"]
        .as_object()
        .map(|data| data.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    keys.sort();
    keys
}

fn decode_certificate_sans(encoded: &str) -> Result<Vec<String>, String> {
    if encoded.is_empty() {
        return Ok(Vec::new());
    }
    let pem = STANDARD
        .decode(encoded)
        .map_err(|error| format!("site identity certificate data is not valid base64: {error}"))?;
    let certificate = openssl::x509::X509::from_pem(&pem)
        .map_err(|error| format!("site identity certificate could not be parsed: {error}"))?;
    let names = certificate.subject_alt_names();
    let mut sans = names
        .as_ref()
        .into_iter()
        .flat_map(|stack| stack.iter())
        .filter_map(|name| name.dnsname().or_else(|| name.uri()))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    sans.sort();
    Ok(sans)
}

fn verify_enrollment_certificate_material(
    encoded_certificate: &str,
    encoded_private_key: &str,
    encoded_ca: &str,
) -> (bool, bool) {
    let Ok(certificate_pem) = STANDARD.decode(encoded_certificate) else {
        return (false, false);
    };
    let Ok(private_key_pem) = STANDARD.decode(encoded_private_key) else {
        return (false, false);
    };
    let Ok(ca_pem) = STANDARD.decode(encoded_ca) else {
        return (false, false);
    };
    let Ok(certificate) = openssl::x509::X509::from_pem(&certificate_pem) else {
        return (false, false);
    };
    let Ok(private_key) = openssl::pkey::PKey::private_key_from_pem(&private_key_pem) else {
        return (false, false);
    };
    let certificate_key_match = certificate
        .public_key()
        .is_ok_and(|public_key| public_key.public_eq(&private_key));
    let Ok(ca_certificate) = openssl::x509::X509::from_pem(&ca_pem) else {
        return (certificate_key_match, false);
    };
    let Ok(mut store_builder) = openssl::x509::store::X509StoreBuilder::new() else {
        return (certificate_key_match, false);
    };
    if store_builder.add_cert(ca_certificate).is_err() {
        return (certificate_key_match, false);
    }
    let store = store_builder.build();
    let Ok(chain) = openssl::stack::Stack::new() else {
        return (certificate_key_match, false);
    };
    let Ok(mut context) = openssl::x509::X509StoreContext::new() else {
        return (certificate_key_match, false);
    };
    let chain_valid = context
        .init(
            &store,
            &certificate,
            &chain,
            openssl::x509::X509StoreContextRef::verify_cert,
        )
        .unwrap_or(false);
    (certificate_key_match, chain_valid)
}

fn check_external_model_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let (Some(model_raw), Some(provider_raw)) = (
        args.external_model_resource.as_deref(),
        args.external_provider_resource.as_deref(),
    ) else {
        report.checks.push(Check {
            name: "maas/external-model-scope".to_owned(),
            status: "warn".to_owned(),
            detail: "not run by design: ExternalModel/ExternalProvider translation is out of scope; MaaS is checked only through its platform-readiness gate".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    };
    let model_ref = parse_resource_ref(model_raw, &args.namespace)?;
    let provider_ref = parse_resource_ref(provider_raw, &args.namespace)?;
    if !is_resource_kind(&model_ref.kind, &["externalmodel", "externalmodels"])
        || !is_resource_kind(&provider_ref.kind, &["externalprovider", "externalproviders"])
    {
        return Err("MaaS resources must be ExternalModel and ExternalProvider kinds".to_owned());
    }
    let model_capture = get_resource(
        evidence_dir,
        context,
        Some(&model_ref.namespace),
        &model_ref.kind,
        &model_ref.name,
        "maas-external-model",
    )?;
    if !model_capture.success() {
        add_blocked_check(report, "maas/external-model", "ExternalModel could not be read");
        return Ok(());
    }
    let model = parse_capture_json(&model_capture, "ExternalModel")?;
    check_external_model_status(args, report, &model);
    let Some(provider_binding) = external_provider_binding(&model, &provider_ref) else {
        add_failed_check(
            report,
            "maas/provider-reference",
            "ExternalModel does not reference the expected ExternalProvider",
        );
        return Ok(());
    };
    let provider_capture = get_resource(
        evidence_dir,
        context,
        Some(&provider_ref.namespace),
        &provider_ref.kind,
        &provider_ref.name,
        "maas-external-provider",
    )?;
    if !provider_capture.success() {
        add_blocked_check(report, "maas/external-provider", "ExternalProvider could not be read");
        return Ok(());
    }
    let provider = parse_capture_json(&provider_capture, "ExternalProvider")?;
    check_external_provider_status(report, &provider);
    check_provider_credential_reference(
        report,
        evidence_dir,
        context,
        &model_ref,
        &provider_ref,
        provider_binding,
        &provider,
    )?;
    check_provider_trust(args, report, evidence_dir, context)?;
    check_provider_transport(args, report, evidence_dir, &provider_ref, &provider)?;
    check_external_model_route(args, report, evidence_dir, context, &model_ref, &model)?;
    Ok(())
}

fn check_external_model_status(args: &GridLlmdArgs, report: &mut Report, model: &Value) {
    let generation = model["metadata"]["generation"].as_u64();
    let observed = model["status"]["observedGeneration"].as_u64();
    let phase_ready = model["status"]["phase"].as_str() == Some("Ready")
        || ready_condition(&model["status"]["conditions"], generation);
    let generation_current = generation.is_some() && generation == observed;
    let model_name = model["spec"]["modelName"]
        .as_str()
        .or_else(|| model["metadata"]["name"].as_str())
        .unwrap_or_default();
    let identity_matches = !args.model.is_empty() && model_name == args.model;
    report.checks.push(Check {
        name: "maas/external-model".to_owned(),
        status: if phase_ready && generation_current {
            "pass"
        } else {
            "blocked"
        }
        .to_owned(),
        detail: format!(
            "phase={:?}, observedGeneration={observed:?}, generation={generation:?}",
            model["status"]["phase"].as_str()
        ),
        observed_epoch_seconds: epoch_seconds(),
    });
    let expected_gateway = gate_resource_ref(args, "gateway").ok();
    let spec_gateway = expected_gateway.as_ref().is_some_and(|gateway| {
        model["spec"]["gatewayRefs"].as_array().is_some_and(|refs| {
            refs.iter().any(|reference| {
                reference["name"].as_str() == Some(gateway.name.as_str())
                    && reference["namespace"].as_str() == Some(gateway.namespace.as_str())
            })
        })
    });
    report.checks.push(Check {
        name: "maas/external-model-gateway-binding".to_owned(),
        status: if spec_gateway { "pass" } else { "blocked" }.to_owned(),
        detail: format!("ExternalModel explicitly selects the expected namespaced serving Gateway: {spec_gateway}"),
        observed_epoch_seconds: epoch_seconds(),
    });
    report.checks.push(Check {
        name: "maas/model-identity".to_owned(),
        status: if identity_matches { "pass" } else { "fail" }.to_owned(),
        detail: format!("ExternalModel identity matches request model: {identity_matches}"),
        observed_epoch_seconds: epoch_seconds(),
    });
}

fn external_provider_binding<'a>(model: &'a Value, provider: &ResourceRef) -> Option<&'a Value> {
    model["spec"]["externalProviderRefs"]
        .as_array()?
        .iter()
        .find(|binding| {
            let reference = &binding["ref"];
            let name_matches = reference["name"].as_str() == Some(provider.name.as_str());
            let binding_namespace = reference["namespace"]
                .as_str()
                .unwrap_or_else(|| model["metadata"]["namespace"].as_str().unwrap_or_default());
            name_matches && binding_namespace == provider.namespace
        })
}

fn check_external_provider_status(report: &mut Report, provider: &Value) {
    let generation = provider["metadata"]["generation"].as_u64();
    let observed = provider["status"]["observedGeneration"].as_u64();
    let ready = (provider["status"]["phase"].as_str() == Some("Ready")
        || ready_condition(&provider["status"]["conditions"], generation))
        && generation == observed;
    report.checks.push(Check {
        name: "maas/external-provider".to_owned(),
        status: if ready { "pass" } else { "blocked" }.to_owned(),
        detail: format!(
            "phase={:?}, observedGeneration={observed:?}, generation={generation:?}",
            provider["status"]["phase"].as_str()
        ),
        observed_epoch_seconds: epoch_seconds(),
    });
}

fn ready_condition(conditions: &Value, generation: Option<u64>) -> bool {
    conditions.as_array().is_some_and(|items| {
        items.iter().any(|condition| {
            condition["type"].as_str() == Some("Ready")
                && condition["status"].as_str() == Some("True")
                && generation.is_none_or(|wanted| condition["observedGeneration"].as_u64() == Some(wanted))
        })
    })
}

fn check_provider_trust(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let Some(reference) = args.provider_trust.as_deref() else {
        add_blocked_check(
            report,
            "maas/provider-trust",
            "provider TLS trust source was not configured",
        );
        return Ok(());
    };
    if reference == "system-ca" {
        report.checks.push(Check {
            name: "maas/provider-trust".to_owned(),
            status: "warn".to_owned(),
            detail: "system CA trust was selected; this input alone does not prove the effective provider transport or hostname verification".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
});
        return Ok(());
    }
    let (resource, key) = reference
        .split_once('#')
        .ok_or_else(|| "--provider-trust must be system-ca or kind/namespace/name#key".to_owned())?;
    let parts = resource.split('/').collect::<Vec<_>>();
    let [kind, namespace, name] = parts.as_slice() else {
        return Err("--provider-trust reference must be kind/namespace/name#key".to_owned());
    };
    if !is_resource_kind(kind, &["secret", "secrets", "configmap", "configmaps"]) {
        return Err("provider trust resource must be a Secret or ConfigMap".to_owned());
    }
    let capture = run_command(
        "kubectl",
        &[
            "--context",
            context,
            "--namespace",
            namespace,
            "get",
            kind,
            name,
            "-o",
            "json",
        ],
    )?;
    let (exists, keys) = if capture.success() {
        let object = parse_capture_json(&capture, "provider trust resource")?;
        let keys = object["data"]
            .as_object()
            .map(|data| data.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        (true, keys)
    } else {
        (false, Vec::new())
    };
    let present = exists && keys.iter().any(|candidate| candidate == key);
    let safe = serde_json::json!({
        "kind": kind,
        "namespace": namespace,
        "name": name,
        "exists": exists,
        "keyNames": keys,
        "valuesCaptured": false,
    });
    write_json_private(&evidence_dir.join("provider-trust-shape.json"), &safe)?;
    report.checks.push(Check {
        name: "maas/provider-trust".to_owned(),
        status: if present { "warn" } else { "blocked" }.to_owned(),
        detail: format!(
            "provider trust reference {kind}/{namespace}/{name} key {key:?} exists={present}; presence is not proof the live transport uses it; values omitted"
        ),
        observed_epoch_seconds: epoch_seconds(),
    });
    Ok(())
}

fn check_provider_transport(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    provider_ref: &ResourceRef,
    provider: &Value,
) -> Result<(), String> {
    let Some(hook) = args.provider_transport_hook.as_deref() else {
        add_blocked_check(
            report,
            "maas/provider-transport-tls",
            "effective provider TLS, SNI, and hostname verification require --provider-transport-hook",
        );
        return Ok(());
    };
    let endpoint = provider["spec"]["endpoint"].as_str().unwrap_or_default();
    if endpoint.is_empty() {
        add_blocked_check(
            report,
            "maas/provider-transport-tls",
            "ExternalProvider.spec.endpoint is empty",
        );
        return Ok(());
    }
    let Some(trust_source) = args.provider_trust.as_deref() else {
        add_blocked_check(
            report,
            "maas/provider-transport-tls",
            "provider trust source was not configured",
        );
        return Ok(());
    };
    let extra_args = vec![
        "--external-provider-resource".to_owned(),
        format!("{}/{}/{}", provider_ref.namespace, provider_ref.kind, provider_ref.name),
        "--expected-endpoint".to_owned(),
        endpoint.to_owned(),
        "--provider-trust".to_owned(),
        trust_source.to_owned(),
    ];
    let capture = run_hook_command(args, hook, evidence_dir, &extra_args, None)?;
    write_capture(evidence_dir, "provider-transport-hook", &capture)?;
    if !capture.success() {
        add_blocked_check(report, "maas/provider-transport-tls", "provider transport hook failed");
        return Ok(());
    }
    let Ok(observed) = serde_json::from_str::<ProviderTransportHookReport>(&capture.stdout()) else {
        add_blocked_check(
            report,
            "maas/provider-transport-tls",
            "provider transport hook returned invalid JSON",
        );
        return Ok(());
    };
    let transport_ref = parse_resource_ref(&observed.resource, &args.namespace)?;
    let transport_resource = get_resource(
        evidence_dir,
        args.context.as_deref().unwrap_or_default(),
        Some(&transport_ref.namespace),
        &transport_ref.kind,
        &transport_ref.name,
        "provider-transport-resource",
    )?;
    if !transport_resource.success() {
        add_blocked_check(
            report,
            "maas/provider-transport-tls",
            "provider transport hook referenced an unreadable resource",
        );
        return Ok(());
    }
    let endpoint_matches = observed.endpoint.eq_ignore_ascii_case(endpoint);
    let sni_matches = tls_sni_matches(endpoint, &observed.sni);
    let trust_matches = observed.trust_source == trust_source;
    let tls_valid = observed.tls_verified
        && observed.hostname_verified
        && !observed.insecure_skip_verify
        && api_access_acceptable(args, observed.api_ca_verified)
        && endpoint_matches
        && sni_matches
        && trust_matches
        && !observed.resource.is_empty()
        && !observed.configuration_revision.is_empty()
        && observed.schema == "praxis-extproc.grid-llmd-provider-transport/v1";
    let record = ProviderTransportRecord {
        resource: format!(
            "{}/{}/{}",
            transport_ref.namespace, transport_ref.kind, transport_ref.name
        ),
        trust_source: if trust_source == "system-ca" {
            "system-ca".to_owned()
        } else {
            "namespaced-reference".to_owned()
        },
        endpoint_matches,
        sni_matches,
        tls_verified: observed.tls_verified,
        hostname_verified: observed.hostname_verified,
        insecure_skip_verify: observed.insecure_skip_verify,
        configuration_revision: observed.configuration_revision,
        observed_epoch_seconds: epoch_seconds(),
    };
    report.provider_transport_verified = tls_valid;
    report.provider_transport = Some(record.clone());
    record_boolean_check(
        report,
        "maas/provider-transport-tls",
        tls_valid,
        format!(
            "effective TLS={}, hostname verification={}, SNI match={}, trust source match={}, insecureSkipVerify={}, hook API CA verified={}, runner API mode={}, config revision recorded",
            record.tls_verified,
            record.hostname_verified,
            record.sni_matches,
            trust_matches,
            record.insecure_skip_verify,
            observed.api_ca_verified,
            report.api_tls_mode
        ),
    );
    write_json_private(&evidence_dir.join("provider-transport-tls.json"), &record)
}

fn check_extproc_filter(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    if args.extproc_filter_resources.len() < args.expected_extproc_hops {
        add_blocked_check(
            report,
            "extproc/filter-config",
            "an Envoy/Istio ExtProc filter resource was not configured",
        );
        return Ok(());
    }
    let mut all_configured = true;
    for (index, raw) in args.extproc_filter_resources.iter().enumerate() {
        let filter_ref = parse_resource_ref(raw, &args.namespace)?;
        let evidence_name = format!("extproc-filter-{}", index + 1);
        let capture = get_resource(
            evidence_dir,
            context,
            Some(&filter_ref.namespace),
            &filter_ref.kind,
            &filter_ref.name,
            &evidence_name,
        )?;
        if !capture.success() {
            add_blocked_check(
                report,
                &format!("extproc/filter-{}/config", index + 1),
                "configured Envoy/Istio filter resource could not be read",
            );
            all_configured = false;
            continue;
        }
        let filter = parse_capture_json(&capture, "ExtProc filter resource")?;
        let spec_present = filter["spec"].is_object();
        let spec_text = serde_json::to_string(&filter["spec"])
            .unwrap_or_default()
            .to_ascii_lowercase();
        let extproc_configured = spec_present
            && ["ext_proc", "extproc", "externalprocessor"]
                .iter()
                .any(|needle| spec_text.contains(needle));
        let kind = filter["kind"].as_str().unwrap_or(&filter_ref.kind);
        record_boolean_check(
            report,
            &format!("extproc/filter-{}/config", index + 1),
            extproc_configured,
            format!(
                "filter resource {}/{}/{} has an ExtProc field in spec={extproc_configured}; response oracle remains required",
                filter_ref.namespace, kind, filter_ref.name
            ),
        );
        all_configured &= extproc_configured;
    }
    record_boolean_check(
        report,
        "extproc/filter-config",
        all_configured,
        format!(
            "{} configured Envoy/Istio filter resource(s) were inspected for ExtProc configuration",
            args.extproc_filter_resources.len()
        ),
    );
    Ok(())
}

fn check_source_and_image_pins(args: &GridLlmdArgs, report: &mut Report) {
    let (sources, source_errors) = parse_component_pins(&args.source_revisions, PinKind::GitSha);
    let required_sources = if args.qualification_scope == QualificationScope::RoutingSmoke {
        &ROUTING_SMOKE_REQUIRED_SOURCE_COMPONENTS[..]
    } else {
        &REQUIRED_SOURCE_COMPONENTS[..]
    };
    let required_images = if args.qualification_scope == QualificationScope::RoutingSmoke {
        &ROUTING_SMOKE_REQUIRED_IMAGE_COMPONENTS[..]
    } else {
        &REQUIRED_IMAGE_COMPONENTS[..]
    };
    let missing_sources = required_sources
        .iter()
        .filter(|component| !sources.contains_key(**component))
        .copied()
        .collect::<Vec<_>>();
    let sources_ok = !sources.is_empty() && source_errors.is_empty() && missing_sources.is_empty();
    record_boolean_check(
        report,
        "provenance/source-revisions",
        sources_ok,
        if source_errors.is_empty() && missing_sources.is_empty() {
            format!("{} immutable 40-hex source revisions recorded", sources.len())
        } else {
            format!(
                "invalid source revision input(s): {}; missing required components: {}",
                source_errors.join(", "),
                missing_sources.join(", ")
            )
        },
    );

    let (image_pins, image_errors) = parse_image_pins(&args.image_pins);
    let (runtime_digests, runtime_errors) = parse_component_pins(&args.runtime_image_digests, PinKind::Sha256);
    let mut errors = image_errors;
    errors.extend(runtime_errors);
    let ipp_present = report.runtime_images.iter().any(|record| {
        let identity = format!("{} {} {}", record.image, record.container, record.pod).to_ascii_lowercase();
        identity.contains("payload-processing") || identity.contains("/ipp") || identity.ends_with("ipp")
    });
    if ipp_present && !image_pins.contains_key("ipp") {
        errors.push("IPP runtime detected but no immutable ipp image pin was supplied".to_owned());
    }
    if image_pins.contains_key("ipp") && !sources.contains_key("ipp") {
        errors.push("IPP image pin supplied without an ipp source revision".to_owned());
    }
    let missing_images = required_images
        .iter()
        .filter(|component| !image_pins.contains_key(**component))
        .copied()
        .collect::<Vec<_>>();
    if !missing_images.is_empty() {
        errors.push(format!(
            "missing required image components: {}",
            missing_images.join(", ")
        ));
    }
    let unknown_runtime_components = runtime_digests
        .keys()
        .filter(|component| !image_pins.contains_key(*component))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown_runtime_components.is_empty() {
        errors.push(format!(
            "runtime digest without image pin: {}",
            unknown_runtime_components.join(", ")
        ));
    }
    let mut all_runtime_found = true;
    for (component, reference) in &image_pins {
        let expected_digest = runtime_digests.get(component);
        let runtime_records = report
            .runtime_images
            .iter()
            .filter(|record| record.image == *reference && record.ready && !record.image_id.is_empty())
            .collect::<Vec<_>>();
        if runtime_records.is_empty() {
            all_runtime_found = false;
            errors.push(format!("{component}: pinned image is not running in a captured Pod"));
            continue;
        }
        let Some(expected_digest) = expected_digest else {
            all_runtime_found = false;
            errors.push(format!("{component}: runtime manifest digest was not supplied"));
            continue;
        };
        for record in runtime_records {
            if runtime_image_digest(&record.image_id).as_deref() != Some(expected_digest.as_str()) {
                all_runtime_found = false;
                errors.push(format!(
                    "{component}: runtime image ID did not match the expected manifest digest"
                ));
            }
        }
    }
    let image_contract_ok = !image_pins.is_empty() && all_runtime_found && errors.is_empty();
    record_boolean_check(
        report,
        "provenance/runtime-images",
        image_contract_ok,
        if image_contract_ok {
            format!(
                "{} immutable image references matched captured runtime manifest IDs",
                image_pins.len()
            )
        } else {
            format!("image pin validation did not pass: {}", errors.join("; "))
        },
    );
}

#[derive(Clone, Copy)]
enum PinKind {
    GitSha,
    Sha256,
}

fn parse_component_pins(values: &[String], kind: PinKind) -> (BTreeMap<String, String>, Vec<String>) {
    let mut pins = BTreeMap::new();
    let mut errors = Vec::new();
    for raw in values {
        let Some((component, digest)) = raw.split_once('=') else {
            errors.push(format!("{raw:?} must be component=value"));
            continue;
        };
        let valid = match kind {
            PinKind::GitSha => digest.len() == 40 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            PinKind::Sha256 => is_sha256_digest(digest),
        };
        if component.is_empty() || !valid {
            errors.push(format!("{component:?} has an invalid immutable pin"));
            continue;
        }
        if pins.insert(component.to_owned(), digest.to_ascii_lowercase()).is_some() {
            errors.push(format!("duplicate pin for {component}"));
        }
    }
    (pins, errors)
}

fn parse_image_pins(values: &[String]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut pins = BTreeMap::new();
    let mut errors = Vec::new();
    for raw in values {
        let Some((component, reference)) = raw.split_once('=') else {
            errors.push(format!("{raw:?} must be component=image@sha256:digest"));
            continue;
        };
        let digest = reference.rsplit_once('@').map(|(_, digest)| digest).unwrap_or_default();
        if component.is_empty() || reference.is_empty() || !is_sha256_digest(digest) {
            errors.push(format!(
                "{component:?} must use an image reference pinned by sha256 digest"
            ));
            continue;
        }
        if pins.insert(component.to_owned(), reference.to_owned()).is_some() {
            errors.push(format!("duplicate image pin for {component}"));
        }
    }
    (pins, errors)
}

fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn is_git_commit_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_digest(value: &[u8]) -> String {
    let digest = Sha256::digest(value);
    format!("sha256:{digest:x}")
}

fn body_oracle_header_matches(headers: &str, header_name: &str, expected_digest: &str) -> bool {
    let expected_hex = expected_digest.strip_prefix("sha256:").unwrap_or(expected_digest);
    header_value(headers, header_name).is_some_and(|observed| observed.eq_ignore_ascii_case(expected_hex))
}

fn runtime_image_digest(image_id: &str) -> Option<String> {
    let digest = image_id
        .rsplit_once('@')
        .map(|(_, digest)| digest)
        .or_else(|| image_id.rsplit('/').next())?;
    is_sha256_digest(digest).then(|| digest.to_ascii_lowercase())
}

fn check_provider_credential_reference(
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    model_ref: &ResourceRef,
    provider_ref: &ResourceRef,
    binding: &Value,
    provider: &Value,
) -> Result<(), String> {
    let (secret_name, namespace, auth_type) = binding["auth"]["secretRef"]["name"]
        .as_str()
        .map(|name| (name, model_ref.namespace.as_str(), binding["auth"]["type"].as_str()))
        .or_else(|| {
            provider["spec"]["auth"]["secretRef"]["name"].as_str().map(|name| {
                (
                    name,
                    provider_ref.namespace.as_str(),
                    provider["spec"]["auth"]["type"].as_str(),
                )
            })
        })
        .unwrap_or(("", "", None));
    if secret_name.is_empty() {
        add_blocked_check(
            report,
            "maas/credentials",
            "no effective provider Secret reference is configured",
        );
        return Ok(());
    }
    let (exists, keys) = read_secret_key_names(context, namespace, secret_name)?;
    let required_key_present = auth_type != Some("apikey") || keys.iter().any(|key| key == "api-key");
    let detail =
        format!("Secret reference {namespace}/{secret_name} exists={exists}; key names={keys:?}; values omitted");
    report.checks.push(Check {
        name: "maas/credentials".to_owned(),
        status: if exists && required_key_present {
            "pass"
        } else {
            "blocked"
        }
        .to_owned(),
        detail,
        observed_epoch_seconds: epoch_seconds(),
    });
    let safe = serde_json::json!({
        "namespace": namespace,
        "secretName": secret_name,
        "exists": exists,
        "keyNames": keys,
        "secretValuesCaptured": false,
    });
    write_json_private(&evidence_dir.join("maas-credential-shape.json"), &safe)
}

fn read_secret_key_names(context: &str, namespace: &str, name: &str) -> Result<(bool, Vec<String>), String> {
    let capture = run_command(
        "kubectl",
        &[
            "--context",
            context,
            "--namespace",
            namespace,
            "get",
            "secret",
            name,
            "-o",
            "json",
        ],
    )?;
    if !capture.success() {
        return Ok((false, Vec::new()));
    }
    let secret = parse_capture_json(&capture, "Secret")?;
    let mut keys = secret["data"]
        .as_object()
        .map(|data| data.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    keys.sort();
    Ok((true, keys))
}

fn check_external_model_route(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    model_ref: &ResourceRef,
    model: &Value,
) -> Result<(), String> {
    let expected_gateway = gate_resource_ref(args, "gateway")?;
    let Some(status_route_ref) = model_route_ref(
        model,
        &model_ref.namespace,
        &expected_gateway.name,
        &expected_gateway.namespace,
    ) else {
        add_blocked_check(
            report,
            "maas/generated-route",
            "ExternalModel status has no generated HTTPRoute reference",
        );
        return Ok(());
    };
    let route_ref = if let Some(raw) = args.httproute_resource.as_deref() {
        let explicit = parse_resource_ref(raw, &model_ref.namespace)?;
        let matches_status = explicit.kind.eq_ignore_ascii_case(&status_route_ref.kind)
            && explicit.name == status_route_ref.name
            && explicit.namespace == status_route_ref.namespace;
        record_boolean_check(
            report,
            "maas/route-reference-match",
            matches_status,
            "explicit HTTPRoute input must match the route referenced by ExternalModel status".to_owned(),
        );
        if !matches_status {
            add_failed_check(
                report,
                "maas/generated-route",
                "explicit HTTPRoute does not match the ExternalModel status reference",
            );
            return Ok(());
        }
        explicit
    } else {
        status_route_ref
    };
    if !is_resource_kind(&route_ref.kind, &["httproute", "httproutes"]) {
        return Err("ExternalModel route reference must identify an HTTPRoute".to_owned());
    }
    let route = get_resource(
        evidence_dir,
        context,
        Some(&route_ref.namespace),
        &route_ref.kind,
        &route_ref.name,
        "maas-generated-httproute",
    )?;
    if !route.success() {
        add_blocked_check(
            report,
            "maas/generated-route",
            "generated HTTPRoute does not exist or is unreadable",
        );
        return Ok(());
    }
    let route_json = parse_capture_json(&route, "HTTPRoute")?;
    let ready = http_route_accepted_for(&route_json, &expected_gateway.name, &expected_gateway.namespace);
    let path_matches = route_matches_request_path(&route_json, &args.gateway_path);
    let host_matches = route_matches_request_host(&route_json, args.gateway_url.as_deref());
    let backend_present = route_json["spec"]["rules"].as_array().is_some_and(|rules| {
        rules
            .iter()
            .any(|rule| rule["backendRefs"].as_array().is_some_and(|refs| !refs.is_empty()))
    });
    report.checks.push(Check {
        name: "maas/generated-route".to_owned(),
        status: if ready && path_matches && host_matches && backend_present { "pass" } else { "blocked" }.to_owned(),
        detail: format!(
            "ExternalModel status route {}/{}; selected Gateway Accepted/ResolvedRefs={ready}, request path matches={path_matches}, host matches={host_matches}, backend refs present={backend_present}",
            route_ref.namespace, route_ref.name
        ),
        observed_epoch_seconds: epoch_seconds(),
});
    Ok(())
}

fn check_grid_gateway_route(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let (Some(route_raw), Some(service_raw)) =
        (args.httproute_resource.as_deref(), args.grid_gateway_service.as_deref())
    else {
        add_blocked_check(
            report,
            "grid-gateway/http-route",
            "direct Grid Gateway qualification requires --httproute-resource and --grid-gateway-service",
        );
        return Ok(());
    };
    let route_ref = parse_resource_ref(route_raw, &args.namespace)?;
    if !is_resource_kind(&route_ref.kind, &["httproute", "httproutes"]) {
        return Err("--httproute-resource must identify an HTTPRoute".to_owned());
    }
    let service_ref = parse_resource_ref(service_raw, &args.namespace)?;
    if !is_resource_kind(&service_ref.kind, &["service", "services", "svc"]) {
        return Err("--grid-gateway-service must identify a Service".to_owned());
    }
    let gateway_ref = gate_resource_ref(args, "gateway")?;
    if !is_resource_kind(&gateway_ref.kind, &["gateway", "gateways"]) {
        return Err("the Gateway readiness gate must identify a Gateway API Gateway".to_owned());
    }
    let route_capture = get_resource(
        evidence_dir,
        context,
        Some(&route_ref.namespace),
        &route_ref.kind,
        &route_ref.name,
        "grid-gateway-http-route",
    )?;
    if !route_capture.success() {
        add_blocked_check(
            report,
            "grid-gateway/http-route",
            "task-owned direct Grid Gateway HTTPRoute does not exist or is unreadable",
        );
        return Ok(());
    }
    let route = parse_capture_json(&route_capture, "Grid Gateway HTTPRoute")?;
    let accepted = http_route_accepted_for(&route, &gateway_ref.name, &gateway_ref.namespace);
    record_boolean_check(
        report,
        "grid-gateway/http-route-parent",
        accepted,
        format!(
            "HTTPRoute {}/{} has current Accepted and ResolvedRefs conditions for Gateway {}/{}",
            route_ref.namespace, route_ref.name, gateway_ref.namespace, gateway_ref.name
        ),
    );
    let path_matches = route_matches_request_path(&route, &args.gateway_path);
    if let Some(gateway_url) = args.gateway_url.as_deref() {
        let host_matches = route_matches_request_host(&route, Some(gateway_url));
        record_boolean_check(
            report,
            "grid-gateway/http-route-request-match",
            path_matches && host_matches,
            format!(
                "request path match={path_matches}, Gateway hostname match={host_matches}; URL omitted from evidence"
            ),
        );
    } else {
        add_blocked_check(
            report,
            "grid-gateway/http-route-hostname",
            "--gateway-url is required to match the HTTPRoute hostname to the requested front door",
        );
        record_boolean_check(
            report,
            "grid-gateway/http-route-path",
            path_matches,
            format!("request path match={path_matches}"),
        );
    }
    let service_capture = get_resource(
        evidence_dir,
        context,
        Some(&service_ref.namespace),
        "service",
        &service_ref.name,
        "grid-gateway-service",
    )?;
    if !service_capture.success() {
        add_blocked_check(
            report,
            "grid-gateway/backend-service",
            "expected Grid Gateway Service does not exist or is unreadable",
        );
        return Ok(());
    }
    let service = parse_capture_json(&service_capture, "Grid Gateway Service")?;
    let service_usable = service["metadata"]["name"].as_str() == Some(service_ref.name.as_str())
        && service["spec"]["type"].as_str() != Some("ExternalName")
        && service["spec"]["ports"]
            .as_array()
            .is_some_and(|ports| !ports.is_empty());
    let backend_matches = route_targets_only_service(&route, &args.gateway_path, &service_ref);
    record_boolean_check(
        report,
        "grid-gateway/backend-service",
        service_usable && backend_matches,
        format!(
            "HTTPRoute request-matching rules target only expected Grid Gateway Service {}/{}, Service usable={service_usable}, backend match={backend_matches}",
            service_ref.namespace, service_ref.name
        ),
    );
    let backend_ports_match = route_backend_ports_exist(&route, &args.gateway_path, &service);
    record_boolean_check(
        report,
        "grid-gateway/backend-service-port",
        backend_ports_match,
        "each matched HTTPRoute backend port is present on the expected Grid Gateway Service".to_owned(),
    );
    Ok(())
}

fn model_route_ref(
    model: &Value,
    default_namespace: &str,
    expected_gateway: &str,
    expected_namespace: &str,
) -> Option<ResourceRef> {
    let status = model["status"]["gateways"].as_array()?.iter().find(|entry| {
        entry["name"].as_str() == Some(expected_gateway) && entry["namespace"].as_str() == Some(expected_namespace)
    })?;
    let route_name = status["httpRouteRef"]["name"]
        .as_str()
        .or_else(|| model["status"]["httpRouteName"].as_str())?;
    let namespace = status["httpRouteRef"]["namespace"]
        .as_str()
        .unwrap_or(default_namespace);
    Some(ResourceRef {
        namespace: namespace.to_owned(),
        kind: "httproute".to_owned(),
        name: route_name.to_owned(),
    })
}

fn http_route_accepted_for(route: &Value, gateway: &str, gateway_namespace: &str) -> bool {
    let generation = route["metadata"]["generation"].as_u64();
    let route_namespace = route["metadata"]["namespace"].as_str().unwrap_or_default();
    route["status"]["parents"].as_array().is_some_and(|parents| {
        parents.iter().any(|parent| {
            let parent_ref = &parent["parentRef"];
            let parent_kind = parent_ref["kind"].as_str().unwrap_or("Gateway");
            let parent_group = parent_ref["group"].as_str().unwrap_or("gateway.networking.k8s.io");
            let parent_matches = parent_ref["name"].as_str() == Some(gateway)
                && parent_ref["namespace"].as_str().unwrap_or(route_namespace) == gateway_namespace
                && parent_kind == "Gateway"
                && parent_group == "gateway.networking.k8s.io";
            let conditions = parent["conditions"].as_array();
            parent_matches
                && ["Accepted", "ResolvedRefs"].iter().all(|condition_type| {
                    conditions.is_some_and(|items| {
                        items.iter().any(|condition| {
                            condition["type"].as_str() == Some(condition_type)
                                && condition["status"].as_str() == Some("True")
                                && generation
                                    .is_none_or(|wanted| condition["observedGeneration"].as_u64() == Some(wanted))
                        })
                    })
                })
        })
    })
}

fn route_matches_request_path(route: &Value, request_path: &str) -> bool {
    route["spec"]["rules"].as_array().is_some_and(|rules| {
        rules
            .iter()
            .any(|rule| route_rule_matches_request_path(rule, request_path))
    })
}

fn route_rule_matches_request_path(rule: &Value, request_path: &str) -> bool {
    let Some(matches) = rule["matches"].as_array() else {
        // Gateway API defaults an omitted matches list to match-all.
        return true;
    };
    matches.iter().any(|matched| {
        let path = &matched["path"];
        let value = path["value"].as_str().unwrap_or("/");
        match path["type"].as_str().unwrap_or("PathPrefix") {
            "Exact" => request_path == value,
            "PathPrefix" => {
                if value == "/" {
                    request_path.starts_with('/')
                } else {
                    request_path == value
                        || request_path
                            .strip_prefix(value)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                }
            },
            _ => false,
        }
    })
}

fn route_targets_only_service(route: &Value, request_path: &str, service: &ResourceRef) -> bool {
    let route_namespace = route["metadata"]["namespace"].as_str().unwrap_or_default();
    let Some(rules) = route["spec"]["rules"].as_array() else {
        return false;
    };
    let matching_rules = rules
        .iter()
        .filter(|rule| route_rule_matches_request_path(rule, request_path))
        .collect::<Vec<_>>();
    !matching_rules.is_empty()
        && matching_rules.iter().all(|rule| {
            rule["backendRefs"].as_array().is_some_and(|backends| {
                !backends.is_empty()
                    && backends.iter().all(|backend| {
                        let namespace = backend["namespace"].as_str().unwrap_or(route_namespace);
                        let kind = backend["kind"].as_str().unwrap_or("Service");
                        let group = backend["group"].as_str().unwrap_or_default();
                        namespace == service.namespace
                            && backend["name"].as_str() == Some(service.name.as_str())
                            && kind == "Service"
                            && group.is_empty()
                    })
            })
        })
}

fn route_backend_ports_exist(route: &Value, request_path: &str, service: &Value) -> bool {
    let Some(rules) = route["spec"]["rules"].as_array() else {
        return false;
    };
    let matching_rules = rules
        .iter()
        .filter(|rule| route_rule_matches_request_path(rule, request_path))
        .collect::<Vec<_>>();
    !matching_rules.is_empty()
        && matching_rules.iter().all(|rule| {
            rule["backendRefs"].as_array().is_some_and(|backends| {
                !backends.is_empty()
                    && backends.iter().all(|backend| {
                        backend["port"].as_u64().is_some_and(|wanted| {
                            service["spec"]["ports"]
                                .as_array()
                                .is_some_and(|ports| ports.iter().any(|port| port["port"].as_u64() == Some(wanted)))
                        })
                    })
            })
        })
}

fn tls_sni_matches(endpoint: &str, sni: &str) -> bool {
    url_host(endpoint).is_some_and(|host| {
        host.trim_end_matches('.')
            .eq_ignore_ascii_case(sni.trim().trim_end_matches('.'))
    })
}

fn route_matches_request_host(route: &Value, gateway_url: Option<&str>) -> bool {
    let hostnames = route["spec"]["hostnames"].as_array();
    let Some(hostnames) = hostnames else { return true };
    if hostnames.is_empty() {
        return true;
    }
    let Some(host) = gateway_url.and_then(url_host) else {
        return false;
    };
    hostnames.iter().filter_map(Value::as_str).any(|allowed| {
        if let Some(suffix) = allowed.strip_prefix("*.") {
            host.ends_with(&format!(".{suffix}")) && host != suffix
        } else {
            host.eq_ignore_ascii_case(allowed)
        }
    })
}

fn url_host(value: &str) -> Option<String> {
    let (_, rest) = value.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    let host = authority.rsplit_once(':').map_or(authority, |(host, port)| {
        if port.chars().all(|character| character.is_ascii_digit()) {
            host
        } else {
            authority
        }
    });
    Some(host.to_ascii_lowercase())
}

fn gateway_connect_to_matches(gateway_url: &str, mapping: &str) -> bool {
    let parts = mapping.split(':').collect::<Vec<_>>();
    if parts.len() != 4 {
        return false;
    }
    let Some(request_host) = url_host(gateway_url) else {
        return false;
    };
    let Some(request_port) = url_port(gateway_url) else {
        return false;
    };
    let valid_host = |host: &str| {
        host.len() <= 253
            && !host.is_empty()
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label.as_bytes()[0].is_ascii_alphanumeric()
                    && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                    && label.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            })
    };
    let valid_port = |port: &str| port.parse::<u16>().is_ok_and(|value| value > 0);
    gateway_url.starts_with("https://")
        && parts[0].eq_ignore_ascii_case(&request_host)
        && parts[1].parse::<u16>().ok() == Some(request_port)
        && valid_host(parts[2])
        && valid_port(parts[3])
}

fn url_port(value: &str) -> Option<u16> {
    let (scheme, rest) = value.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    if let Some((_, port)) = authority.rsplit_once(':')
        && port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return port.parse::<u16>().ok();
    }
    match scheme {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    }
}

fn parse_capture_json(capture: &CommandCapture, label: &str) -> Result<Value, String> {
    serde_json::from_str(&capture.stdout()).map_err(|error| format!("parse {label} JSON: {error}"))
}

fn add_blocked_check(report: &mut Report, name: &str, detail: &str) {
    report.checks.push(Check {
        name: name.to_owned(),
        status: "blocked".to_owned(),
        detail: detail.to_owned(),
        observed_epoch_seconds: epoch_seconds(),
    });
}

fn add_failed_check(report: &mut Report, name: &str, detail: &str) {
    report.checks.push(Check {
        name: name.to_owned(),
        status: "fail".to_owned(),
        detail: detail.to_owned(),
        observed_epoch_seconds: epoch_seconds(),
    });
}

fn check_grid_operator_deployment(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let Some(raw) = args.grid_operator_deployment.as_deref() else {
        add_blocked_check(
            report,
            "praxis-ai-grid-operator/deployment",
            "the downstream ODH Grid operator Deployment must be named explicitly",
        );
        return Ok(());
    };
    let deployment_ref = parse_deployment_ref(raw, &args.namespace)?;
    let deployment = get_resource(
        evidence_dir,
        context,
        Some(&deployment_ref.namespace),
        "deployment",
        &deployment_ref.name,
        "praxis-ai-grid-operator-deployment",
    )?;
    if !deployment.success() {
        add_blocked_check(
            report,
            "praxis-ai-grid-operator/deployment",
            "the named downstream operator Deployment does not exist or is unreadable",
        );
        return Ok(());
    }
    let deployment_json = parse_capture_json(&deployment, "downstream Grid operator Deployment")?;
    record_deployment_image(
        report,
        &deployment_ref.namespace,
        &deployment_ref.name,
        &deployment.stdout(),
    );
    let (ready, detail) = deployment_readiness(&deployment.stdout());
    record_boolean_check(
        report,
        "praxis-ai-grid-operator/deployment-ready",
        ready,
        format!(
            "{}; deployment={}/{}",
            detail, deployment_ref.namespace, deployment_ref.name
        ),
    );
    let (pins, errors) = parse_image_pins(&args.image_pins);
    let expected_image = pins.get("praxis-ai-grid-operator");
    let actual_images = deployment_json["spec"]["template"]["spec"]["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|container| container["image"].as_str())
        .collect::<Vec<_>>();
    let image_matches = expected_image.is_some_and(|expected| deployment_has_exact_image(&deployment_json, expected));
    record_boolean_check(
        report,
        "praxis-ai-grid-operator/deployment-image",
        image_matches,
        if errors.is_empty() {
            format!(
                "named Deployment contains exactly one container pinned to the required downstream operator image; observed {} container image(s)",
                actual_images.len()
            )
        } else {
            format!("image pin input is invalid: {}", errors.join("; "))
        },
    );
    Ok(())
}

fn deployment_has_exact_image(deployment: &Value, expected_image: &str) -> bool {
    deployment["spec"]["template"]["spec"]["containers"]
        .as_array()
        .is_some_and(|containers| {
            containers
                .iter()
                .filter(|container| container["image"].as_str() == Some(expected_image))
                .count()
                == 1
        })
}

/// Verify the Grid operator's real output contract rather than only checking
/// that a Grid CR exists. The operator owns GridNetwork/GridSite/
/// InferenceProvider reconciliation and publishes a versioned overlay
/// ConfigMap. MaaS ExternalModel translation is intentionally outside this
/// suite; the Grid operator owns only the Grid overlay.
fn check_grid_operator_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let raw = args
        .gate_resources
        .iter()
        .find(|value| value.starts_with("grid="))
        .ok_or_else(|| "the grid gate is required before checking the Grid operator contract".to_owned())?;
    let (_, resource) = raw
        .split_once('=')
        .ok_or_else(|| format!("invalid grid gate {raw:?}"))?;
    let (resource, _) = resource
        .split_once('#')
        .map_or((resource, None), |(name, condition)| (name, Some(condition)));
    let grid_ref = parse_resource_ref(resource, &args.namespace)?;
    let kind = grid_ref.kind.as_str();
    let name = grid_ref.name.as_str();

    let network = get_resource(
        evidence_dir,
        context,
        Some(&grid_ref.namespace),
        kind,
        name,
        "operator-grid-network",
    )?;
    if !network.success() {
        report.checks.push(Check {
            name: "grid-operator/status".to_owned(),
            status: "blocked".to_owned(),
            detail: format!("could not read GridNetwork: {}", network.stderr().trim()),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    }
    let network_json =
        serde_json::from_str::<Value>(&network.stdout()).map_err(|error| format!("parse GridNetwork JSON: {error}"))?;
    let (status_field, status_items) = grid_routing_map_status_entries(&network_json);
    let statuses = status_items.cloned().unwrap_or_default();
    let expected_gateway = args.grid_gateway_name.clone().or_else(|| {
        args.gate_resources.iter().find_map(|value| {
            let (gate, resource) = value.split_once('=')?;
            if gate != "gateway" {
                return None;
            }
            let resource = resource.split_once('#').map_or(resource, |(name, _)| name);
            let parsed = parse_resource_ref(resource, &args.namespace).ok()?;
            is_resource_kind(&parsed.kind, &["gateway", "gateways"]).then_some(parsed.name)
        })
    });
    let selected = expected_gateway.as_deref().and_then(|gateway| {
        statuses
            .iter()
            .find(|entry| entry["gatewayName"].as_str() == Some(gateway))
    });
    let selected = selected.or_else(|| (statuses.len() == 1).then(|| &statuses[0]));
    let Some(status) = selected else {
        report.checks.push(Check {
            name: "grid-operator/status".to_owned(),
            status: "fail".to_owned(),
            detail: format!(
                "GridNetwork.status.{status_field} has {} entries and none identifies the serving Gateway",
                statuses.len()
            ),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    };

    let phase = status["phase"].as_str().unwrap_or("unknown");
    let config_map_name = status["configMapName"].as_str().unwrap_or_default();
    let rendered_revision = status["renderedRevision"].as_str().unwrap_or_default();
    let distributed_revision = status["distributedRevision"].as_str().unwrap_or_default();
    let content_digest = status["contentDigest"].as_str().unwrap_or_default();
    let network_generation = network_json["metadata"]["generation"].as_u64();
    let observed_generation = status["observedGeneration"].as_u64();
    let status_ok = network_generation.is_some()
        && observed_generation == network_generation
        && phase == "Distributed"
        && !config_map_name.is_empty()
        && !rendered_revision.is_empty()
        && rendered_revision == distributed_revision
        && !content_digest.is_empty();
    report.checks.push(Check {
        name: "grid-operator/status".to_owned(),
        status: if status_ok { "pass" } else { "fail" }.to_owned(),
        detail: format!(
            "statusField={status_field}, gateway={:?}, phase={phase}, observedGeneration={observed_generation:?}/{network_generation:?}, configMap={config_map_name:?}, renderedRevision={rendered_revision:?}, distributedRevision={distributed_revision:?}",
            status["gatewayName"].as_str()
        ),
        observed_epoch_seconds: epoch_seconds(),
});
    check_optional_consumer_config(report, &network_json, status);
    if !status_ok || config_map_name.is_empty() {
        return Ok(());
    }

    let config_map_namespace = status["namespace"]
        .as_str()
        .filter(|value| !value.is_empty())
        .unwrap_or(report.namespace.as_str());
    let overlay = get_resource(
        evidence_dir,
        context,
        Some(config_map_namespace),
        "configmap",
        config_map_name,
        "operator-overlay-configmap",
    )?;
    if !overlay.success() {
        report.checks.push(Check {
            name: "grid-operator/overlay".to_owned(),
            status: "fail".to_owned(),
            detail: format!(
                "could not read generated overlay ConfigMap: {}",
                overlay.stderr().trim()
            ),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    }
    let overlay_json = serde_json::from_str::<Value>(&overlay.stdout())
        .map_err(|error| format!("parse generated overlay ConfigMap JSON: {error}"))?;
    let data = &overlay_json["data"];
    let envelope_text = data["routing-overlay.json"].as_str();
    let legacy_present = data["routing-config.json"].is_string();
    let annotations = &overlay_json["metadata"]["annotations"];
    let Some(envelope_text) = envelope_text else {
        report.checks.push(Check {
            name: "grid-operator/overlay".to_owned(),
            status: "fail".to_owned(),
            detail: "generated ConfigMap is missing data[routing-overlay.json]".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
        });
        return Ok(());
    };
    let envelope = serde_json::from_str::<Value>(envelope_text)
        .map_err(|error| format!("parse generated routing overlay envelope: {error}"))?;
    let envelope_revision = envelope["revision"]["value"].as_str().unwrap_or_default();
    let envelope_digest = envelope["content_digest"]["value"].as_str().unwrap_or_default();
    let producer = envelope["provenance"]["producer"].as_str().unwrap_or_default();
    let annotation_revision = annotations["grid.praxis.fast/overlay-revision"]
        .as_str()
        .unwrap_or_default();
    let annotation_digest = annotations["grid.praxis.fast/overlay-content-digest"]
        .as_str()
        .unwrap_or_default();
    let overlay_ok = legacy_present
        && producer == "grid-operator"
        && envelope_revision == distributed_revision
        && envelope_digest == content_digest
        && annotation_revision == distributed_revision
        && annotation_digest == content_digest;
    report.checks.push(Check {
        name: "grid-operator/overlay".to_owned(),
        status: if overlay_ok { "pass" } else { "fail" }.to_owned(),
        detail: format!(
            "producer={producer:?}, legacyKey={legacy_present}, envelopeRevision={envelope_revision:?}, statusRevision={distributed_revision:?}, annotationRevision={annotation_revision:?}"
        ),
        observed_epoch_seconds: epoch_seconds(),
});
    check_grid_scoring_contract(args, report, evidence_dir, &network_json, &envelope, "preflight")?;
    Ok(())
}

/// If the GridNetwork opts into operator-managed consumer Praxis config, make
/// sure its status says that config was rendered. Otherwise record the
/// intentional boundary: the deployment-supplied Praxis/ExtProc config owns
/// the consumer path for this suite.
fn check_optional_consumer_config(report: &mut Report, network: &Value, overlay_status: &Value) {
    let gateway_name = overlay_status["gatewayName"].as_str();
    let gateway_ref = network["spec"]["gatewayRefs"]
        .as_array()
        .and_then(|refs| refs.iter().find(|gateway| gateway["name"].as_str() == gateway_name));
    let enabled = gateway_ref.is_some_and(|gateway| {
        gateway["consumerConfig"]["enabled"].as_bool() == Some(true)
            || gateway["praxisConfig"]["generate"].as_bool() == Some(true)
    });
    if !enabled {
        report.checks.push(Check {
            name: "grid-operator/consumer-config".to_owned(),
            status: "pass".to_owned(),
            detail: "consumer config is not operator-generated; the deployment-supplied Praxis/ExtProc configuration owns the consumer path".to_owned(),
            observed_epoch_seconds: epoch_seconds(),
});
        return;
    }
    let consumer_status = grid_praxis_config_status_entries(network);
    let rendered = consumer_status.and_then(|statuses| {
        statuses
            .iter()
            .find(|entry| entry["gatewayName"].as_str() == gateway_name && entry["phase"].as_str() == Some("Rendered"))
    });
    let passed = rendered.is_some_and(|entry| entry["configMapName"].as_str().is_some_and(|name| !name.is_empty()));
    report.checks.push(Check {
        name: "grid-operator/consumer-config".to_owned(),
        status: if passed { "pass" } else { "blocked" }.to_owned(),
        detail: if passed {
            format!("operator-managed consumer config rendered for gateway={gateway_name:?}")
        } else {
            format!(
                "consumerConfig.enabled=true but no Rendered consumer/Praxis config status exists for gateway={gateway_name:?}"
            )
        },
        observed_epoch_seconds: epoch_seconds(),
});
}

fn resource_condition(json: &str, condition_type: &str) -> (bool, String) {
    if condition_type == "exists" {
        return (true, "resource exists".to_owned());
    }
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return (false, "kubectl returned invalid resource JSON".to_owned());
    };
    let Some(condition) = value["status"]["conditions"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"].as_str() == Some(condition_type)))
    else {
        return (false, format!("condition {condition_type:?} was not present"));
    };
    let status = condition["status"].as_str().unwrap_or("unknown");
    let generation = value["metadata"]["generation"].as_u64();
    let observed_generation = condition["observedGeneration"]
        .as_u64()
        .or_else(|| value["status"]["observedGeneration"].as_u64());
    let generation_current = match generation {
        Some(generation) => observed_generation == Some(generation),
        None => true,
    };
    (
        status == "True" && generation_current,
        format!(
            "condition {condition_type}={status}, observedGeneration={observed_generation:?}, generation={generation:?}"
        ),
    )
}

fn is_resource_kind(kind: &str, accepted: &[&str]) -> bool {
    let normalized = kind.to_ascii_lowercase();
    accepted.iter().any(|candidate| normalized == *candidate)
        || accepted
            .iter()
            .any(|candidate| normalized.starts_with(&format!("{candidate}.")))
}

fn check_pool_resources(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let has_two_pool_refs = args.pool_resources.len() >= 2;
    if !has_two_pool_refs {
        add_blocked_check(
            report,
            "pool/independent-resources",
            &format!(
                "only {} InferencePool reference(s) were supplied; signal qualification requires two distinct pools",
                args.pool_resources.len()
            ),
        );
    }
    let namespace = report.namespace.clone();
    let mut identities = std::collections::BTreeSet::new();
    let mut all_are_inference_pools = true;
    let mut unique = true;
    for raw in &args.pool_resources {
        let (resource, condition) = raw
            .split_once('#')
            .map_or((raw.as_str(), None), |(name, condition)| (name, Some(condition)));
        let resource_ref = parse_resource_ref(resource, &namespace)?;
        if !is_resource_kind(&resource_ref.kind, &["inferencepool", "inferencepools"]) {
            all_are_inference_pools = false;
            add_failed_check(
                report,
                "pool/independent-resources",
                "each pool input must identify an actual InferencePool",
            );
            continue;
        }
        let identity = format!("{}/{}/{}", resource_ref.namespace, resource_ref.kind, resource_ref.name);
        if !identities.insert(identity) {
            unique = false;
        }
        check_resource(
            report,
            evidence_dir,
            context,
            Some(&resource_ref.namespace),
            &resource_ref.kind,
            &resource_ref.name,
            &format!("pool/{}", resource_ref.name),
            condition,
        )?;
    }
    if has_two_pool_refs && all_are_inference_pools && unique && identities.len() >= 2 {
        record_boolean_check(
            report,
            "pool/independent-resources",
            true,
            format!("{} distinct InferencePool resources were checked", identities.len()),
        );
    } else if has_two_pool_refs {
        add_failed_check(
            report,
            "pool/independent-resources",
            "at least two distinct namespaced InferencePool resources are required; duplicate pool refs are not independent",
        );
    }
    Ok(())
}

fn check_serving_graph(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let (model_resources, model_errors) = parse_pool_resource_mappings(
        &args.pool_model_resources,
        &args.namespace,
        &["llminferenceservice", "llminferenceservices"],
    );
    let (backend_services, service_errors) =
        parse_pool_resource_mappings(&args.pool_backend_services, &args.namespace, &["service", "services"]);
    let mut errors = model_errors;
    errors.extend(service_errors);
    if !errors.is_empty() {
        add_blocked_check(
            report,
            "serving/pool-graph-inputs",
            &format!("invalid per-pool serving graph inputs: {}", errors.join("; ")),
        );
        return Ok(());
    }

    let pool_names = args
        .pool_resources
        .iter()
        .filter_map(|raw| {
            raw.split('#')
                .next()
                .and_then(|resource| parse_resource_ref(resource, &args.namespace).ok())
                .map(|resource| resource.name)
        })
        .collect::<std::collections::BTreeSet<_>>();
    let model_names = model_resources
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let service_names = backend_services
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let model_targets = model_resources
        .values()
        .map(resource_identity)
        .collect::<std::collections::BTreeSet<_>>();
    let service_targets = backend_services
        .values()
        .map(resource_identity)
        .collect::<std::collections::BTreeSet<_>>();
    if pool_names.len() < 2
        || pool_names.len() != args.pool_resources.len()
        || pool_names != model_names
        || pool_names != service_names
        || model_targets.len() != model_resources.len()
        || service_targets.len() != backend_services.len()
    {
        add_blocked_check(
            report,
            "serving/pool-graph-inputs",
            "each of two distinct InferencePools must map to exactly one LLMInferenceService and one backend Service by pool name",
        );
        return Ok(());
    }

    let (runtime_digests, _) = parse_component_pins(&args.runtime_image_digests, PinKind::Sha256);
    let expected_epp_digest = runtime_digests.get("llm-d-epp").map(String::as_str);
    let expected_simulator_digest = runtime_digests.get("llm-d-sim").map(String::as_str);
    let mut namespace_snapshots = BTreeMap::new();
    let mut pool_selectors = std::collections::BTreeSet::new();
    for raw in &args.pool_resources {
        let (resource, _) = raw
            .split_once('#')
            .map_or((raw.as_str(), None), |(resource, condition)| {
                (resource, Some(condition))
            });
        let pool_ref = parse_resource_ref(resource, &args.namespace)?;
        if !namespace_snapshots.contains_key(&pool_ref.namespace)
            && let Some(snapshot) =
                collect_serving_namespace_snapshot(report, evidence_dir, context, &pool_ref.namespace)?
        {
            namespace_snapshots.insert(pool_ref.namespace.clone(), snapshot);
        }
    }

    for raw in &args.pool_resources {
        let (resource, _) = raw
            .split_once('#')
            .map_or((raw.as_str(), None), |(resource, condition)| {
                (resource, Some(condition))
            });
        let pool_ref = parse_resource_ref(resource, &args.namespace)?;
        let Some(model_ref) = model_resources.get(&pool_ref.name) else {
            add_blocked_check(
                report,
                "serving/pool-graph-inputs",
                &format!("pool {} has no LLMInferenceService mapping", pool_ref.name),
            );
            continue;
        };
        let Some(backend_ref) = backend_services.get(&pool_ref.name) else {
            add_blocked_check(
                report,
                "serving/pool-graph-inputs",
                &format!("pool {} has no backend Service mapping", pool_ref.name),
            );
            continue;
        };
        if model_ref.namespace != pool_ref.namespace || backend_ref.namespace != pool_ref.namespace {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/namespace", pool_ref.name),
                "LLMInferenceService and backend Service must be in the InferencePool namespace",
            );
            continue;
        }

        let pool = read_resource_json(
            report,
            evidence_dir,
            context,
            &pool_ref,
            &format!("serving-pool-{}", safe_name(&pool_ref.name)),
        )?;
        let model = read_resource_json(
            report,
            evidence_dir,
            context,
            model_ref,
            &format!("serving-model-{}", safe_name(&pool_ref.name)),
        )?;
        let (Some(pool), Some(model)) = (pool, model) else {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/objects", pool_ref.name),
                "InferencePool or mapped LLMInferenceService is missing or unreadable",
            );
            continue;
        };

        let model_ready = resource_condition(&serde_json::to_string(&model).unwrap_or_default(), "Ready").0;
        let pool_status_ready = current_pool_status_ready(&pool);
        // Some KServe-managed InferencePool objects do not publish status. In
        // that case, readiness is established by the current LLMInferenceService
        // condition plus the EPP and simulator Service target checks below.
        let pool_ready = pool_status_ready.unwrap_or(model_ready);
        let pool_status_detail = match pool_status_ready {
            Some(true) => "current InferencePool status conditions are accepted".to_owned(),
            Some(false) => "published InferencePool status conditions are false or stale".to_owned(),
            None => "InferencePool status is not published; readiness is checked through LLMInferenceService, EPP, and simulator targets".to_owned(),
        };
        let model_pool = &model["status"]["router"]["scheduler"]["inferencePool"];
        let model_reports_pool = model_pool["name"].as_str() == Some(pool_ref.name.as_str())
            && model_pool["kind"].as_str().is_none_or(|kind| kind == "InferencePool")
            && model_pool["namespace"]
                .as_str()
                .is_none_or(|namespace| namespace == pool_ref.namespace);
        let endpoint_picker = &pool["spec"]["endpointPickerRef"];
        let epp_name = endpoint_picker["name"].as_str().unwrap_or_default();
        let epp_kind = endpoint_picker["kind"].as_str().unwrap_or("Service");
        let epp_group = endpoint_picker["group"].as_str().unwrap_or_default();
        let epp_port = endpoint_picker["port"]["number"].as_u64();
        let epp_ref_valid = !epp_name.is_empty() && epp_kind.eq_ignore_ascii_case("Service") && epp_group.is_empty();
        let model_service = &model["status"]["router"]["scheduler"]["service"];
        let model_reports_epp_service = epp_ref_valid
            && model_service["name"].as_str() == Some(epp_name)
            && model_service["kind"].as_str().is_none_or(|kind| kind == "Service")
            && model_service["namespace"]
                .as_str()
                .is_none_or(|namespace| namespace == pool_ref.namespace);
        let pool_selector = inference_pool_service_selector(&pool);
        let selector_valid = pool_selector.is_some();
        let selector_unique = pool_selector.is_some_and(|selector| {
            serde_json::to_string(&selector["matchLabels"]).is_ok_and(|selector| pool_selectors.insert(selector))
        });
        let Some(namespace_snapshot) = namespace_snapshots.get(&pool_ref.namespace) else {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/namespace-snapshot", pool_ref.name),
                "could not collect the serving namespace snapshot",
            );
            continue;
        };
        let epp_inspection = if epp_ref_valid && epp_port.is_some() {
            inspect_service_targets(
                report,
                context,
                evidence_dir,
                namespace_snapshot,
                &ResourceRef {
                    namespace: pool_ref.namespace.clone(),
                    kind: "service".to_owned(),
                    name: epp_name.to_owned(),
                },
                None,
                epp_port,
                expected_epp_digest,
                "llm-d-epp",
            )?
        } else {
            None
        };
        let backend_inspection = inspect_service_targets(
            report,
            context,
            evidence_dir,
            namespace_snapshot,
            backend_ref,
            pool_selector,
            None,
            expected_simulator_digest,
            "llm-d-sim",
        )?;
        let simulator_receipt_logging_ready = backend_inspection
            .as_ref()
            .is_some_and(|inspection| simulator_receipt_logging_enabled(namespace_snapshot, inspection));
        record_boolean_check(
            report,
            &format!("serving/pool/{}/simulator-receipt-logging", pool_ref.name),
            simulator_receipt_logging_ready,
            "the single pinned simulator Service target is Ready and runs with debug request-completion logging"
                .to_owned(),
        );
        if !simulator_receipt_logging_ready {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/simulator-receipt-logging", pool_ref.name),
                "independent receipt requires exactly one ready pinned simulator target with --v=4 enabled",
            );
        }
        let epp_result = epp_inspection;
        let model_ok = pool_ready
            && model_ready
            && model_reports_pool
            && model_reports_epp_service
            && selector_valid
            && selector_unique;
        record_boolean_check(
            report,
            &format!("serving/pool/{}/model-and-epp-ref", pool_ref.name),
            model_ok,
            format!(
                "poolReady={pool_ready}, poolStatus={pool_status_detail}, modelReady={model_ready}, modelStatusNamesPool={model_reports_pool}, modelStatusNamesEppService={model_reports_epp_service}, poolSelectorNonEmpty={selector_valid}, poolSelectorUnique={selector_unique}, endpointPickerRef={epp_kind}/{epp_name}:{epp_port:?}"
            ),
        );
        if !epp_ref_valid || epp_port.is_none() {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/epp-ref", pool_ref.name),
                "InferencePool.spec.endpointPickerRef must identify a Service with a service port",
            );
        }
        if let Some(inspection) = &epp_result {
            record_boolean_check(
                report,
                &format!("serving/pool/{}/epp-targets", pool_ref.name),
                inspection.passed,
                inspection.detail.clone(),
            );
        } else {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/epp-targets", pool_ref.name),
                "the EPP Service target graph could not be inspected",
            );
        }
        if let Some(inspection) = &backend_inspection {
            record_boolean_check(
                report,
                &format!("serving/pool/{}/simulator-targets", pool_ref.name),
                inspection.passed,
                inspection.detail.clone(),
            );
        } else {
            add_blocked_check(
                report,
                &format!("serving/pool/{}/simulator-targets", pool_ref.name),
                "the simulator Service target graph could not be inspected",
            );
        }
        if let (Some(epp), Some(backend)) = (epp_result, backend_inspection) {
            report.serving_pools.push(ServingPoolRecord {
                pool: format!("{}/{}/{}", pool_ref.namespace, pool_ref.kind, pool_ref.name),
                graph_passed: model_ok && epp.passed && backend.passed,
                model_resource: format!("{}/{}/{}", model_ref.namespace, model_ref.kind, model_ref.name),
                pool_ready,
                model_reports_pool,
                model_reports_epp_service,
                epp_service: format!("{}/service/{}", pool_ref.namespace, epp_name),
                epp_ready_pod_uids: epp.pod_uids,
                epp_deployments: epp.deployment_names,
                epp_runtime_image_ids: epp.image_ids,
                backend_service: format!("{}/{}/{}", backend_ref.namespace, backend_ref.kind, backend_ref.name),
                backend_ready_pod_uids: backend.pod_uids,
                simulator_runtime_image_ids: backend.image_ids,
                observed_epoch_seconds: epoch_seconds(),
            });
        }
    }
    let serving_pools = report.serving_pools.clone();
    check_pool_backend_identity_separation(report, &serving_pools, pool_names.len());
    Ok(())
}

/// Compare the live mounted Praxis request contract with the two generated pool routes.
/// Pool routes are separate from the public task Gateway route and can otherwise return
/// an upstream Envoy 404/NR after Grid has already selected a pool.
fn check_pool_route_contract(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    let Some(hook) = args.pool_route_contract_hook.as_deref() else {
        if args.mode == Mode::Run {
            add_blocked_check(
                report,
                "serving/pool-route-contract",
                "run mode requires --pool-route-contract-hook to compare mounted Praxis config with both pool HTTPRoutes before traffic",
            );
        } else {
            report.checks.push(Check {
                name: "serving/pool-route-contract".to_owned(),
                status: "skipped".to_owned(),
                detail: "no route-contract hook was supplied for this read-only preflight".to_owned(),
                observed_epoch_seconds: epoch_seconds(),
            });
        }
        return Ok(());
    };

    let network = gate_resource_ref(args, "grid")?;
    let gateway = gate_gateway_name(args)?;
    let mut hook_args = vec![
        hook.display().to_string(),
        "--context".to_owned(),
        context.to_owned(),
        "--namespace".to_owned(),
        args.namespace.clone(),
        "--grid-network".to_owned(),
        network.name,
        "--gateway-name".to_owned(),
        gateway,
        "--model".to_owned(),
        args.model.clone(),
        "--model-header".to_owned(),
        args.model_header.clone(),
        "--request-path".to_owned(),
        args.gateway_path.clone(),
        "--evidence-dir".to_owned(),
        evidence_dir.display().to_string(),
    ];
    for mapping in &args.pool_model_resources {
        hook_args.push("--pool-model-resource".to_owned());
        hook_args.push(mapping.clone());
    }
    let capture = run_command_owned("python3", &hook_args)?;
    write_capture(evidence_dir, "pool-route-contract-hook", &capture)?;
    let summary = serde_json::from_str::<Value>(&capture.stdout()).ok();
    let passed = capture.success() && summary.as_ref().is_some_and(|value| value["status"] == "PASS");
    let detail = summary.as_ref().and_then(|value| value["detail"].as_str()).map_or_else(
        || "route-contract hook did not return a passing JSON report; see captured hook output".to_owned(),
        str::to_owned,
    );
    report.checks.push(Check {
        name: "serving/pool-route-contract".to_owned(),
        status: if passed { "pass" } else { "fail" }.to_owned(),
        detail,
        observed_epoch_seconds: epoch_seconds(),
    });
    Ok(())
}

fn check_pool_backend_identity_separation(report: &mut Report, pools: &[ServingPoolRecord], expected_count: usize) {
    if expected_count < 2 || pools.len() != expected_count {
        add_blocked_check(
            report,
            "serving/pools/independent-simulator-targets",
            &format!(
                "need complete ready backend target records for at least two pools; expected={expected_count}, observed={}",
                pools.len()
            ),
        );
        return;
    }

    match pool_backend_identity_separation(pools) {
        PoolBackendIdentityCheck::Incomplete(detail) => {
            add_blocked_check(report, "serving/pools/independent-simulator-targets", &detail);
        },
        PoolBackendIdentityCheck::Overlap(detail) => {
            add_failed_check(report, "serving/pools/independent-simulator-targets", &detail);
        },
        PoolBackendIdentityCheck::Distinct(detail) => {
            record_boolean_check(report, "serving/pools/independent-simulator-targets", true, detail);
        },
    }
}

fn pool_backend_identity_separation(pools: &[ServingPoolRecord]) -> PoolBackendIdentityCheck {
    let mut backend_uids = Vec::with_capacity(pools.len());
    for pool in pools {
        let uids = pool
            .backend_ready_pod_uids
            .iter()
            .filter(|uid| !uid.is_empty())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        if uids.is_empty() || uids.len() != pool.backend_ready_pod_uids.len() {
            return PoolBackendIdentityCheck::Incomplete(format!(
                "pool {} has no complete set of ready simulator Pod UIDs",
                pool.pool
            ));
        }
        backend_uids.push((pool.pool.as_str(), uids));
    }

    let mut overlaps = Vec::new();
    for left in 0..backend_uids.len() {
        for right in (left + 1)..backend_uids.len() {
            let shared = backend_uids[left]
                .1
                .intersection(&backend_uids[right].1)
                .cloned()
                .collect::<Vec<_>>();
            if !shared.is_empty() {
                overlaps.push(format!(
                    "{} and {} share ready simulator Pod UID(s) {:?}",
                    backend_uids[left].0, backend_uids[right].0, shared
                ));
            }
        }
    }
    if overlaps.is_empty() {
        PoolBackendIdentityCheck::Distinct(format!(
            "{} pools target pairwise-disjoint ready simulator Pod UID sets",
            backend_uids.len()
        ))
    } else {
        PoolBackendIdentityCheck::Overlap(overlaps.join("; "))
    }
}

fn resource_identity(resource: &ResourceRef) -> String {
    format!("{}/{}/{}", resource.namespace, resource.kind, resource.name)
}

fn parse_pool_resource_mappings(
    values: &[String],
    default_namespace: &str,
    accepted_kinds: &[&str],
) -> (BTreeMap<String, ResourceRef>, Vec<String>) {
    let mut mappings = BTreeMap::new();
    let mut errors = Vec::new();
    for raw in values {
        let Some((pool, reference)) = raw.split_once('=') else {
            errors.push(format!("{raw:?} must be pool-name=resource/name"));
            continue;
        };
        let parsed = match parse_resource_ref(reference, default_namespace) {
            Ok(parsed) if is_resource_kind(&parsed.kind, accepted_kinds) => parsed,
            Ok(parsed) => {
                errors.push(format!("{raw:?} has unsupported resource kind {:?}", parsed.kind));
                continue;
            },
            Err(error) => {
                errors.push(error);
                continue;
            },
        };
        if pool.is_empty() || mappings.insert(pool.to_owned(), parsed).is_some() {
            errors.push(format!("duplicate or empty pool mapping {pool:?}"));
        }
    }
    (mappings, errors)
}

fn collect_serving_namespace_snapshot(
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    namespace: &str,
) -> Result<Option<Box<ServingNamespaceSnapshot>>, String> {
    let mut captured = BTreeMap::new();
    for resource in ["pods", "endpointslices", "replicasets", "deployments"] {
        let capture = run_command(
            "kubectl",
            &[
                "--context",
                context,
                "--namespace",
                namespace,
                "get",
                resource,
                "-o",
                "json",
            ],
        )?;
        write_capture(
            evidence_dir,
            &format!("serving-graph-{}-{resource}", safe_name(namespace)),
            &capture,
        )?;
        if !capture.success() {
            add_blocked_check(
                report,
                &format!("serving/namespace/{namespace}/{resource}"),
                &format!("could not read namespace {resource}"),
            );
            return Ok(None);
        }
        let value = match serde_json::from_str::<Value>(&capture.stdout()) {
            Ok(value) => value,
            Err(error) => {
                add_blocked_check(
                    report,
                    &format!("serving/namespace/{namespace}/{resource}"),
                    &format!("namespace {resource} returned invalid JSON: {error}"),
                );
                return Ok(None);
            },
        };
        if resource == "pods" {
            record_pod_images(report, &capture.stdout());
        }
        captured.insert(resource, value);
    }
    Ok(Some(Box::new(ServingNamespaceSnapshot {
        pods: captured.remove("pods").unwrap_or_default(),
        endpoint_slices: captured.remove("endpointslices").unwrap_or_default(),
        replica_sets: captured.remove("replicasets").unwrap_or_default(),
        deployments: captured.remove("deployments").unwrap_or_default(),
    })))
}

fn read_resource_json(
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    resource_ref: &ResourceRef,
    evidence_name: &str,
) -> Result<Option<Value>, String> {
    let capture = get_resource(
        evidence_dir,
        context,
        Some(&resource_ref.namespace),
        &resource_ref.kind,
        &resource_ref.name,
        evidence_name,
    )?;
    if !capture.success() {
        add_blocked_check(
            report,
            &format!("serving/resource/{evidence_name}"),
            "serving resource does not exist or cannot be read",
        );
        return Ok(None);
    }
    parse_capture_json(&capture, "serving resource").map(Some)
}

fn current_pool_status_ready(pool: &Value) -> Option<bool> {
    let generation = pool["metadata"]["generation"].as_u64();
    if let Some(conditions) = pool["status"]["conditions"]
        .as_array()
        .filter(|items| !items.is_empty())
    {
        return Some(conditions.iter().any(|condition| {
            let condition_type = condition["type"].as_str().unwrap_or_default();
            let observed = condition["observedGeneration"]
                .as_u64()
                .or_else(|| pool["status"]["observedGeneration"].as_u64());
            matches!(condition_type, "Ready" | "Accepted" | "Programmed")
                && condition["status"].as_str() == Some("True")
                && generation.is_none_or(|generation| observed == Some(generation))
        }));
    }

    let parents = pool["status"]["parents"].as_array()?;
    if parents.is_empty() {
        return None;
    }
    Some(parents.iter().all(|parent| {
        let conditions = parent["conditions"].as_array();
        ["Accepted", "ResolvedRefs"].iter().all(|condition_type| {
            conditions.is_some_and(|items| {
                items.iter().any(|condition| {
                    let observed = condition["observedGeneration"].as_u64();
                    condition["type"].as_str() == Some(condition_type)
                        && condition["status"].as_str() == Some("True")
                        && generation.is_none_or(|wanted| observed == Some(wanted))
                })
            })
        })
    }))
}

fn inspect_service_targets(
    report: &mut Report,
    context: &str,
    evidence_dir: &Path,
    namespace_snapshot: &ServingNamespaceSnapshot,
    service_ref: &ResourceRef,
    pool_selector: Option<&Value>,
    expected_port: Option<u64>,
    expected_image_digest: Option<&str>,
    component: &str,
) -> Result<Option<ServiceTargetInspection>, String> {
    let Some(service) = read_resource_json(
        report,
        evidence_dir,
        context,
        service_ref,
        &format!(
            "serving-service-{}-{}",
            safe_name(&service_ref.namespace),
            safe_name(&service_ref.name)
        ),
    )?
    else {
        return Ok(None);
    };
    Ok(Some(inspect_service_target_objects(
        service_ref,
        &service,
        namespace_snapshot,
        pool_selector,
        expected_port,
        expected_image_digest,
        component,
    )))
}

fn inspect_service_target_objects(
    service_ref: &ResourceRef,
    service: &Value,
    namespace_snapshot: &ServingNamespaceSnapshot,
    pool_selector: Option<&Value>,
    expected_port: Option<u64>,
    expected_image_digest: Option<&str>,
    component: &str,
) -> ServiceTargetInspection {
    let service_name = service["metadata"]["name"].as_str().unwrap_or_default();
    let service_selector = &service["spec"]["selector"];
    let selector_valid = nonempty_string_map(service_selector);
    let port_matches = expected_port.is_none_or(|expected| {
        service["spec"]["ports"]
            .as_array()
            .is_some_and(|ports| ports.iter().any(|port| port["port"].as_u64() == Some(expected)))
    });
    let selected_slices = &namespace_snapshot.endpoint_slices;
    let matching_slices = selected_slices["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|slice| slice["metadata"]["labels"]["kubernetes.io/service-name"].as_str() == Some(service_name))
        .collect::<Vec<_>>();
    let mut target_uids = std::collections::BTreeSet::new();
    let mut deployments = std::collections::BTreeSet::new();
    let mut image_ids = std::collections::BTreeSet::new();
    let mut ready_endpoint_count = 0_usize;
    let mut every_target_valid = selector_valid && port_matches && !matching_slices.is_empty();
    for endpoint in matching_slices
        .iter()
        .flat_map(|slice| slice["endpoints"].as_array().into_iter().flatten())
    {
        if endpoint["conditions"]["ready"].as_bool() != Some(true) {
            continue;
        }
        ready_endpoint_count += 1;
        let target_ref = &endpoint["targetRef"];
        let pod_name = target_ref["name"].as_str().unwrap_or_default();
        let pod_uid = target_ref["uid"].as_str().unwrap_or_default();
        let target_namespace = target_ref["namespace"].as_str().unwrap_or(&service_ref.namespace);
        if target_ref["kind"].as_str() != Some("Pod")
            || pod_name.is_empty()
            || pod_uid.is_empty()
            || target_namespace != service_ref.namespace
        {
            every_target_valid = false;
            continue;
        }
        let Some(pod) = namespace_snapshot.pods["items"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|pod| {
                pod["metadata"]["name"].as_str() == Some(pod_name) && pod["metadata"]["uid"].as_str() == Some(pod_uid)
            })
        else {
            every_target_valid = false;
            continue;
        };
        let labels = &pod["metadata"]["labels"];
        let service_selected = labels_match(labels, service_selector);
        let pool_selected = pool_selector.is_none_or(|selector| labels_match(labels, &selector["matchLabels"]));
        let ready = pod_is_ready(pod);
        let deployment =
            serving_deployment_for_pod(pod, &namespace_snapshot.replica_sets, &namespace_snapshot.deployments);
        let mut pod_image_matches = false;
        for status in pod["status"]["containerStatuses"].as_array().into_iter().flatten() {
            let image_id = status["imageID"].as_str().unwrap_or_default();
            if status["ready"].as_bool() == Some(true)
                && expected_image_digest.is_some_and(|digest| runtime_image_digest(image_id).as_deref() == Some(digest))
            {
                pod_image_matches = true;
                if !image_id.is_empty() {
                    image_ids.insert(image_id.to_owned());
                }
            }
        }
        every_target_valid &= service_selected && pool_selected && ready && deployment.is_some() && pod_image_matches;
        if ready && service_selected && pool_selected {
            target_uids.insert(pod_uid.to_owned());
        }
        if let Some(deployment) = deployment {
            deployments.insert(deployment);
        }
    }
    let passed = every_target_valid
        && ready_endpoint_count > 0
        && !target_uids.is_empty()
        && expected_image_digest.is_some()
        && !image_ids.is_empty();
    ServiceTargetInspection {
        passed,
        detail: format!(
            "service={}/{}, selectorValid={selector_valid}, endpointSlices={}, readyEndpoints={ready_endpoint_count}, readyPodTargets={}, controllerDeployments={}, {component}DigestMatchedPods={}, portMatches={port_matches}",
            service_ref.namespace,
            service_name,
            matching_slices.len(),
            target_uids.len(),
            deployments.len(),
            image_ids.len(),
        ),
        pod_uids: target_uids.into_iter().collect(),
        deployment_names: deployments.into_iter().collect(),
        image_ids: image_ids.into_iter().collect(),
    }
}

fn nonempty_string_map(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|entries| !entries.is_empty() && entries.values().all(Value::is_string))
}

fn inference_pool_service_selector(pool: &Value) -> Option<&Value> {
    let selector = &pool["spec"]["selector"];
    selector["matchLabels"]
        .as_object()
        .filter(|labels| !labels.is_empty())
        .map(|_| selector)
}

fn labels_match(labels: &Value, selector: &Value) -> bool {
    selector.as_object().is_some_and(|entries| {
        !entries.is_empty()
            && entries.iter().all(|(key, expected)| {
                expected
                    .as_str()
                    .is_some_and(|expected| labels[key].as_str() == Some(expected))
            })
    })
}

fn pod_is_ready(pod: &Value) -> bool {
    pod["status"]["phase"].as_str() == Some("Running")
        && pod["status"]["conditions"].as_array().is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition["type"].as_str() == Some("Ready") && condition["status"].as_str() == Some("True")
            })
        })
}

fn serving_deployment_for_pod(pod: &Value, replica_sets: &Value, deployments: &Value) -> Option<String> {
    let owners = pod["metadata"]["ownerReferences"].as_array()?;
    let controller = owners
        .iter()
        .find(|owner| owner["controller"].as_bool() == Some(true))
        .or_else(|| owners.first())?;
    let (deployment_name, deployment_uid) = match controller["kind"].as_str()? {
        "Deployment" => (
            controller["name"].as_str()?.to_owned(),
            controller["uid"].as_str()?.to_owned(),
        ),
        "ReplicaSet" => {
            let replica_set = replica_sets["items"].as_array()?.iter().find(|replica_set| {
                replica_set["metadata"]["name"].as_str() == controller["name"].as_str()
                    && replica_set["metadata"]["uid"].as_str() == controller["uid"].as_str()
            })?;
            let owner = replica_set["metadata"]["ownerReferences"]
                .as_array()?
                .iter()
                .find(|owner| owner["kind"].as_str() == Some("Deployment"))?;
            (owner["name"].as_str()?.to_owned(), owner["uid"].as_str()?.to_owned())
        },
        _ => return None,
    };
    let deployment = deployments["items"].as_array()?.iter().find(|deployment| {
        deployment["metadata"]["name"].as_str() == Some(deployment_name.as_str())
            && deployment["metadata"]["uid"].as_str() == Some(deployment_uid.as_str())
    })?;
    let (ready, _) = deployment_readiness(&serde_json::to_string(deployment).ok()?);
    let selector = &deployment["spec"]["selector"]["matchLabels"];
    (ready && labels_match(&pod["metadata"]["labels"], selector)).then_some(deployment_name)
}

fn check_ipp_inventory(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
) -> Result<(), String> {
    match args.ipp_state {
        IppState::Unknown => {
            add_blocked_check(
                report,
                "serving/ipp-inventory",
                "IPP participation in this serving path has not been established",
            );
        },
        IppState::Absent => {
            let runtime_ipp_image_found = report.runtime_images.iter().any(|record| {
                let identity = format!("{} {} {}", record.image, record.container, record.pod).to_ascii_lowercase();
                identity.contains("payload-processing") || identity.contains("/ipp") || identity.ends_with("ipp")
            });
            record_boolean_check(
                report,
                "serving/ipp-inventory",
                !runtime_ipp_image_found && args.ipp_resources.is_empty(),
                if runtime_ipp_image_found {
                    "caller declared IPP absent, but a captured serving-path image looks like IPP".to_owned()
                } else if args.ipp_resources.is_empty() {
                    "caller verified IPP is absent from this serving path; captured serving-path runtime images contain no IPP identity".to_owned()
                } else {
                    "--ipp-resource was supplied while --ipp-state=absent".to_owned()
                },
            );
        },
        IppState::Present => {
            if args.ipp_resources.is_empty() {
                add_blocked_check(
                    report,
                    "serving/ipp-inventory",
                    "IPP is declared present but no IPP resource/configuration reference was supplied",
                );
                return Ok(());
            }
            let mut all_readable = true;
            for (index, raw) in args.ipp_resources.iter().enumerate() {
                let resource_ref = parse_resource_ref(raw, &args.namespace)?;
                if is_resource_kind(&resource_ref.kind, &["secret", "secrets"]) {
                    add_blocked_check(
                        report,
                        &format!("serving/ipp-resource-{}", index + 1),
                        "raw Secret objects are not captured as IPP configuration evidence",
                    );
                    all_readable = false;
                    continue;
                }
                let object = read_resource_json(
                    report,
                    evidence_dir,
                    context,
                    &resource_ref,
                    &format!("serving-ipp-resource-{}", index + 1),
                )?;
                let read = object.is_some();
                all_readable &= read;
                record_boolean_check(
                    report,
                    &format!("serving/ipp-resource-{}", index + 1),
                    read,
                    format!(
                        "captured sanitized IPP configuration resource {}/{}/{}",
                        resource_ref.namespace, resource_ref.kind, resource_ref.name
                    ),
                );
            }
            record_boolean_check(
                report,
                "serving/ipp-inventory",
                all_readable,
                format!(
                    "{} explicitly named IPP configuration/attachment resource(s) were read with sensitive values redacted",
                    args.ipp_resources.len()
                ),
            );
        },
    }
    Ok(())
}

fn check_simulator_control_pool_coverage(args: &GridLlmdArgs, report: &mut Report) -> Result<(), String> {
    if !args.require_signal_transition {
        return Ok(());
    }
    let mut pool_names = std::collections::BTreeSet::new();
    for reference in &args.pool_resources {
        let resource = reference.split('#').next().unwrap_or(reference);
        let pool = parse_resource_ref(resource, &args.namespace)?;
        pool_names.insert(pool.name);
    }
    let mut control_pools = std::collections::BTreeSet::new();
    let mut valid = pool_names.len() == args.pool_resources.len() && pool_names.len() >= 2;
    for raw in &args.simulator_control_resources {
        let control = parse_simulator_control_ref(raw, &args.namespace)?;
        match control.pool {
            Some(pool) if pool_names.contains(&pool) => valid &= control_pools.insert(pool),
            _ => valid = false,
        }
    }
    valid &= control_pools == pool_names;
    record_boolean_check(
        report,
        "simulator/control-pool-coverage",
        valid,
        if valid {
            format!(
                "one simulator control is mapped to each pool: {}",
                pool_names.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        } else {
            "each distinct InferencePool needs one control reference tagged with @<pool-name>".to_owned()
        },
    );
    if !valid {
        add_blocked_check(
            report,
            "simulator/control-pool-coverage",
            "simulator controls do not map one-to-one to the configured InferencePools",
        );
    }
    Ok(())
}

fn check_deployment(
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    namespace: &str,
    deployment: &str,
) -> Result<(), String> {
    let args = vec![
        "--context".to_owned(),
        context.to_owned(),
        "--namespace".to_owned(),
        namespace.to_owned(),
        "get".to_owned(),
        "deployment".to_owned(),
        deployment.to_owned(),
        "-o".to_owned(),
        "json".to_owned(),
    ];
    let capture = run_command_owned("kubectl", &args)?;
    write_capture(evidence_dir, &format!("deployment-{}", safe_name(deployment)), &capture)?;
    let (passed, detail) = if capture.success() {
        let stdout = capture.stdout();
        record_deployment_image(report, namespace, deployment, &stdout);
        deployment_readiness(&stdout)
    } else {
        (false, capture.stderr().trim().to_owned())
    };
    report.checks.push(Check {
        name: format!("deployment/{namespace}/{deployment}"),
        status: if passed { "pass" } else { "fail" }.to_owned(),
        detail,
        observed_epoch_seconds: epoch_seconds(),
    });
    Ok(())
}

fn deployment_readiness(json: &str) -> (bool, String) {
    let Ok(value) = serde_json::from_str::<Value>(json) else {
        return (false, "kubectl returned invalid deployment JSON".to_owned());
    };
    let desired = value["spec"]["replicas"].as_u64().unwrap_or(1);
    let available = value["status"]["availableReplicas"].as_u64().unwrap_or(0);
    let ready = value["status"]["readyReplicas"].as_u64().unwrap_or(0);
    let generation = value["metadata"]["generation"].as_u64();
    let observed_generation = value["status"]["observedGeneration"].as_u64();
    let current = generation.is_none_or(|wanted| observed_generation.is_some_and(|seen| seen >= wanted));
    (
        current && desired > 0 && ready >= desired && available >= desired,
        format!(
            "desired={desired}, ready={ready}, available={available}, observedGeneration={observed_generation:?}, generation={generation:?}"
        ),
    )
}

fn collect_snapshots(
    report: &mut Report,
    evidence_dir: &Path,
    context: &str,
    namespace: &str,
    deployments: &[String],
    grid_operator_deployment: Option<&str>,
) -> Result<(), String> {
    let mut all_deployments = deployments.to_vec();
    if let Some(operator_deployment) = grid_operator_deployment
        && !all_deployments.iter().any(|existing| existing == operator_deployment)
    {
        all_deployments.push(operator_deployment.to_owned());
    }
    let snapshots = [
        ("pods", vec!["get", "pods", "-o", "json"]),
        ("services", vec!["get", "services", "-o", "json"]),
        ("endpointslices", vec!["get", "endpointslices", "-o", "json"]),
        (
            "grid-resources",
            vec![
                "get",
                "gridnetworks.grid.praxis.fast,gridsites.grid.praxis.fast,inferenceproviders.grid.praxis.fast",
                "-o",
                "json",
            ],
        ),
        (
            "integration-resources",
            vec!["get", "llminferenceservices,inferencepools", "-o", "json"],
        ),
        (
            "httproutes",
            vec!["get", "httproutes.gateway.networking.k8s.io", "-o", "json"],
        ),
        (
            "gateways",
            vec!["get", "gateways.gateway.networking.k8s.io", "-o", "json"],
        ),
        (
            "envoyfilters",
            vec!["get", "envoyfilters.networking.istio.io", "-o", "json"],
        ),
        (
            "destinationrules",
            vec!["get", "destinationrules.networking.istio.io", "-o", "json"],
        ),
        (
            "events",
            vec!["get", "events", "--sort-by=.lastTimestamp", "-o", "json"],
        ),
        ("nodes", vec!["get", "nodes", "-o", "json"]),
    ];
    for (name, kubectl_args) in snapshots {
        let mut args = vec![
            "--context".to_owned(),
            context.to_owned(),
            "--namespace".to_owned(),
            namespace.to_owned(),
        ];
        args.extend(kubectl_args.into_iter().map(str::to_owned));
        let capture = run_command_owned("kubectl", &args)?;
        write_capture(evidence_dir, &format!("snapshot-{name}"), &capture)?;
        if name == "pods" && capture.success() {
            record_pod_images(report, &capture.stdout());
        }
        report.checks.push(Check {
            name: format!("snapshot/{name}"),
            status: if capture.success() { "pass" } else { "warn" }.to_owned(),
            detail: format!("exit status {}", capture.status()),
            observed_epoch_seconds: epoch_seconds(),
        });
    }
    let external_namespaces = all_deployments
        .iter()
        .filter_map(|raw| parse_deployment_ref(raw, namespace).ok())
        .map(|reference| reference.namespace)
        .filter(|candidate| candidate != namespace)
        .collect::<std::collections::BTreeSet<_>>();
    for extra_namespace in external_namespaces {
        let capture = run_command(
            "kubectl",
            &[
                "--context",
                context,
                "--namespace",
                &extra_namespace,
                "get",
                "pods",
                "-o",
                "json",
            ],
        )?;
        let label = format!("pods-{}", safe_name(&extra_namespace));
        write_capture(evidence_dir, &format!("snapshot-{label}"), &capture)?;
        if capture.success() {
            record_pod_images(report, &capture.stdout());
        }
        report.checks.push(Check {
            name: format!("snapshot/{label}"),
            status: if capture.success() { "pass" } else { "warn" }.to_owned(),
            detail: format!("exit status {}", capture.status()),
            observed_epoch_seconds: epoch_seconds(),
        });
        for resource_kind in ["services", "endpointslices", "events"] {
            let capture = run_command(
                "kubectl",
                &[
                    "--context",
                    context,
                    "--namespace",
                    &extra_namespace,
                    "get",
                    resource_kind,
                    "-o",
                    "json",
                ],
            )?;
            write_capture(
                evidence_dir,
                &format!("snapshot-{}-{}", safe_name(&extra_namespace), resource_kind),
                &capture,
            )?;
            report.checks.push(Check {
                name: format!("snapshot/{extra_namespace}/{resource_kind}"),
                status: if capture.success() { "pass" } else { "warn" }.to_owned(),
                detail: format!("exit status {}", capture.status()),
                observed_epoch_seconds: epoch_seconds(),
            });
        }
    }
    for deployment in &all_deployments {
        let reference = parse_deployment_ref(deployment, namespace)?;
        let capture = run_command(
            "kubectl",
            &[
                "--context",
                context,
                "--namespace",
                &reference.namespace,
                "logs",
                &format!("deployment/{}", reference.name),
                "--all-containers=true",
                "--prefix=true",
                "--tail=200",
            ],
        )?;
        let label = format!("{}-{}", reference.namespace, reference.name);
        write_capture(evidence_dir, &format!("logs-{}", safe_name(&label)), &capture)?;
        report.checks.push(Check {
            name: format!("logs/{}/{}", reference.namespace, reference.name),
            status: if capture.success() { "pass" } else { "warn" }.to_owned(),
            detail: format!("sanitized bounded logs; exit status {}", capture.status()),
            observed_epoch_seconds: epoch_seconds(),
        });
    }
    Ok(())
}

fn record_deployment_image(report: &mut Report, namespace: &str, deployment: &str, json: &str) {
    let Ok(object) = serde_json::from_str::<Value>(json) else {
        return;
    };
    for container in object["spec"]["template"]["spec"]["containers"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let name = container["name"].as_str().unwrap_or_default().to_owned();
        let image = container["image"].as_str().unwrap_or_default().to_owned();
        let record = RuntimeImageRecord {
            namespace: namespace.to_owned(),
            pod: format!("deployment/{namespace}/{deployment}"),
            pod_uid: String::new(),
            container: name,
            image,
            image_id: String::new(),
            ready: false,
        };
        if !report
            .runtime_images
            .iter()
            .any(|existing| existing.pod == record.pod && existing.container == record.container)
        {
            report.runtime_images.push(record);
        }
    }
}

fn record_pod_images(report: &mut Report, json: &str) {
    let Ok(object) = serde_json::from_str::<Value>(json) else {
        return;
    };
    for pod in object["items"].as_array().into_iter().flatten() {
        let namespace = pod["metadata"]["namespace"].as_str().unwrap_or_default();
        let pod_name = pod["metadata"]["name"].as_str().unwrap_or_default();
        let pod_uid = pod["metadata"]["uid"].as_str().unwrap_or_default();
        let pod_ready = pod["status"]["phase"].as_str() == Some("Running")
            && pod["status"]["conditions"].as_array().is_some_and(|conditions| {
                conditions.iter().any(|condition| {
                    condition["type"].as_str() == Some("Ready") && condition["status"].as_str() == Some("True")
                })
            });
        for container in pod["status"]["containerStatuses"].as_array().into_iter().flatten() {
            let record = RuntimeImageRecord {
                namespace: namespace.to_owned(),
                pod: pod_name.to_owned(),
                pod_uid: pod_uid.to_owned(),
                container: container["name"].as_str().unwrap_or_default().to_owned(),
                image: container["image"].as_str().unwrap_or_default().to_owned(),
                image_id: container["imageID"].as_str().unwrap_or_default().to_owned(),
                ready: pod_ready && container["ready"].as_bool() == Some(true),
            };
            if !report.runtime_images.iter().any(|existing| {
                existing.pod == record.pod && existing.container == record.container && !existing.image_id.is_empty()
            }) {
                report.runtime_images.push(record);
            }
        }
    }
}

fn run_hook(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    hook: &Path,
    phase: &str,
    pressure_pool: Option<&str>,
) -> Result<String, String> {
    let capture = run_hook_command(args, hook, evidence_dir, &[], pressure_pool)?;
    write_capture(evidence_dir, &format!("hook-{phase}"), &capture)?;
    let passed = capture.success();
    let stdout = capture.stdout();
    report.checks.push(Check {
        name: format!("hook/{phase}"),
        status: if passed { "pass" } else { "fail" }.to_owned(),
        detail: if passed {
            "hook completed".to_owned()
        } else {
            capture.stderr().trim().to_owned()
        },
        observed_epoch_seconds: epoch_seconds(),
    });
    if passed {
        Ok(stdout)
    } else {
        Err(format!("{phase} hook failed with exit status {}", capture.status()))
    }
}

fn run_hook_command(
    args: &GridLlmdArgs,
    hook: &Path,
    evidence_dir: &Path,
    extra_args: &[String],
    pressure_pool: Option<&str>,
) -> Result<CommandCapture, String> {
    let hook_args = [
        args.timeout_seconds.to_string(),
        hook.display().to_string(),
        "--context".to_owned(),
        args.context.clone().unwrap_or_default(),
        "--namespace".to_owned(),
        args.namespace.clone(),
        "--evidence-dir".to_owned(),
        evidence_dir.display().to_string(),
    ];
    let mut command = Command::new("timeout");
    command.args(&hook_args);
    command.args(extra_args);
    if let Some(pressure_pool) = pressure_pool {
        command.env("GRID_LLMD_PRESSURE_POOL", pressure_pool);
    }
    if let Some(config) = KUBE_COMMAND_CONFIG.get() {
        if let Some(kubeconfig) = config.kubeconfig.as_ref() {
            command.env("KUBECONFIG", kubeconfig);
        }
        if let Some(api_ca_file) = config.api_ca_file.as_ref() {
            command.env("KUBE_CA_FILE", api_ca_file);
        }
        if config.allow_insecure_dev_api {
            command.env("CLUSTER_ENV", "ephemeral-dev");
            command.env("ALLOW_INSECURE_DEV_API", "true");
        }
    }
    let output = command
        .output()
        .map_err(|error| format!("run hook {}: {error}", hook.display()))?;
    Ok(CommandCapture { output })
}

// The qualification probe intentionally keeps the complete request/revision/oracle
// evidence for one bounded request in this scope. Payloads and response captures
// remain heap-backed or file-backed; the static frame is only slightly above the
// repository's conservative Clippy threshold.
#[expect(
    clippy::large_stack_frames,
    reason = "one bounded probe keeps revision, retry, request, and oracle evidence together; variable payloads are heap/file backed"
)]
fn run_probe(args: &GridLlmdArgs, report: &mut Report, evidence_dir: &Path, phase: &str) -> Result<Probe, String> {
    let retry_observation_before = report
        .gateway_revisions
        .last()
        .filter(|observation| observation.phase == phase)
        .cloned()
        .ok_or_else(|| format!("Gateway revision and retry observation is missing before {phase} request"))?;
    let grid_revision = report
        .grid_revisions
        .iter()
        .rev()
        .find(|revision| revision.phase == phase)
        .cloned()
        .ok_or_else(|| format!("Grid overlay observation is missing before {phase} request"))?;
    let expected_pool = expected_probe_pool(args, report, phase, &grid_revision)?;
    let grid_revision = Box::new(grid_revision);
    let gateway_url = args
        .gateway_url
        .as_deref()
        .ok_or_else(|| "--gateway-url is required for inference probes".to_owned())?;
    let url = format!(
        "{}/{}",
        gateway_url.trim_end_matches('/'),
        args.gateway_path.trim_start_matches('/')
    );
    let request_path = evidence_dir.join(format!("request-{phase}.json"));
    let headers_path = evidence_dir.join(format!("response-{phase}.headers"));
    let response_path = evidence_dir.join(format!("response-{phase}.body"));
    let stderr_path = evidence_dir.join(format!("probe-{phase}.stderr"));
    let request_id = format!(
        "grid-llmd-{}-{}-{phase}",
        report.started_epoch_seconds,
        std::process::id()
    );
    let body = render_request_body(&args.model, &args.prompt);
    let (request_file, request_body_sha256) = create_serialized_request_file(&body)?;
    write_json_private(&request_path, &redacted_request_body(&body))?;
    let auth_value = request_auth_header_value(args.auth_token.as_deref(), args.no_auth, &args.auth_header)?;
    if !valid_header_name(&args.request_id_header) {
        return Err("request ID header name is invalid".to_owned());
    }
    let model_header = request_model_header(&args.model_header, &args.model)?;
    let auth_config = auth_value.map_or_else(String::new, |value| {
        format!(
            "header = {}\n",
            curl_config_quote(&format!("{}: {value}", args.auth_header))
        )
    });
    let auth_token = args.auth_token.as_deref().unwrap_or_default();
    let mut curl_args = vec![
        "--config".to_owned(),
        "-".to_owned(),
        "--silent".to_owned(),
        "--show-error".to_owned(),
        "--max-time".to_owned(),
        args.timeout_seconds.to_string(),
        "--connect-timeout".to_owned(),
        args.timeout_seconds.min(30).to_string(),
        "--retry".to_owned(),
        "0".to_owned(),
    ];
    if let Some(connect_to) = args.gateway_connect_to.as_deref() {
        curl_args.extend(["--connect-to".to_owned(), connect_to.to_owned()]);
    }
    if let Some(ca_file) = args.gateway_ca_file.as_ref() {
        curl_args.extend(["--cacert".to_owned(), ca_file.display().to_string()]);
    }
    curl_args.push("--request".to_owned());
    curl_args.push("POST".to_owned());
    curl_args.push("--header".to_owned());
    curl_args.push("content-type: application/json".to_owned());
    curl_args.push("--header".to_owned());
    curl_args.push(model_header);
    curl_args.push("--header".to_owned());
    curl_args.push(format!("{}: {request_id}", args.request_id_header));
    curl_args.push("--header".to_owned());
    curl_args.push("connection: close".to_owned());
    curl_args.push("--data-binary".to_owned());
    curl_args.push(format!("@{}", request_file.path().display()));
    curl_args.push("--dump-header".to_owned());
    curl_args.push(headers_path.display().to_string());
    curl_args.push("--output".to_owned());
    curl_args.push(response_path.display().to_string());
    curl_args.push("--write-out".to_owned());
    curl_args.push("%{http_code}".to_owned());
    curl_args.push(url);
    let capture = run_command_with_stdin("curl", &curl_args, auth_config.as_bytes())?;
    drop(request_file);
    let retry_observation_after = wait_for_gateway_revision(
        args,
        report,
        evidence_dir,
        &grid_revision,
        &format!("{phase}-after-request"),
    )?;
    let (no_retry_attribution, upstream_request_delta, retry_attempt_delta) =
        gateway_retry_attribution(&retry_observation_before, &retry_observation_after);
    record_boolean_check(
        report,
        &format!("gateway/{phase}-retry-attribution"),
        no_retry_attribution,
        format!(
            "configured retry limit={}, upstream request delta={upstream_request_delta:?}, retry attempt delta={retry_attempt_delta:?}, same Gateway/Envoy instances={}",
            retry_observation_after.route_retry_limit,
            retry_observation_after.pod_uid == retry_observation_before.pod_uid
                && retry_observation_after.proxy_pod_uid == retry_observation_before.proxy_pod_uid
                && retry_observation_after.route_cluster_sha256 == retry_observation_before.route_cluster_sha256
        ),
    );
    let stderr = redact_probe_text(
        &capture.stderr(),
        auth_token,
        args.gateway_url.as_deref(),
        &args.auth_header,
        &args.prompt,
    );
    write_private(&stderr_path, stderr.as_bytes())?;
    let status_code = capture.stdout().trim().parse::<u16>().ok();
    let raw_headers = fs::read_to_string(&headers_path).unwrap_or_default();
    let headers = sanitize_response_headers(
        &raw_headers,
        &args.auth_header,
        auth_token,
        args.gateway_url.as_deref(),
        &args.prompt,
    );
    write_private(&headers_path, headers.as_bytes())?;
    let simulator_receipt_match =
        collect_simulator_receipt(args, report, evidence_dir, phase, &expected_pool, &request_id)?;
    let response_body = fs::read_to_string(&response_path).unwrap_or_default();
    let response_body = redact_probe_text(
        &response_body,
        auth_token,
        args.gateway_url.as_deref(),
        &args.auth_header,
        &args.prompt,
    );
    let response_body = sanitize_capture_text(&response_body);
    write_private(&response_path, response_body.as_bytes())?;
    let provider_attribution = header_value(&headers, &args.provider_header);
    let backend_attribution = header_value(&headers, &args.backend_header);
    let expected_attribution = configured_pool_attribution_for_name(args, &expected_pool);
    let attribution_matches = expected_attribution.is_some_and(|expected| {
        response_attribution_matches(
            provider_attribution.as_deref(),
            backend_attribution.as_deref(),
            expected,
        )
    });
    let extproc_markers = args
        .extproc_headers
        .iter()
        .filter_map(|expectation| {
            let (name, _) = parse_extproc_header_expectation(expectation)?;
            header_value(&headers, name).map(|value| (name.to_owned(), value))
        })
        .collect::<BTreeMap<_, _>>();
    let body_oracle_match = args
        .extproc_body_oracle
        .as_ref()
        .is_some_and(|header| body_oracle_header_matches(&headers, header, &request_body_sha256));
    let request_oracle_match = args
        .extproc_request_oracle
        .as_ref()
        .map(|oracle| response_body.contains(oracle));
    let expected_simulator_identity = configured_pool_response_oracle(args, &expected_pool);
    let simulator_identity_match = expected_simulator_identity.is_none_or(|oracle| response_body.contains(oracle));
    let simulator_request_id_echo_match =
        header_value(&headers, "x-request-id").as_deref() == Some(request_id.as_str());
    let extproc_proof = if args.extproc_headers.is_empty() {
        body_oracle_match
    } else {
        args.extproc_headers
            .iter()
            .all(|expectation| extproc_header_expectation_matches(&headers, expectation))
    };
    let passed = capture.success()
        && status_code.is_some_and(|status| (200..300).contains(&status))
        && provider_attribution.as_ref().is_some_and(|value| !value.is_empty())
        && backend_attribution.as_ref().is_some_and(|value| !value.is_empty())
        && attribution_matches
        && extproc_proof
        && (args.extproc_body_oracle.is_none() || body_oracle_match)
        && simulator_request_id_echo_match
        && simulator_receipt_match;
    let passed = passed && request_oracle_match.unwrap_or(true) && simulator_identity_match && no_retry_attribution;
    let probe = Probe {
        phase: phase.to_owned(),
        observed_epoch_seconds: epoch_seconds(),
        request_id,
        expected_pool: expected_pool.clone(),
        model_header: args.model_header.clone(),
        status_code,
        provider_attribution,
        backend_attribution,
        extproc_markers,
        request_body_sha256,
        body_oracle_match,
        request_oracle_match,
        simulator_identity_match,
        simulator_request_id_echo_match,
        simulator_receipt_match,
        gateway_upstream_request_delta: upstream_request_delta,
        gateway_retry_attempt_delta: retry_attempt_delta,
        headers_file: headers_path.display().to_string(),
        body_file: response_path.display().to_string(),
        stderr_file: stderr_path.display().to_string(),
        passed,
    };
    let detail = format!(
        "status={:?}, expected_pool={}, provider={:?}, backend={:?}, attribution_matches_expected={}, extproc_markers={:?}, request_body_sha256={}, body_oracle_match={}, request_oracle={:?}, simulator_identity={}, simulator_request_id_echo={}, simulator_log_receipt={}, upstream_request_delta={:?}, retry_attempt_delta={:?}",
        probe.status_code,
        probe.expected_pool,
        probe.provider_attribution,
        probe.backend_attribution,
        attribution_matches,
        probe.extproc_markers,
        probe.request_body_sha256,
        probe.body_oracle_match,
        probe.request_oracle_match,
        probe.simulator_identity_match,
        probe.simulator_request_id_echo_match,
        probe.simulator_receipt_match,
        probe.gateway_upstream_request_delta,
        probe.gateway_retry_attempt_delta
    );
    report.checks.push(Check {
        name: format!("probe/{phase}"),
        status: if passed { "pass" } else { "fail" }.to_owned(),
        detail,
        observed_epoch_seconds: epoch_seconds(),
    });
    report.probes.push(probe.clone());
    Ok(probe)
}

fn collect_simulator_receipt(
    args: &GridLlmdArgs,
    report: &mut Report,
    evidence_dir: &Path,
    phase: &str,
    pool: &str,
    request_id: &str,
) -> Result<bool, String> {
    let context = args.context.as_deref().unwrap_or_default();
    let expected_simulator_image = parse_image_pins(&args.image_pins)
        .0
        .get("llm-d-sim")
        .cloned()
        .ok_or_else(|| "the immutable llm-d-sim image pin is missing".to_owned())?;
    let mut pod_args = vec![
        "--context".to_owned(),
        context.to_owned(),
        "--namespace".to_owned(),
        args.namespace.clone(),
        "get".to_owned(),
        "pods".to_owned(),
        "-l".to_owned(),
        format!("praxis.fast/simulator-pool={pool}"),
        "-o".to_owned(),
        "json".to_owned(),
    ];
    let pod_capture = run_command_owned("kubectl", &pod_args)?;
    let mut pod_name = String::new();
    let mut pod_uid = String::new();
    let mut container_name = String::new();
    let mut receipt_match = false;
    if pod_capture.success() {
        let pods = parse_capture_json(&pod_capture, "simulator Pod list")?;
        let matching = pods["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|pod| {
                let ready = pod["status"]["conditions"].as_array().is_some_and(|conditions| {
                    conditions.iter().any(|condition| {
                        condition["type"].as_str() == Some("Ready") && condition["status"].as_str() == Some("True")
                    })
                });
                ready
                    && pod["metadata"]["labels"]["praxis.fast/simulator-pool"].as_str() == Some(pool)
                    && pod["spec"]["containers"].as_array().is_some_and(|containers| {
                        containers
                            .iter()
                            .any(|container| container["image"].as_str() == Some(expected_simulator_image.as_str()))
                    })
            })
            .collect::<Vec<_>>();
        if matching.len() == 1 {
            let pod = matching[0];
            pod_name = pod["metadata"]["name"].as_str().unwrap_or_default().to_owned();
            pod_uid = pod["metadata"]["uid"].as_str().unwrap_or_default().to_owned();
            container_name = pod["spec"]["containers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|container| container["image"].as_str() == Some(expected_simulator_image.as_str()))
                .and_then(|container| container["name"].as_str())
                .unwrap_or_default()
                .to_owned();
            if !pod_name.is_empty() && !container_name.is_empty() {
                pod_args = vec![
                    "--context".to_owned(),
                    context.to_owned(),
                    "--namespace".to_owned(),
                    args.namespace.clone(),
                    "logs".to_owned(),
                    pod_name.clone(),
                    "--container".to_owned(),
                    container_name.clone(),
                    "--since=5m".to_owned(),
                ];
                let logs = run_command_owned("kubectl", &pod_args)?;
                receipt_match = logs.success() && simulator_completion_log_matches(&logs.stdout(), request_id);
            }
        }
    }
    let result = serde_json::json!({
        "schema": "praxis-extproc.simulator-receipt/v1",
        "phase": phase,
        "pool": pool,
        "podName": pod_name,
        "podUid": pod_uid,
        "container": container_name,
        "requestId": request_id,
        "completionLogMatched": receipt_match,
        "rawSimulatorLogsPersisted": false,
    });
    write_json_private(
        &evidence_dir.join(format!("simulator-receipt-{}.json", safe_name(phase))),
        &result,
    )?;
    record_boolean_check(
        report,
        &format!("simulator/{phase}-independent-receipt"),
        receipt_match,
        format!(
            "pinned Ready simulator target {}/{} emitted a request-completion record correlated to the unique request ID; raw logs were not saved",
            args.namespace,
            if pod_name.is_empty() {
                "<unresolved>"
            } else {
                pod_name.as_str()
            }
        ),
    );
    Ok(receipt_match)
}

fn simulator_completion_log_matches(logs: &str, request_id: &str) -> bool {
    logs.lines()
        .any(|line| line.contains("Finished processing request") && line.contains(request_id))
}

fn create_serialized_request_file(body: &Value) -> Result<(tempfile::NamedTempFile, String), String> {
    let serialized = body.to_string();
    let digest = sha256_digest(serialized.as_bytes());
    let mut request_file = tempfile::Builder::new()
        .prefix("grid-llmd-request-")
        .tempfile()
        .map_err(|error| format!("create private temporary inference request: {error}"))?;
    request_file
        .write_all(serialized.as_bytes())
        .map_err(|error| format!("write private temporary inference request: {error}"))?;
    Ok((request_file, digest))
}

fn record_boolean_check(report: &mut Report, name: &str, passed: bool, detail: String) {
    report.checks.push(Check {
        name: name.to_owned(),
        status: if passed { "pass" } else { "fail" }.to_owned(),
        detail,
        observed_epoch_seconds: epoch_seconds(),
    });
}

fn resolve_evidence_dir(requested: Option<&Path>, epoch: u64) -> PathBuf {
    requested.map_or_else(
        || PathBuf::from(format!("target/e2e-grid-llmd/{epoch}")),
        Path::to_path_buf,
    )
}

fn run_command(program: &str, args: &[&str]) -> Result<CommandCapture, String> {
    run_command_owned(program, &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

fn run_command_owned(program: impl AsRef<Path>, args: &[String]) -> Result<CommandCapture, String> {
    let mut command = Command::new(program.as_ref());
    if let Some(config) = KUBE_COMMAND_CONFIG.get() {
        if let Some(kubeconfig) = config.kubeconfig.as_ref() {
            command.env("KUBECONFIG", kubeconfig);
        }
        if let Some(api_ca_file) = config.api_ca_file.as_ref() {
            command.env("KUBE_CA_FILE", api_ca_file);
        }
        if config.allow_insecure_dev_api {
            command.env("CLUSTER_ENV", "ephemeral-dev");
            command.env("ALLOW_INSECURE_DEV_API", "true");
        }
        if program.as_ref().file_name().and_then(|name| name.to_str()) == Some("kubectl") {
            if let Some(kubeconfig) = config.kubeconfig.as_ref() {
                command.arg("--kubeconfig").arg(kubeconfig);
            }
            if let Some(api_ca_file) = config.api_ca_file.as_ref() {
                command
                    .arg("--certificate-authority")
                    .arg(api_ca_file)
                    .arg("--insecure-skip-tls-verify=false");
            } else if config.allow_insecure_dev_api {
                command.arg("--insecure-skip-tls-verify=true");
            }
        }
    }
    let output = command
        .args(args)
        .output()
        .map_err(|error| format!("run {}: {error}", program.as_ref().display()))?;
    Ok(CommandCapture { output })
}

fn run_command_with_stdin(program: impl AsRef<Path>, args: &[String], input: &[u8]) -> Result<CommandCapture, String> {
    let mut child = Command::new(program.as_ref())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run {}: {error}", program.as_ref().display()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("open stdin for {}", program.as_ref().display()))?;
    stdin
        .write_all(input)
        .map_err(|error| format!("write stdin for {}: {error}", program.as_ref().display()))?;
    drop(stdin);
    let output = child
        .wait_with_output()
        .map_err(|error| format!("wait for {}: {error}", program.as_ref().display()))?;
    Ok(CommandCapture { output })
}

fn write_capture(evidence_dir: &Path, name: &str, capture: &CommandCapture) -> Result<(), String> {
    let content = format!(
        "exit_status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
        capture.status(),
        sanitize_capture_text(&capture.stdout()),
        sanitize_capture_text(&capture.stderr())
    );
    write_private(&evidence_dir.join(format!("{name}.log")), content.as_bytes())
}

fn write_report(evidence_dir: &Path, report: &Report) -> Result<(), String> {
    let mut value = serde_json::to_value(report).map_err(|error| format!("encode report: {error}"))?;
    redact_sensitive_json(&mut value, "");
    let json = serde_json::to_vec_pretty(&value).map_err(|error| format!("encode report: {error}"))?;
    write_private(&evidence_dir.join("report.json"), &json)
}

fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(value).map_err(|error| format!("encode {}: {error}", path.display()))?;
    write_private(path, &json)
}

fn write_private(path: &Path, content: &[u8]) -> Result<(), String> {
    fs::write(path, content).map_err(|error| format!("write {}: {error}", path.display()))?;
    set_private_permissions(path, 0o600)
}

fn set_private_permissions(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let permissions = fs::Permissions::from_mode(mode);
    fs::set_permissions(path, permissions)
        .map_err(|error| format!("set private permissions on {}: {error}", path.display()))
}

fn sanitize_capture_text(value: &str) -> String {
    let context = OUTPUT_REDACTIONS.get();
    if let Ok(mut json) = serde_json::from_str::<Value>(value) {
        redact_sensitive_json(&mut json, "");
        redact_json_string_values(&mut json, context);
        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "[REDACTED INVALID JSON]".to_owned())
    } else {
        let redacted = context.map_or_else(
            || redact_text(value, None, None),
            |context| {
                redact_probe_text(
                    value,
                    context.auth_token.as_deref().unwrap_or_default(),
                    context.gateway_url.as_deref(),
                    &context.auth_header,
                    &context.prompt,
                )
            },
        );
        redact_urls(&redacted)
    }
}

fn redact_json_string_values(value: &mut Value, context: Option<&OutputRedactions>) {
    match value {
        Value::Object(object) => {
            for child in object.values_mut() {
                redact_json_string_values(child, context);
            }
        },
        Value::Array(items) => {
            for item in items {
                redact_json_string_values(item, context);
            }
        },
        Value::String(text) => {
            *text = context.map_or_else(
                || redact_text(text, None, None),
                |context| {
                    redact_probe_text(
                        text,
                        context.auth_token.as_deref().unwrap_or_default(),
                        context.gateway_url.as_deref(),
                        &context.auth_header,
                        &context.prompt,
                    )
                },
            );
        },
        _ => {},
    }
}

fn redact_sensitive_json(value: &mut Value, parent: &str) {
    match value {
        Value::Object(object) => {
            let secret_object = parent == "secret" || object.get("kind").and_then(Value::as_str) == Some("Secret");
            for (key, child) in object.iter_mut() {
                let lower = normalize_json_key(key);
                if lower == "env" {
                    redact_environment_values(child);
                    continue;
                }
                let secret_reference = lower.ends_with("secretref") || lower.ends_with("credentialref");
                let secret_payload = secret_object && matches!(lower.as_str(), "data" | "stringdata");
                let sensitive = secret_payload || is_sensitive_json_key(&lower);
                if sensitive && !secret_reference {
                    *child = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_sensitive_json(child, if secret_object { "secret" } else { &lower });
                }
            }
        },
        Value::Array(items) => {
            for item in items {
                redact_sensitive_json(item, parent);
            }
        },
        Value::String(text) => *text = redact_inline_credentials(&redact_urls(text)),
        _ => {},
    }
}

fn redact_environment_values(value: &mut Value) {
    let Some(entries) = value.as_array_mut() else {
        redact_sensitive_json(value, "env");
        return;
    };
    for entry in entries {
        let env_name = entry["name"].as_str().map(normalize_json_key).unwrap_or_default();
        let sensitive_name = is_sensitive_json_key(&env_name);
        if let Some(fields) = entry.as_object_mut() {
            for (key, child) in fields {
                if sensitive_name && normalize_json_key(key) == "value" {
                    *child = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_sensitive_json(child, &normalize_json_key(key));
                }
            }
        }
    }
}

fn normalize_json_key(value: &str) -> String {
    value
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_sensitive_json_key(key: &str) -> bool {
    matches!(
        key,
        "token"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "password"
            | "passwd"
            | "authorization"
            | "proxyauthorization"
            | "apikey"
            | "credential"
            | "credentials"
            | "credentialvalue"
            | "secret"
            | "secretvalue"
            | "clientsecret"
            | "privatekey"
            | "tlskey"
            | "endpoint"
            | "host"
            | "hostname"
            | "url"
            | "uri"
            | "address"
            | "sni"
            | "hostnames"
            | "urls"
            | "uris"
            | "addresses"
            | "snis"
    ) || key.ends_with("password")
        || key.ends_with("apikey")
        || key.ends_with("token")
        || key.ends_with("clientsecret")
        || key.ends_with("privatekey")
        || key.ends_with("endpoint")
        || key.ends_with("url")
        || key.ends_with("hostname")
        || key.ends_with("address")
        || key.ends_with("sni")
}

fn redact_urls(value: &str) -> String {
    let sanitized = match regex::Regex::new(r#"(?i)\b(?:https?|wss?)://[^\s"'<>]+"#) {
        Ok(regex) => regex.replace_all(value, "[REDACTED_URL]").into_owned(),
        Err(_) => value.to_owned(),
    };
    redact_ip_addresses(&sanitized)
}

fn redact_ip_addresses(value: &str) -> String {
    use std::net::{Ipv4Addr, Ipv6Addr};

    static IPV4_PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    static IPV6_PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let ipv4_pattern = IPV4_PATTERN
        .get_or_init(|| regex::Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b").expect("IPv4 expression is valid"));
    let ipv4_redacted = ipv4_pattern
        .replace_all(value, |capture: &regex::Captures<'_>| {
            if capture[0].parse::<Ipv4Addr>().is_ok() {
                "[REDACTED_IP]".to_owned()
            } else {
                capture[0].to_owned()
            }
        })
        .into_owned();
    let ipv6_pattern = IPV6_PATTERN.get_or_init(|| {
        regex::Regex::new(r"(?i)\[[0-9a-f:]+\]|[0-9a-f]*:[0-9a-f:]+").expect("IPv6 expression is valid")
    });
    ipv6_pattern
        .replace_all(&ipv4_redacted, |capture: &regex::Captures<'_>| {
            let token = &capture[0];
            let address = token.trim_start_matches('[').trim_end_matches(']');
            if address.parse::<Ipv6Addr>().is_ok() {
                "[REDACTED_IP]".to_owned()
            } else {
                token.to_owned()
            }
        })
        .into_owned()
}

fn status_from_checks(report: &Report) -> String {
    status_from_states(
        report.qualification_scope,
        report.checks.iter().any(|check| check.status == "fail") || report.probes.iter().any(|probe| !probe.passed),
        report.checks.iter().any(|check| check.status == "blocked"),
        report.passed(),
    )
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "these three report facts are a compact, explicit status decision table"
)]
fn status_from_states(scope: QualificationScope, has_failure: bool, has_blocked: bool, passed: bool) -> String {
    if has_failure {
        "fail".to_owned()
    } else if has_blocked {
        "blocked".to_owned()
    } else if passed && scope == QualificationScope::RoutingSmoke {
        "partial".to_owned()
    } else if passed {
        "pass".to_owned()
    } else {
        "not-run".to_owned()
    }
}

fn write_summary(evidence_dir: &Path, report: &Report) -> Result<(), String> {
    let mut text = format!(
        "# Grid + llm-d + ExtProc E2E summary\n\n- Status: `{}`\n- Qualification: `{}`\n- Resource profile: `{}`\n- Result label: `{}`\n- Mode: `{}`\n- SOURCE_MODE: `{}`\n- Source resolution: `{}`\n- Platform manifests: `{}`\n- Scope: `{}`\n- Enrollment qualification: `{}`\n- Capacity baseline: `{}`\n- Under-baseline dev smoke: `{}`\n- API TLS mode: `{}`\n- Context: `{}`\n- Namespace: `{}`\n- Gateway connect mapping configured: `{}`\n- Started (Unix seconds): `{}`\n- Auth configured: `{}`\n",
        report.status,
        report.qualification,
        report.resource_profile.as_str(),
        report.result_label,
        report.mode.as_str(),
        report.source_mode.as_str(),
        report.source_resolution_status,
        report.platform_manifests,
        report.qualification_scope.as_str(),
        report.enrollment_qualification,
        report.capacity_baseline,
        report.under_baseline_dev_smoke,
        report.api_tls_mode,
        report.context,
        report.namespace,
        report.gateway_connect_to_configured,
        report.started_epoch_seconds,
        report.auth_configured,
    );
    if !report.source_revisions.is_empty() {
        text.push_str("\n## Resolved source revisions\n\n");
        for revision in &report.source_revisions {
            writeln!(text, "- `{revision}`").expect("formatting into a String cannot fail");
        }
    }
    text.push_str("\n## Checks\n\n");
    for check in &report.checks {
        writeln!(text, "- `{}` — `{}`: {}", check.name, check.status, check.detail)
            .expect("formatting into a String cannot fail");
    }
    text.push_str("\nNo credential values or private endpoint hostnames are written to this summary.\n");
    let text = sanitize_capture_text(&text);
    write_private(&evidence_dir.join("SUMMARY.md"), text.as_bytes())
}

fn write_bug_capture(evidence_dir: &Path, report: &Report) -> Result<(), String> {
    let first_failure = report
        .checks
        .iter()
        .find(|check| matches!(check.status.as_str(), "fail" | "blocked"))
        .map(|check| {
            (
                check.name.as_str(),
                check.status.as_str(),
                check.detail.as_str(),
                check.observed_epoch_seconds,
            )
        });
    let failing_probe = report.probes.iter().find(|probe| !probe.passed);
    let phase = first_failure
        .map(|entry| entry.0)
        .or_else(|| failing_probe.map(|probe| probe.phase.as_str()))
        .unwrap_or("not-run");
    let observed_at = first_failure
        .map(|entry| entry.3)
        .or_else(|| failing_probe.map(|probe| probe.observed_epoch_seconds));
    let classification = classify_failure(phase);
    let cause = first_failure
        .map(|entry| format!("`{}` — {} ({})", entry.0, entry.2, entry.1))
        .or_else(|| {
            failing_probe.map(|probe| format!("probe `{}` failed its status/attribution/oracle assertion", probe.phase))
        })
        .unwrap_or_else(|| "qualification did not execute all required phases".to_owned());
    let probe_files = report
        .probes
        .iter()
        .map(|probe| {
            format!(
                "- `{}` request ID `{}` observed at epoch `{}`; response files `{}` and `{}`",
                probe.phase,
                probe.request_id,
                probe.observed_epoch_seconds,
                Path::new(&probe.headers_file)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("headers"),
                Path::new(&probe.body_file)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("body"),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!(
        "# BUG-CAPTURE — non-pass Grid + llm-d + ExtProc E2E run\n\n\
         - Status: `{}`\n- Qualification: `{}`\n- Resource profile: `{}`\n- Result label: `{}`\n- First failing phase: `{phase}`\n\
         - First failure observed at Unix epoch seconds: `{}`\n- Classification: `{classification}`\n\
         - Context: `{}`\n- Namespace: `{}`\n- Source revisions: `{}`\n\
         - Immutable image inputs: `{}`\n- Runtime image records: `{}`\n- First failure: {cause}\n\n\
         ## Retained evidence\n\n\
         - `report.json` and `SUMMARY.md`\n- `resource-*.log`, `deployment-*.log`, `snapshot-*.log`, and `logs-*.log`\n\
         - `grid-revision-*.json`, `epp-metrics-*.json`, and `simulator-control-*.json` when collected\n{probe_files}\n\n\
         ## Reproduction\n\n\
         Re-run `cargo run --locked -p xtask -- e2e grid-llmd --mode run --require-signal-transition` with the same sanitized environment and\
         parameter values documented in `tests/grid-llmd-e2e/README.md`. The runner does not store the authentication value, API key, Secret data,\
         private key, or authorization header.\n\n\
         ## Triage\n\n\
         - Likely owner: `{}`\n- Reproducibility: not established from a single non-pass run; retain this evidence and repeat after the identified gate is addressed.\n\n\
         Review the sanitized resource snapshots, GridNetwork status and referenced overlay ConfigMap, EPP samples, simulator controls,\
         Gateway/HTTPRoute/filter configuration, ExtProc logs, and sanitized request/response files above before opening a follow-up issue.\n",
        report.status,
        report.qualification,
        report.resource_profile.as_str(),
        report.result_label,
        observed_at.map_or_else(|| "not recorded".to_owned(), |timestamp| timestamp.to_string()),
        report.context,
        report.namespace,
        report.source_revisions.join(", "),
        report.image_pins.join(", "),
        report.runtime_images.len(),
        classification,
    );
    let text = sanitize_capture_text(&text);
    write_private(&evidence_dir.join("BUG-CAPTURE.md"), text.as_bytes())
}

fn classify_failure(phase: &str) -> &'static str {
    if phase.starts_with("secure-cluster") || phase.starts_with("kubectl") {
        "environment/access or test setup"
    } else if phase.starts_with("maas") {
        "MaaS or platform integration"
    } else if phase.starts_with("extproc") {
        "Praxis ExtProc or filter attachment"
    } else if phase.starts_with("grid") {
        "Grid operator, overlay, or Gateway integration"
    } else if phase.starts_with("enrollment") {
        "Grid Enrollment or site identity"
    } else if phase.starts_with("epp") || phase.starts_with("pool") {
        "llm-d EPP or serving configuration"
    } else if phase.starts_with("simulator") {
        "llm-d-sim control or E2E hook"
    } else if phase.starts_with("provenance") {
        "source or image provenance input"
    } else {
        "cross-component integration; owner requires evidence review"
    }
}

fn header_value(headers: &str, wanted: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(wanted)
            .then(|| value.trim().to_owned())
    })
}

fn curl_config_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn sanitize_gateway_url(value: &str) -> String {
    let Some((scheme, remainder)) = value.split_once("://") else {
        return "<redacted-endpoint>".to_owned();
    };
    let endpoint_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
    let suffix = remainder.get(endpoint_end..).unwrap_or_default();
    let path = suffix.split(['?', '#']).next().unwrap_or_default();
    format!("{scheme}://<redacted>{path}")
}

fn sanitize_context(value: &str) -> String {
    if value.is_empty() || value == "<required>" {
        value.to_owned()
    } else if value.contains("://") {
        "<context-url-redacted>".to_owned()
    } else {
        "<context-redacted>".to_owned()
    }
}

fn redact_text(value: &str, token: Option<&str>, private_url: Option<&str>) -> String {
    let mut sanitized = value.to_owned();
    if let Some(token) = token.filter(|token| !token.is_empty()) {
        sanitized = sanitized.replace(token, "[REDACTED]");
    }
    if let Some(url) = private_url.filter(|url| !url.is_empty()) {
        sanitized = sanitized.replace(url, &sanitize_gateway_url(url));
    }
    let sanitized = sanitized
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("set-cookie:") {
                line.split_once(':').map_or_else(
                    || "[REDACTED HEADER]".to_owned(),
                    |(name, _)| format!("{name}: [REDACTED]"),
                )
            } else if lower.starts_with("location:") {
                line.split_once(':').map_or_else(
                    || "location: [REDACTED]".to_owned(),
                    |(name, value)| format!("{name}: {}", sanitize_gateway_url(value.trim())),
                )
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    redact_inline_credentials(&redact_urls(&sanitized))
}

fn redact_inline_credentials(value: &str) -> String {
    static CREDENTIAL_PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = CREDENTIAL_PATTERN.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)\b(authorization|proxy-authorization|x-api-key|api[-_]?key|access[-_]?token|refresh[-_]?token|id[-_]?token|client[-_]?secret|password|passwd|private[-_]?key|secret(?:[-_]?value)?|credential(?:[-_]?value)?)\b(\s*[:=]\s*)(?:\"[^\"]*\"|'[^']*'|[^\s,;}]+)"#,
        )
        .expect("credential redaction expression is valid")
    });
    pattern.replace_all(value, "$1$2[REDACTED]").into_owned()
}

fn redact_probe_text(value: &str, token: &str, private_url: Option<&str>, auth_header: &str, prompt: &str) -> String {
    let redacted = redact_text(value, Some(token), private_url).replace(prompt, "[SYNTHETIC_TEST_PROMPT_OMITTED]");
    redacted
        .lines()
        .map(|line| {
            line.split_once(':').map_or_else(
                || line.to_owned(),
                |(name, _)| {
                    if name.trim().eq_ignore_ascii_case(auth_header) {
                        format!("{}: [REDACTED]", name.trim())
                    } else {
                        line.to_owned()
                    }
                },
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_response_headers(
    headers: &str,
    auth_header: &str,
    token: &str,
    private_url: Option<&str>,
    prompt: &str,
) -> String {
    redact_probe_text(headers, token, private_url, auth_header, prompt)
}

fn valid_header_name(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Parse an ExtProc marker as a header name or an exact `Header=ExpectedValue` assertion.
fn parse_extproc_header_expectation(value: &str) -> Option<(&str, Option<&str>)> {
    let (name, expected) = value
        .split_once('=')
        .map_or((value, None), |(name, expected)| (name, Some(expected)));
    if !valid_header_name(name)
        || expected.is_some_and(|expected| {
            expected.is_empty() || expected.trim() != expected || expected.bytes().any(|byte| byte.is_ascii_control())
        })
    {
        return None;
    }
    Some((name, expected))
}

fn extproc_header_expectation_matches(headers: &str, expectation: &str) -> bool {
    parse_extproc_header_expectation(expectation).is_some_and(|(name, expected)| {
        header_value(headers, name)
            .is_some_and(|observed| expected.map_or_else(|| !observed.is_empty(), |expected| observed == expected))
    })
}

fn request_model_header(name: &str, value: &str) -> Result<String, String> {
    if !valid_header_name(name) {
        return Err("model header name is invalid".to_owned());
    }
    if value.is_empty() || value.trim() != value || value.contains(['\r', '\n']) {
        return Err("model header value is empty or contains invalid whitespace".to_owned());
    }
    if value.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("model header value is invalid".to_owned());
    }
    Ok(format!("{name}: {value}"))
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn simulator_receipt_requires_the_unique_request_id_on_a_completion_log() {
        let request_id = "grid-llmd-1234-5678-baseline";
        assert!(simulator_completion_log_matches(
            "I1028 16:00:00 simulator.go:401] Finished processing request id=grid-llmd-1234-5678-baseline",
            request_id
        ));
        assert!(!simulator_completion_log_matches(
            "I1028 16:00:00 simulator.go:401] Finished processing request id=another-request",
            request_id
        ));
        assert!(!simulator_completion_log_matches(
            "I1028 16:00:00 http.go:394] Received new HTTP req=grid-llmd-1234-5678-baseline",
            request_id
        ));
    }

    #[test]
    fn baseline_pool_is_derived_from_the_lowest_overlay_rank() {
        assert_eq!(
            selected_pool_from_ranks(["pool-a", "pool-b"], [Some(2), Some(1)]).as_deref(),
            Ok("pool-b")
        );
        assert_eq!(
            selected_pool_from_ranks(["pool-a", "pool-b"], [Some(1), Some(2)]).as_deref(),
            Ok("pool-a")
        );
        assert!(selected_pool_from_ranks(["pool-a", "pool-b"], [Some(1), Some(1)]).is_err());
    }

    #[test]
    fn downstream_grid_operator_source_is_required_for_this_qualification() {
        assert!(REQUIRED_SOURCE_COMPONENTS.contains(&"praxis-ai-grid-operator"));
        assert!(REQUIRED_IMAGE_COMPONENTS.contains(&"praxis-ai-grid-operator"));
        assert!(!REQUIRED_IMAGE_COMPONENTS.contains(&"grid-operator"));
    }

    #[test]
    fn native_routing_smoke_does_not_require_external_model_or_enrollment_images() {
        assert!(ROUTING_SMOKE_REQUIRED_SOURCE_COMPONENTS.contains(&"grid"));
        assert!(ROUTING_SMOKE_REQUIRED_SOURCE_COMPONENTS.contains(&"maas"));
        assert!(ROUTING_SMOKE_REQUIRED_SOURCE_COMPONENTS.contains(&"praxis-ai"));
        assert!(ROUTING_SMOKE_REQUIRED_IMAGE_COMPONENTS.contains(&"maas-controller"));
        assert!(!ROUTING_SMOKE_REQUIRED_IMAGE_COMPONENTS.contains(&"grid-enrollment"));
        assert!(ROUTING_SMOKE_REQUIRED_IMAGE_COMPONENTS.contains(&"praxis-ai"));
    }

    #[test]
    fn gateway_revision_hook_budget_is_not_limited_to_the_poll_interval() {
        assert_eq!(gateway_revision_hook_timeout_seconds(240), 60);
        assert_eq!(gateway_revision_hook_timeout_seconds(15), 15);
        assert_eq!(gateway_revision_hook_timeout_seconds(0), 1);
    }

    #[test]
    fn gateway_request_attribution_requires_one_upstream_attempt_and_no_retry() {
        let before = GatewayRevisionRecord {
            phase: "baseline".to_owned(),
            revision: "a".repeat(64),
            pod_uid: "praxis-pod".to_owned(),
            proxy_pod_uid: "envoy-pod".to_owned(),
            api_ca_verified: false,
            route_retry_limit: 2,
            route_backend_count: 1,
            route_cluster_sha256: "a".repeat(64),
            route_backend_service_matches: true,
            serving_config_digest: "b".repeat(64),
            mounted_serving_config_matches: true,
            overlay_candidate_identities_match: true,
            overlay_candidate_count: 2,
            retry_attempts: None,
            retry_counter_present: false,
            upstream_requests: 10,
            request_counter_present: true,
            counter_source: "envoy_clusters_per_host_rq_total".to_owned(),
            observed_epoch_seconds: 1,
        };
        let mut after = before.clone();
        after.phase = "baseline-after-request".to_owned();
        after.upstream_requests = 11;
        assert_eq!(gateway_retry_attribution(&before, &after), (true, Some(1), None));

        after.upstream_requests = 12;
        after.retry_attempts = Some(1);
        after.retry_counter_present = true;
        let mut retry_observed_before = before.clone();
        retry_observed_before.retry_attempts = Some(0);
        retry_observed_before.retry_counter_present = true;
        assert_eq!(
            gateway_retry_attribution(&retry_observed_before, &after),
            (false, Some(2), Some(1))
        );

        after = before.clone();
        after.proxy_pod_uid = "restarted-envoy-pod".to_owned();
        after.upstream_requests = 11;
        assert_eq!(gateway_retry_attribution(&before, &after), (false, Some(1), None));

        after = before.clone();
        after.upstream_requests = 12;
        assert_eq!(gateway_retry_attribution(&before, &after), (false, Some(2), None));
    }

    #[test]
    fn grid_model_header_is_sent_and_rejects_header_injection() {
        assert_eq!(
            request_model_header("x-model", "grid-test-model"),
            Ok("x-model: grid-test-model".to_owned())
        );
        assert!(request_model_header("x-model\r\nX-Evil", "grid-test-model").is_err());
        assert!(request_model_header("x-model", "grid-test-model\r\nX-Evil: yes").is_err());
        assert!(request_model_header("x-model", "").is_err());
    }

    #[test]
    fn grid_site_and_backend_attribution_can_have_distinct_values() {
        let expected = configured_pool_attribution(Some("grid-llmd-site"), Some("run-pool-a"), None)
            .expect("provider and backend identities are both configured");
        assert_eq!(expected, ("grid-llmd-site", "run-pool-a"));
        assert!(response_attribution_matches(
            Some("grid-llmd-site"),
            Some("run-pool-a"),
            expected
        ));
        assert!(!response_attribution_matches(
            Some("grid-llmd-site"),
            Some("run-pool-b"),
            expected
        ));
    }

    #[test]
    fn legacy_pool_attribution_fills_both_response_headers() {
        assert_eq!(
            configured_pool_attribution(None, None, Some("pool-a")),
            Some(("pool-a", "pool-a"))
        );
        assert_eq!(configured_pool_attribution(Some("site-a"), None, None), None);
    }

    #[test]
    fn unauthenticated_grid_route_sends_no_client_credential() {
        assert_eq!(request_auth_header_value(None, true, "authorization"), Ok(None));
        assert!(request_auth_header_value(Some("short-lived-token"), true, "authorization").is_err());
        assert!(request_auth_header_value(None, false, "authorization").is_err());
        assert_eq!(
            request_auth_header_value(Some("opaque-key"), false, "x-api-key"),
            Ok(Some("opaque-key".to_owned()))
        );
        assert_eq!(
            request_auth_header_value(Some("opaque-token"), false, "authorization"),
            Ok(Some("Bearer opaque-token".to_owned()))
        );
    }

    #[test]
    fn queue_depth_score_uses_epp_capacity_normalization_and_grid_weight() {
        assert_eq!(expected_queue_depth_score(0.0, 4.0), Some(1.0));
        assert_eq!(expected_queue_depth_score(2.0, 4.0), Some(0.5));
        assert_eq!(expected_queue_depth_score(4.0, 4.0), Some(0.0));
        assert_eq!(expected_queue_depth_score(8.0, 4.0), Some(0.0));
        assert_eq!(expected_queue_depth_score(-1.0, 4.0), None);
        assert_eq!(expected_queue_depth_score(1.0, 0.0), None);
        assert_eq!(expected_queue_depth_score(f64::NAN, 4.0), None);
    }

    #[test]
    fn overlay_capture_keeps_queue_scores_and_exact_provider_identity() {
        let envelope = serde_json::json!({
            "overlay": {"candidates": [
                {"kind":"inference_model","name":"grid-test-model","site":"site-a","cluster":"run-pool-a","score":1.0,"rank":0,"score_breakdown":{"queue_depth":1.0}},
                {"kind":"inference_model","name":"grid-test-model","site":"site-a","cluster":"run-pool-b","score":0.0,"rank":1,"score_breakdown":{"queue_depth":0.0}}
            ]}
        });
        let (scores, ranks, queue_scores) = overlay_candidate_state(&envelope);
        assert_eq!(
            provider_candidate_value(&scores, "grid-test-model", "run-pool-a"),
            Some(1.0)
        );
        assert_eq!(
            provider_candidate_rank(&ranks, "grid-test-model", "run-pool-b"),
            Some(1)
        );
        assert_eq!(
            provider_candidate_value(&queue_scores, "grid-test-model", "run-pool-a"),
            Some(1.0)
        );
        assert_eq!(overlay_candidate_counts(&envelope), (2, 2));
        assert_eq!(pool_resource_name("test-ns/inferencepool/pool-a#ready"), "pool-a");
    }

    #[test]
    fn overlay_candidate_count_rejects_duplicate_candidate_identities() {
        let duplicate = serde_json::json!({
            "overlay": {"candidates": [
                {"kind":"inference_model","name":"grid-test-model","site":"site-a","cluster":"run-pool-a"},
                {"kind":"inference_model","name":"grid-test-model","site":"site-a","cluster":"run-pool-a"},
                {"kind":"inference_model","name":"grid-test-model","site":"site-a","cluster":"run-pool-b"}
            ]}
        });
        assert_eq!(overlay_candidate_counts(&duplicate), (3, 2));
    }

    #[test]
    fn distributed_grid_revision_requires_current_network_generation() {
        let record = GridRevisionRecord {
            phase: "baseline".to_owned(),
            observed_epoch_seconds: 1,
            status_field: "routingMapStatus".to_owned(),
            gateway: "gateway".to_owned(),
            overlay_phase: "Distributed".to_owned(),
            network_generation: Some(4),
            observed_generation: Some(4),
            config_map_namespace: "task".to_owned(),
            config_map_name: "overlay".to_owned(),
            rendered_revision: "revision-1".to_owned(),
            distributed_revision: "revision-1".to_owned(),
            content_digest: "digest".to_owned(),
            config_consistent: true,
            candidate_count: 0,
            candidate_identity_count: 0,
            candidate_scores: BTreeMap::new(),
            candidate_ranks: BTreeMap::new(),
            candidate_queue_scores: BTreeMap::new(),
        };
        assert!(grid_revision_is_distributed(&record));

        let mut stale = record;
        stale.observed_generation = Some(3);
        assert!(!grid_revision_is_distributed(&stale));
    }

    #[test]
    fn full_scope_keeps_enrollment_as_a_required_gate() {
        assert!(QualificationScope::Full.required_gates().contains(&"enrollment"));
        assert_eq!(QualificationScope::Full.enrollment_qualification(), "required");
        assert_eq!(status_from_states(QualificationScope::Full, false, false, true), "pass");
    }

    #[test]
    fn routing_smoke_marks_enrollment_unqualified_and_overall_partial() {
        assert!(
            !QualificationScope::RoutingSmoke
                .required_gates()
                .contains(&"enrollment")
        );
        assert_eq!(
            QualificationScope::RoutingSmoke.enrollment_qualification(),
            "skipped-unqualified-etcd-encryption-prerequisite-unverified"
        );
        assert_eq!(
            status_from_states(QualificationScope::RoutingSmoke, false, false, true),
            "partial"
        );
        assert_eq!(
            status_from_states(QualificationScope::RoutingSmoke, false, true, false),
            "blocked"
        );
    }

    #[test]
    fn insecure_api_mode_requires_explicit_ephemeral_dev_opt_in() {
        assert!(api_tls_mode_from_parts(None, false, false, Some("ephemeral-dev")).is_err());
        assert!(api_tls_mode_from_parts(None, false, true, None).is_err());
        assert!(api_tls_mode_from_parts(None, false, true, Some("shared-dev")).is_err());
        assert_eq!(
            api_tls_mode_from_parts(None, false, true, Some("ephemeral-dev")),
            Ok(ApiTlsMode::ExplicitEphemeralDevInsecure)
        );
        let ca = tempfile::NamedTempFile::new().expect("temporary CA file");
        assert_eq!(
            api_tls_mode_from_parts(Some(ca.path()), false, true, Some("ephemeral-dev")),
            Ok(ApiTlsMode::CaVerified)
        );
        assert_eq!(
            api_tls_mode_from_parts(None, true, false, None),
            Ok(ApiTlsMode::CaVerified)
        );
    }

    #[test]
    fn gateway_connect_to_keeps_the_requested_tls_hostname() {
        assert!(gateway_connect_to_matches(
            "https://grid-llmd-site.grid.internal",
            "grid-llmd-site.grid.internal:443:gateway.example.com:443"
        ));
        assert!(gateway_connect_to_matches(
            "https://grid-llmd-site.grid.internal:443/v1/chat/completions",
            "grid-llmd-site.grid.internal:443:gateway.example.com:443"
        ));
        assert!(!gateway_connect_to_matches(
            "https://grid-llmd-site.grid.internal",
            "other.grid.internal:443:gateway.example.com:443"
        ));
        assert!(!gateway_connect_to_matches(
            "http://grid-llmd-site.grid.internal",
            "grid-llmd-site.grid.internal:80:gateway.example.com:443"
        ));
        assert!(!gateway_connect_to_matches(
            "https://grid-llmd-site.grid.internal",
            "grid-llmd-site.grid.internal:443:gateway.example.com:0"
        ));
        assert!(!gateway_connect_to_matches(
            "https://grid-llmd-site.grid.internal",
            "grid-llmd-site.grid.internal:443:gateway.example.com:443:extra"
        ));
    }

    #[test]
    fn kubeconfig_tls_verification_rejects_only_explicit_insecure_mode() {
        let ca_bearing = serde_json::json!({
            "clusters":[{"cluster":{"certificate-authority-data":"REDACTED"}}]
        });
        let system_ca = serde_json::json!({"clusters":[{"cluster":{}}]});
        let insecure = serde_json::json!({
            "clusters":[{"cluster":{"insecure-skip-tls-verify":true}}]
        });
        assert!(kubeconfig_document_uses_tls_verification(&ca_bearing));
        assert!(kubeconfig_document_uses_tls_verification(&system_ca));
        assert!(!kubeconfig_document_uses_tls_verification(&insecure));
        assert!(!kubeconfig_document_uses_tls_verification(
            &serde_json::json!({"clusters":[]})
        ));
    }

    #[test]
    fn strict_resource_profile_is_default_and_reduced_dev_requires_ephemeral_environment() {
        assert_eq!(ResourceProfile::default(), ResourceProfile::Strict);
        assert_eq!(ResourceProfile::Strict.as_str(), "strict");
        assert_eq!(ResourceProfile::ReducedDev.as_str(), "reduced-dev");
        assert!(resource_profile_authorized(ResourceProfile::Strict, None));
        assert!(!resource_profile_authorized(ResourceProfile::ReducedDev, None));
        assert!(!resource_profile_authorized(
            ResourceProfile::ReducedDev,
            Some("shared-dev")
        ));
        assert!(resource_profile_authorized(
            ResourceProfile::ReducedDev,
            Some("ephemeral-dev")
        ));
    }

    #[test]
    fn full_capacity_gate_requires_two_large_workers_and_twenty_aggregate_cpu() {
        assert!(capacity_preflight_allowed(false, 2, 2, 20_000, 0, 0, true));
        assert!(capacity_preflight_allowed(false, 3, 2, 24_000, 0, 0, true));
        assert!(!capacity_preflight_allowed(false, 2, 2, 19_999, 0, 0, true));
        assert!(!capacity_preflight_allowed(false, 2, 1, 24_000, 0, 0, true));
        assert!(!capacity_preflight_allowed(false, 1, 2, 24_000, 0, 0, true));
    }

    #[test]
    fn under_baseline_capacity_waiver_keeps_worker_readiness_and_pending_gates() {
        assert!(!capacity_preflight_allowed(false, 3, 0, 10_500, 0, 0, true));
        assert!(capacity_preflight_allowed(true, 2, 0, 10_500, 0, 0, true));
        assert!(!capacity_preflight_allowed(true, 1, 0, 10_500, 0, 0, true));
        assert!(!capacity_preflight_allowed(true, 2, 0, 10_500, 0, 1, true));
        assert!(!capacity_preflight_allowed(true, 2, 0, 10_500, 0, 0, false));
        assert!(!capacity_preflight_allowed(true, 2, 0, 10_500, 1, 0, true));
    }

    #[test]
    fn openshift_prerequisites_require_healthy_operators_and_default_dynamic_storage() {
        let cluster_version = serde_json::json!({
            "status":{"conditions":[
                {"type":"Available","status":"True"},
                {"type":"Progressing","status":"False"},
                {"type":"Failing","status":"False"}
            ]}
        });
        assert!(resource_conditions_match(
            &cluster_version,
            &[("Available", "True"), ("Progressing", "False"), ("Failing", "False")]
        ));

        let healthy_operator = serde_json::json!({
            "status":{"conditions":[
                {"type":"Available","status":"True"},
                {"type":"Progressing","status":"False"},
                {"type":"Degraded","status":"False"}
            ]}
        });
        let degraded_operator = serde_json::json!({
            "status":{"conditions":[
                {"type":"Available","status":"True"},
                {"type":"Progressing","status":"False"},
                {"type":"Degraded","status":"True"}
            ]}
        });
        assert!(cluster_operators_are_healthy(
            &serde_json::json!({"items":[healthy_operator]})
        ));
        assert!(!cluster_operators_are_healthy(
            &serde_json::json!({"items":[degraded_operator]})
        ));
        assert!(!cluster_operators_are_healthy(&serde_json::json!({"items":[]})));

        let dynamic_default = serde_json::json!({
            "metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}},
            "provisioner":"ebs.csi.aws.com"
        });
        let static_default = serde_json::json!({
            "metadata":{"annotations":{"storageclass.kubernetes.io/is-default-class":"true"}},
            "provisioner":"kubernetes.io/no-provisioner"
        });
        assert!(is_default_dynamic_storage_class(&dynamic_default));
        assert!(!is_default_dynamic_storage_class(&static_default));
    }

    #[test]
    fn parses_node_capacity_quantities_without_lowering_the_baseline() {
        assert_eq!(parse_cpu_millis("3500m"), Some(3500));
        assert_eq!(parse_cpu_millis("8"), Some(8000));
        assert_eq!(parse_cpu_millis("1.25"), Some(1250));
        assert_eq!(parse_cpu_millis(".5"), Some(500));
        assert_eq!(parse_cpu_millis("0.0005"), None);
        assert_eq!(parse_cpu_millis("bad"), None);
        assert_eq!(parse_memory_bytes("14859428Ki"), Some(14_859_428 * 1024));
        assert_eq!(parse_memory_bytes("32Gi"), Some(32 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_bytes("unknown"), None);
    }

    #[test]
    fn hook_api_ca_claim_is_required_unless_the_reported_mode_is_dev_unverified() {
        assert!(!api_access_acceptable_for_mode(Ok(ApiTlsMode::CaVerified), false));
        assert!(api_access_acceptable_for_mode(Ok(ApiTlsMode::CaVerified), true));
        assert!(api_access_acceptable_for_mode(
            Ok(ApiTlsMode::ExplicitEphemeralDevInsecure),
            false
        ));
        assert!(!api_access_acceptable_for_mode(Err("not opted in".to_owned()), false));
    }

    #[test]
    fn downstream_grid_operator_deployment_requires_the_exact_pinned_image() {
        let expected = "quay.io/opendatahub/odh-praxis-ai-grid-operator@sha256:abcd";
        let downstream = serde_json::json!({
            "spec": {"template": {"spec": {"containers": [{"image": expected}]}}}
        });
        let upstream = serde_json::json!({
            "spec": {"template": {"spec": {"containers": [{
                "image": "ghcr.io/praxis-proxy/grid-operator@sha256:abcd"
            }]}}}
        });
        let duplicate = serde_json::json!({
            "spec": {"template": {"spec": {"containers": [
                {"image": expected}, {"image": expected}
            ]}}}
        });

        assert!(deployment_has_exact_image(&downstream, expected));
        assert!(!deployment_has_exact_image(&upstream, expected));
        assert!(!deployment_has_exact_image(&duplicate, expected));
    }

    #[test]
    fn parses_epp_metric_and_pool_label() {
        let metrics = "# HELP llm_d_epp_average_queue_size queue\nllm_d_epp_average_queue_size{name=\"pool-a\",grid_site=\"east\"} 9\n";
        assert_eq!(
            parse_prometheus_metric(metrics, "llm_d_epp_average_queue_size", "name"),
            Ok(vec![(Some("pool-a".to_owned()), 9.0)])
        );
    }

    #[test]
    fn rejects_missing_or_non_finite_epp_samples() {
        assert!(parse_prometheus_metric("other_metric 1", "wanted", "name").is_err());
        assert!(parse_prometheus_metric("wanted{pool=\"a\"} NaN", "wanted", "pool").is_err());
    }

    #[test]
    fn parses_namespaced_pool_and_simulator_refs() {
        let pool = parse_resource_ref("namespace-a/inferencepool/pool-a", "default").expect("pool ref");
        assert_eq!(pool.namespace, "namespace-a");
        assert_eq!(pool.kind, "inferencepool");
        assert_eq!(pool.name, "pool-a");

        let control = parse_simulator_control_ref("namespace-a/configmap/sim-a#config.yaml@pool-a", "default")
            .expect("simulator ref");
        assert_eq!(control.namespace, "namespace-a");
        assert_eq!(control.name, "sim-a");
        assert_eq!(control.data_key, "config.yaml");
        assert_eq!(control.pool.as_deref(), Some("pool-a"));
    }

    #[test]
    fn rejects_stale_deployment_and_resource_conditions() {
        let deployment = r#"{"metadata":{"generation":4},"spec":{"replicas":1},"status":{"observedGeneration":3,"availableReplicas":1,"readyReplicas":1}}"#;
        assert!(!deployment_readiness(deployment).0);

        let partially_ready = r#"{"metadata":{"generation":4},"spec":{"replicas":2},"status":{"observedGeneration":4,"availableReplicas":2,"readyReplicas":1}}"#;
        assert!(!deployment_readiness(partially_ready).0);

        let ready = r#"{"metadata":{"generation":4},"status":{"conditions":[{"type":"Ready","status":"True","observedGeneration":3}]}}"#;
        assert!(!resource_condition(ready, "Ready").0);
    }

    #[test]
    fn external_provider_sni_matches_endpoint_hostname_not_url_text() {
        assert!(tls_sni_matches(
            "https://grid.example.test:8443/api",
            "grid.example.test"
        ));
        assert!(tls_sni_matches("https://GRID.example.test/", "grid.example.test."));
        assert!(!tls_sni_matches("https://grid.example.test/", "other.example.test"));
        assert!(!tls_sni_matches("not-a-url", "not-a-url"));
    }

    #[test]
    fn gateway_route_defaults_to_match_all_and_rejects_stale_acceptance() {
        let default_match = serde_json::json!({
            "spec": {"rules": [{}]}
        });
        assert!(route_matches_request_path(&default_match, "/v1/chat/completions"));

        let stale_route = serde_json::json!({
            "metadata": {"generation": 5},
            "status": {"parents": [{
                "parentRef": {"name": "gateway", "namespace": "gateway-ns"},
                "conditions": [
                    {"type": "Accepted", "status": "True", "observedGeneration": 4},
                    {"type": "ResolvedRefs", "status": "True", "observedGeneration": 5}
                ]
            }]}
        });
        assert!(!http_route_accepted_for(&stale_route, "gateway", "gateway-ns"));
    }

    #[test]
    fn gateway_root_path_prefix_matches_absolute_request_paths() {
        let route = serde_json::json!({
            "metadata": {"namespace": "grid-smoke"},
            "spec": {"rules": [{
                "matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                "backendRefs": [{"name": "grid-gateway", "port": 8080}]
            }]}
        });
        let service_ref = ResourceRef {
            namespace: "grid-smoke".to_owned(),
            kind: "service".to_owned(),
            name: "grid-gateway".to_owned(),
        };

        assert!(route_matches_request_path(&route, "/v1/chat/completions"));
        assert!(route_targets_only_service(&route, "/v1/chat/completions", &service_ref));
        assert!(!route_matches_request_path(&route, "v1/chat/completions"));
    }

    #[test]
    fn inference_pool_status_uses_current_parent_conditions_when_published() {
        let current = serde_json::json!({
            "metadata": {"generation": 3},
            "status": {"parents": [{"conditions": [
                {"type": "Accepted", "status": "True", "observedGeneration": 3},
                {"type": "ResolvedRefs", "status": "True", "observedGeneration": 3}
            ]}]}
        });
        assert_eq!(current_pool_status_ready(&current), Some(true));

        let stale = serde_json::json!({
            "metadata": {"generation": 3},
            "status": {"parents": [{"conditions": [
                {"type": "Accepted", "status": "True", "observedGeneration": 2},
                {"type": "ResolvedRefs", "status": "True", "observedGeneration": 3}
            ]}]}
        });
        assert_eq!(current_pool_status_ready(&stale), Some(false));

        let unpublished = serde_json::json!({"metadata": {"generation": 3}, "status": {"parents": []}});
        assert_eq!(current_pool_status_ready(&unpublished), None);
    }

    #[test]
    fn grid_gateway_service_reference_requires_a_service_kind() {
        let qualified = parse_resource_ref("grid-llmd-e2e/service/grid-gateway", "default").expect("service ref");
        assert_eq!(qualified.namespace, "grid-llmd-e2e");
        assert!(is_resource_kind(&qualified.kind, &["service", "services", "svc"]));

        let ambiguous = parse_resource_ref("grid-llmd-e2e/grid-gateway", "default").expect("two-part ref");
        assert!(!is_resource_kind(&ambiguous.kind, &["service", "services", "svc"]));
    }

    #[test]
    fn direct_grid_route_requires_current_gateway_parent_and_expected_service() {
        let route = serde_json::json!({
            "metadata": {"namespace": "grid-smoke", "generation": 3},
            "spec": {
                "hostnames": ["grid.example.test"],
                "rules": [{
                    "matches": [{"path": {"type": "PathPrefix", "value": "/v1"}}],
                    "backendRefs": [{"name": "grid-gateway", "port": 8443}]
                }]
            },
            "status": {"parents": [{
                "parentRef": {
                    "name": "grid-front-door",
                    "namespace": "grid-smoke",
                    "kind": "Gateway",
                    "group": "gateway.networking.k8s.io"
                },
                "conditions": [
                    {"type": "Accepted", "status": "True", "observedGeneration": 3},
                    {"type": "ResolvedRefs", "status": "True", "observedGeneration": 3}
                ]
            }]}
        });
        let service_ref = ResourceRef {
            namespace: "grid-smoke".to_owned(),
            kind: "service".to_owned(),
            name: "grid-gateway".to_owned(),
        };
        let service = serde_json::json!({"spec": {"ports": [{"port": 8443}]}});

        assert!(http_route_accepted_for(&route, "grid-front-door", "grid-smoke"));
        assert!(route_matches_request_path(&route, "/v1/chat/completions"));
        assert!(!route_matches_request_path(&route, "/v2/chat/completions"));
        assert!(route_matches_request_host(
            &route,
            Some("https://grid.example.test/v1/chat/completions")
        ));
        assert!(!route_matches_request_host(
            &route,
            Some("https://other.example.test/v1/chat/completions")
        ));
        assert!(route_targets_only_service(&route, "/v1/chat/completions", &service_ref));
        assert!(route_backend_ports_exist(&route, "/v1/chat/completions", &service));
        assert!(!route_targets_only_service(
            &route,
            "/v1/chat/completions",
            &ResourceRef {
                name: "model-service".to_owned(),
                ..service_ref.clone()
            }
        ));
        assert!(!route_backend_ports_exist(
            &route,
            "/v1/chat/completions",
            &serde_json::json!({"spec": {"ports": [{"port": 9443}]}})
        ));

        let mut wrong_parent = route.clone();
        wrong_parent["status"]["parents"][0]["parentRef"]["kind"] = Value::String("Service".to_owned());
        assert!(!http_route_accepted_for(&wrong_parent, "grid-front-door", "grid-smoke"));
    }

    #[test]
    fn route_parent_default_namespace_is_route_namespace() {
        let cross_namespace_route = serde_json::json!({
            "metadata": {"namespace": "routes", "generation": 1},
            "status": {"parents": [{
                "parentRef": {"name": "gateway"},
                "conditions": [
                    {"type": "Accepted", "status": "True", "observedGeneration": 1},
                    {"type": "ResolvedRefs", "status": "True", "observedGeneration": 1}
                ]
            }]}
        });
        assert!(!http_route_accepted_for(
            &cross_namespace_route,
            "gateway",
            "other-namespace"
        ));
    }

    #[test]
    fn epp_metric_change_requires_an_exact_pool_series() {
        let before = BTreeMap::from([("pool-a-extra".to_owned(), 1.0)]);
        let after = BTreeMap::from([("pool-a-extra".to_owned(), 2.0)]);
        assert!(!metric_pool_changed(&before, &after, "pool-a"));

        let exact = BTreeMap::from([("pool-a".to_owned(), 1.0)]);
        let changed = BTreeMap::from([("pool-a".to_owned(), 2.0)]);
        assert!(metric_pool_changed(&exact, &changed, "pool-a"));
        assert!(metric_pool_matches(&exact, &exact, "pool-a"));
        assert!(!metric_pool_matches(&before, &before, "pool-a"));
    }

    #[test]
    fn candidate_pool_matching_does_not_accept_name_substrings() {
        assert!(candidate_matches_pool("namespace/provider-a", "provider-a"));
        assert!(candidate_matches_pool("provider:provider-a", "provider-a"));
        assert!(!candidate_matches_pool("namespace/provider-a-extra", "provider-a"));
        assert!(!candidate_matches_pool("namespace/other-provider", "provider"));
    }

    #[test]
    fn accepts_current_deployment_and_resource_conditions() {
        let deployment = r#"{"metadata":{"generation":4},"spec":{"replicas":2},"status":{"observedGeneration":4,"availableReplicas":2,"readyReplicas":2}}"#;
        assert!(deployment_readiness(deployment).0);

        let ready = r#"{"metadata":{"generation":4},"status":{"conditions":[{"type":"Ready","status":"True","observedGeneration":4}]}}"#;
        assert!(resource_condition(ready, "Ready").0);
    }

    #[test]
    fn parses_image_pin_and_runtime_digest() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let (pins, errors) = parse_image_pins(&[format!("grid-gateway=ghcr.io/praxis/grid-gateway@{digest}")]);
        assert!(errors.is_empty());
        assert!(pins.contains_key("grid-gateway"));
        assert_eq!(
            runtime_image_digest(&format!("docker-pullable://ghcr.io/praxis/grid-gateway@{digest}")),
            Some(digest)
        );
        assert!(parse_image_pins(&["simulator:latest".to_owned()]).1.len() == 1);
    }

    #[test]
    fn computes_sha256_for_simulator_config() {
        assert_eq!(
            sha256_digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn body_oracle_matches_the_exact_serialized_request_digest_header() {
        let request = br#"{"model":"grid-model","messages":[{"role":"user","content":"smoke"}]}"#;
        let digest = sha256_digest(request);
        let observed = digest.strip_prefix("sha256:").expect("SHA-256 prefix");
        let headers = format!("HTTP/2 200\r\nx-qualification-request-sha256: {observed}\r\n");

        assert!(body_oracle_header_matches(
            &headers,
            "x-qualification-request-sha256",
            &digest
        ));
        assert!(body_oracle_header_matches(
            &headers,
            "X-QUALIFICATION-REQUEST-SHA256",
            &digest
        ));
        assert!(!body_oracle_header_matches(
            &headers,
            "x-qualification-request-sha256",
            &sha256_digest(b"different request")
        ));
        assert!(!body_oracle_header_matches(
            "HTTP/2 200\r\nx-qualification-request-sha256: wrong\r\n",
            "x-qualification-request-sha256",
            &digest
        ));
    }

    #[test]
    fn extproc_response_marker_can_require_the_exact_run_value() {
        let headers = "HTTP/2 200\r\nX-Praxis-ExtProc-Response: run-4iz0fvk\r\n";
        assert!(extproc_header_expectation_matches(
            headers,
            "X-Praxis-ExtProc-Response=run-4iz0fvk"
        ));
        assert!(!extproc_header_expectation_matches(
            headers,
            "X-Praxis-ExtProc-Response=another-run"
        ));
        assert!(!extproc_header_expectation_matches(
            "HTTP/2 200\r\nX-Praxis-ExtProc-Response: \r\n",
            "X-Praxis-ExtProc-Response=run-4iz0fvk"
        ));
        assert!(!extproc_header_expectation_matches(headers, "bad header=run-4iz0fvk"));
    }

    #[test]
    fn signal_transition_requires_body_digest_but_not_extra_mutation_oracle() {
        assert!(transition_request_oracles_ready(
            Some("x-qualification-request-sha256"),
            None,
            "grid-model",
            "smoke prompt"
        ));
        assert!(transition_request_oracles_ready(
            Some("x-qualification-request-sha256"),
            Some("extproc-mutated"),
            "grid-model",
            "smoke prompt"
        ));
        assert!(!transition_request_oracles_ready(
            None,
            None,
            "grid-model",
            "smoke prompt"
        ));
        assert!(!transition_request_oracles_ready(
            Some("not a header"),
            None,
            "grid-model",
            "smoke prompt"
        ));
        assert!(!transition_request_oracles_ready(
            Some("x-qualification-request-sha256"),
            Some("smoke"),
            "grid-model",
            "smoke prompt"
        ));
    }

    #[test]
    fn request_body_uses_configured_prompt_but_evidence_redacts_it() {
        let body = render_request_body("model-a", "unique-test-prompt");
        assert_eq!(body["messages"][0]["content"], "unique-test-prompt");
        assert_eq!(
            redacted_request_body(&body)["messages"][0]["content"],
            "[SYNTHETIC_TEST_PROMPT_OMITTED]"
        );
        assert!(mutation_oracle_is_distinct(
            Some("extproc-mutated"),
            "model-a",
            "unique-test-prompt"
        ));
        assert!(!mutation_oracle_is_distinct(
            Some("unique"),
            "model-a",
            "unique-test-prompt"
        ));
    }

    #[test]
    fn simulator_config_must_be_mounted_in_the_observed_container() {
        let pod = serde_json::json!({
            "spec": {
                "volumes": [{"name":"config","configMap":{"name":"sim-config"}}],
                "containers": [{"name":"sim","volumeMounts":[{"name":"config","mountPath":"/etc/sim"}]}]
            }
        });
        assert!(pod_mounts_configmap(&pod, "sim", "sim-config"));
        assert!(!pod_mounts_configmap(&pod, "sim", "other-config"));
        assert!(!pod_mounts_configmap(&pod, "sidecar", "sim-config"));
    }

    #[test]
    fn sensitive_secret_json_data_is_redacted() {
        let mut secret = serde_json::json!({
            "apiVersion":"v1",
            "kind":"Secret",
            "metadata":{"name":"test"},
            "data":{"token":"dG9rZW4=","tls.key":"cHJpdmF0ZQ=="},
            "stringData":{"password":"plain"}
        });
        redact_sensitive_json(&mut secret, "");
        let text = secret.to_string();
        assert!(!text.contains("dG9rZW4="));
        assert!(!text.contains("cHJpdmF0ZQ=="));
        assert!(!text.contains("plain"));
    }

    #[test]
    fn capture_redaction_preserves_valid_json_when_message_contains_a_quoted_url() {
        let input = r#"{"kind":"Event","message":"Liveness probe failed: Get \"https://api.cluster.example.test:6443/readyz\": dial tcp 10.0.0.8:8081: connection refused","authorization":"Bearer sensitive-token"}"#;

        let sanitized = sanitize_capture_text(input);
        let value: Value = serde_json::from_str(&sanitized).expect("sanitized event remains valid JSON");
        let message = value["message"].as_str().expect("event message is a string");

        assert!(message.contains("[REDACTED_URL]"));
        assert!(message.contains("[REDACTED_IP]:8081"));
        assert!(!message.contains("api.cluster.example.test"));
        assert!(!message.contains("10.0.0.8"));
        assert_eq!(value["authorization"], "[REDACTED]");
    }

    #[test]
    fn literal_sensitive_pod_environment_values_are_redacted() {
        let mut pod = serde_json::json!({
            "kind": "Pod",
            "spec": {"containers": [{"env": [
                {"name": "AZURE_OPENAI_API_KEY", "value": "sensitive-value"},
                {"name": "MODEL_ENDPOINT_URL", "value": "https://private.example.test/path"},
                {"name": "LOG_LEVEL", "value": "info"}
            ]}]}
        });
        redact_sensitive_json(&mut pod, "");
        let text = pod.to_string();
        assert!(!text.contains("sensitive-value"));
        assert!(!text.contains("private.example.test"));
        assert!(text.contains("info"));
    }

    #[test]
    fn private_route_hostnames_and_endpoint_addresses_are_redacted() {
        let mut route = serde_json::json!({
            "spec": {"hostnames": ["private.example.test"]},
            "endpoints": [{"addresses": ["10.0.0.7"], "targetRef": {"name": "sim-pod"}}]
        });
        redact_sensitive_json(&mut route, "");
        assert_eq!(route["spec"]["hostnames"], "[REDACTED]");
        assert_eq!(route["endpoints"][0]["addresses"], "[REDACTED]");
        assert_eq!(route["endpoints"][0]["targetRef"]["name"], "sim-pod");
    }

    #[test]
    fn auth_token_is_not_a_command_line_argument() {
        let result = crate::Cli::command().try_get_matches_from(["xtask", "e2e", "grid-llmd", "--auth-token"]);
        assert!(matches!(
            result,
            Err(error) if error.kind() == clap::error::ErrorKind::UnknownArgument
        ));
    }

    #[test]
    fn context_evidence_does_not_contain_private_api_hostnames() {
        assert_eq!(
            sanitize_context("default/api.cluster.example.test:6443/admin"),
            "<context-redacted>"
        );
        assert_eq!(
            sanitize_context("https://api.cluster.example.test:6443"),
            "<context-url-redacted>"
        );
        assert_eq!(sanitize_context("<required>"), "<required>");
    }

    #[test]
    fn data_science_cluster_gate_is_queried_as_cluster_scoped() {
        assert!(is_cluster_scoped_gate_resource_kind("datasciencecluster"));
        assert!(is_cluster_scoped_gate_resource_kind("datascienceclusters"));
        assert!(is_cluster_scoped_gate_resource_kind(
            "datascienceclusters.datasciencecluster.opendatahub.io"
        ));
        assert!(!is_cluster_scoped_gate_resource_kind("maastenantconfig"));
    }

    #[test]
    fn gate_contract_rejects_unrelated_resource_kinds_and_weak_readiness_conditions() {
        assert!(validate_gate_resource_contract("rhoai", "datasciencecluster", Some("Ready")).is_ok());
        assert!(validate_gate_resource_contract("rhoai", "deployment", Some("Ready")).is_err());
        assert!(validate_gate_resource_contract("rhoai", "datasciencecluster", Some("exists")).is_err());
        assert!(
            validate_gate_resource_contract("maas", "maastenantconfigs.maas.opendatahub.io", Some("Ready")).is_ok()
        );
        assert!(validate_gate_resource_contract("maas", "aitenants.maas.opendatahub.io", Some("Ready")).is_ok());
        assert!(validate_gate_resource_contract("maas", "configs.maas.opendatahub.io", Some("Ready")).is_ok());
        assert!(validate_gate_resource_contract("maas", "aigateway", Some("Ready")).is_err());
        assert!(validate_gate_resource_contract("maas", "datasciencecluster", Some("Ready")).is_err());
        assert!(validate_gate_resource_contract("maas", "deployment", Some("Available")).is_err());
        assert!(validate_gate_resource_contract("maas", "maastenantconfig", Some("exists")).is_err());
        assert!(validate_gate_resource_contract("grid", "gridnetwork", Some("exists")).is_ok());
        assert!(validate_gate_resource_contract("enrollment", "siteinvite", Some("Ready")).is_ok());
        assert!(validate_gate_resource_contract("gateway", "gateway", Some("Programmed")).is_ok());
        assert!(validate_gate_resource_contract("gateway", "deployment", Some("Programmed")).is_err());
        assert!(validate_gate_resource_contract("gateway", "gateway", Some("exists")).is_err());
    }

    #[test]
    fn grid_contract_uses_current_routing_map_status_schema() {
        let current = serde_json::json!({
            "status": {"routingMapStatus": [{"gatewayName": "consumer", "phase": "Distributed"}]}
        });
        let legacy = serde_json::json!({
            "status": {"overlayStatus": [{"gatewayName": "consumer", "phase": "Distributed"}]}
        });
        let both = serde_json::json!({
            "status": {
                "overlayStatus": [{"gatewayName": "wrong", "phase": "Pending"}],
                "routingMapStatus": [{"gatewayName": "consumer", "phase": "Distributed"}]
            }
        });

        assert_eq!(grid_routing_map_status_entries(&current).0, "routingMapStatus");
        assert_eq!(grid_routing_map_status_entries(&current).1.unwrap().len(), 1);
        assert_eq!(grid_routing_map_status_entries(&legacy), ("routingMapStatus", None));
        assert_eq!(
            grid_routing_map_status_entries(&both).1.unwrap()[0]["gatewayName"],
            "consumer"
        );
    }

    #[test]
    fn grid_contract_uses_current_praxis_config_status_schema() {
        let current = serde_json::json!({
            "status": {"praxisConfigStatus": [{"gatewayName": "consumer", "phase": "Rendered"}]}
        });
        let legacy = serde_json::json!({
            "status": {"consumerConfigStatus": [{"gatewayName": "consumer", "phase": "Rendered"}]}
        });

        assert_eq!(grid_praxis_config_status_entries(&current).unwrap().len(), 1);
        assert!(grid_praxis_config_status_entries(&legacy).is_none());
    }

    #[test]
    fn runtime_condition_requires_observed_generation() {
        let ready_without_generation =
            r#"{"metadata":{"generation":1},"status":{"conditions":[{"type":"Ready","status":"True"}]}}"#;
        assert!(!resource_condition(ready_without_generation, "Ready").0);
    }

    #[test]
    fn serving_graph_requires_current_pool_condition_and_exact_pod_selectors() {
        let ready_pool = serde_json::json!({
            "metadata": {"generation": 3},
            "status": {"conditions": [{"type": "Accepted", "status": "True", "observedGeneration": 3}]}
        });
        assert_eq!(current_pool_status_ready(&ready_pool), Some(true));
        let stale_pool = serde_json::json!({
            "metadata": {"generation": 3},
            "status": {"conditions": [{"type": "Accepted", "status": "True", "observedGeneration": 2}]}
        });
        assert_eq!(current_pool_status_ready(&stale_pool), Some(false));

        let labels = serde_json::json!({"app": "sim-a", "component": "decode"});
        let pool_selector = serde_json::json!({"matchLabels": {"app": "sim-a"}});
        assert!(labels_match(&labels, &pool_selector["matchLabels"]));
        assert!(!labels_match(&labels, &serde_json::json!({"app": "sim-b"})));
    }

    #[test]
    fn serving_graph_passes_the_inferencepool_matchlabels_selector_to_backend_inspection() {
        let digest = format!("sha256:{}", "b".repeat(64));
        let image_id = format!("docker-pullable://ghcr.io/llm-d/sim@{digest}");
        let snapshot = serving_snapshot(&image_id);
        let service_ref = ResourceRef {
            namespace: "test-ns".to_owned(),
            kind: "service".to_owned(),
            name: "pool-a-svc".to_owned(),
        };
        let service = serde_json::json!({
            "metadata": {"name": "pool-a-svc"},
            "spec": {"selector": {"app": "pool-a"}, "ports": [{"port": 8000}]}
        });
        let pool = serde_json::json!({
            "spec": {"selector": {"matchLabels": {"app": "pool-a"}}}
        });
        let selector = inference_pool_service_selector(&pool);
        assert!(selector.is_some());
        assert!(
            inspect_service_target_objects(
                &service_ref,
                &service,
                &snapshot,
                selector,
                Some(8000),
                Some(&digest),
                "llm-d-sim",
            )
            .passed
        );
    }

    #[test]
    fn serving_graph_requires_disjoint_ready_simulator_pod_sets_for_each_pool() {
        let disjoint = vec![
            serving_pool_record("pool-a", &["pod-a-1", "pod-a-2"]),
            serving_pool_record("pool-b", &["pod-b-1"]),
        ];
        assert!(matches!(
            pool_backend_identity_separation(&disjoint),
            PoolBackendIdentityCheck::Distinct(_)
        ));

        let partially_shared = vec![
            serving_pool_record("pool-a", &["pod-a-1", "shared-pod"]),
            serving_pool_record("pool-b", &["shared-pod", "pod-b-1"]),
        ];
        assert!(matches!(
            pool_backend_identity_separation(&partially_shared),
            PoolBackendIdentityCheck::Overlap(detail)
                if detail.contains("pool-a") && detail.contains("pool-b") && detail.contains("shared-pod")
        ));

        let missing_identity = vec![
            serving_pool_record("pool-a", &["pod-a-1"]),
            serving_pool_record("pool-b", &[]),
        ];
        assert!(matches!(
            pool_backend_identity_separation(&missing_identity),
            PoolBackendIdentityCheck::Incomplete(_)
        ));
    }

    fn serving_pool_record(pool: &str, backend_ready_pod_uids: &[&str]) -> ServingPoolRecord {
        ServingPoolRecord {
            pool: pool.to_owned(),
            graph_passed: true,
            model_resource: format!("test-ns/llminferenceservice/{pool}"),
            pool_ready: true,
            model_reports_pool: true,
            model_reports_epp_service: true,
            epp_service: format!("test-ns/service/{pool}-epp"),
            epp_ready_pod_uids: vec![format!("{pool}-epp-pod")],
            epp_deployments: vec![format!("{pool}-epp")],
            epp_runtime_image_ids: vec![format!("epp-image-{pool}")],
            backend_service: format!("test-ns/service/{pool}-sim"),
            backend_ready_pod_uids: backend_ready_pod_uids.iter().map(|uid| (*uid).to_owned()).collect(),
            simulator_runtime_image_ids: vec![format!("sim-image-{pool}")],
            observed_epoch_seconds: 1,
        }
    }

    #[test]
    fn serving_graph_rejects_stale_endpoint_uids_unready_deployments_and_wrong_images() {
        let digest = format!("sha256:{}", "b".repeat(64));
        let image_id = format!("docker-pullable://ghcr.io/llm-d/sim@{digest}");
        let mut snapshot = serving_snapshot(&image_id);
        let service_ref = ResourceRef {
            namespace: "test-ns".to_owned(),
            kind: "service".to_owned(),
            name: "pool-a-svc".to_owned(),
        };
        let service = serde_json::json!({
            "metadata": {"name": "pool-a-svc"},
            "spec": {"selector": {"app": "pool-a"}, "ports": [{"port": 8000}]}
        });
        let pool_selector = serde_json::json!({"matchLabels": {"app": "pool-a"}});
        assert!(inspect_sim_fixture(&service_ref, &service, &snapshot, &pool_selector, &digest).passed);

        snapshot.endpoint_slices["items"][0]["endpoints"][0]["targetRef"]["uid"] = serde_json::json!("stale-pod-uid");
        assert!(!inspect_sim_fixture(&service_ref, &service, &snapshot, &pool_selector, &digest).passed);

        snapshot = serving_snapshot(&image_id);
        snapshot.deployments["items"][0]["status"]["readyReplicas"] = serde_json::json!(0);
        assert!(!inspect_sim_fixture(&service_ref, &service, &snapshot, &pool_selector, &digest).passed);

        snapshot = serving_snapshot(&image_id);
        let wrong_digest = format!("sha256:{}", "c".repeat(64));
        assert!(
            !inspect_service_target_objects(
                &service_ref,
                &service,
                &snapshot,
                Some(&pool_selector),
                Some(8000),
                Some(&wrong_digest),
                "llm-d-sim",
            )
            .passed
        );
    }

    fn serving_snapshot(image_id: &str) -> ServingNamespaceSnapshot {
        let pod = serde_json::json!({
            "metadata": {
                "name": "pool-a-sim-0",
                "namespace": "test-ns",
                "uid": "pod-uid-1",
                "labels": {"app": "pool-a"},
                "ownerReferences": [{"kind": "ReplicaSet", "name": "pool-a-sim-rs", "uid": "rs-uid-1", "controller": true}]
            },
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "True"}],
                "containerStatuses": [{"name": "sim", "ready": true, "image": "ghcr.io/llm-d/sim", "imageID": image_id}]
            }
        });
        let endpoint_slices = serde_json::json!({"items": [{
            "metadata": {"labels": {"kubernetes.io/service-name": "pool-a-svc"}},
            "endpoints": [{
                "conditions": {"ready": true},
                "targetRef": {"kind": "Pod", "namespace": "test-ns", "name": "pool-a-sim-0", "uid": "pod-uid-1"}
            }]
        }]});
        let replica_sets = serde_json::json!({"items": [{
            "metadata": {
                "name": "pool-a-sim-rs",
                "uid": "rs-uid-1",
                "ownerReferences": [{"kind": "Deployment", "name": "pool-a-sim", "uid": "deployment-uid-1"}]
            }
        }]});
        let deployments = serde_json::json!({"items": [{
            "metadata": {"name": "pool-a-sim", "uid": "deployment-uid-1", "generation": 1},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "pool-a"}}},
            "status": {"observedGeneration": 1, "availableReplicas": 1, "readyReplicas": 1}
        }]});
        ServingNamespaceSnapshot {
            pods: serde_json::json!({"items": [pod]}),
            endpoint_slices,
            replica_sets,
            deployments,
        }
    }

    fn inspect_sim_fixture(
        service_ref: &ResourceRef,
        service: &Value,
        snapshot: &ServingNamespaceSnapshot,
        pool_selector: &Value,
        digest: &str,
    ) -> ServiceTargetInspection {
        inspect_service_target_objects(
            service_ref,
            service,
            snapshot,
            Some(pool_selector),
            Some(8000),
            Some(digest),
            "llm-d-sim",
        )
    }
}
