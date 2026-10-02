//! Shared helpers for the extended qualification tier.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::Deserialize;

use crate::fixtures::{gateway_url, http_client};

/// Hop-1 request-body digest header (pre-IPP).
pub(crate) const HOP_DIGEST_HEADER_1: &str = "x-qualification-request-sha256-hop-1";
/// Hop-2 request-body digest header (post-IPP).
pub(crate) const HOP_DIGEST_HEADER_2: &str = "x-qualification-request-sha256-hop-2";

/// Lowercase hex SHA-256 of `bytes` (matches [`praxis_extproc::e2e::body_oracle::hex_sha256`]).
pub(crate) fn hex_sha256(bytes: &[u8]) -> String {
    praxis_extproc::e2e::body_oracle::hex_sha256(bytes)
}

/// Topology inventory entry from `deploy/overlays/e2e-extended/topology/`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TopologyProfile {
    pub name: String,
    pub ext_proc_hops: u32,
    pub ext_proc_modes: TopologyExtProcModes,
    pub tls: String,
    pub idle_matrix: Vec<IdleMatrixCell>,
}

/// Request/response body modes declared in topology YAML.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TopologyExtProcModes {
    pub request: String,
    pub response: String,
}

/// One idle-matrix cell from topology inventory.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct IdleMatrixCell {
    pub pool_settings_ref: String,
    pub idle_secs: Option<u64>,
}

/// Load the active topology profile (`E2E_EXTENDED_PROFILE`).
pub(crate) fn load_topology_profile() -> TopologyProfile {
    let name = super::load_profile_name();
    let path = repo_root().join(format!("deploy/overlays/e2e-extended/topology/{name}.yaml"));
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("read topology {}: {error}", path.display()));
    serde_yaml::from_str(&raw).unwrap_or_else(|error| panic!("parse topology {}: {error}", path.display()))
}

/// Repository root (`CARGO_MANIFEST_DIR` for integration tests).
pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `kubectl` context for e2e (`E2E_CONTEXT`, default `kind-praxis-e2e`).
pub(crate) fn e2e_kubectl_context() -> String {
    std::env::var("E2E_CONTEXT").unwrap_or_else(|_| "kind-praxis-e2e".to_owned())
}

/// Apply a kustomize overlay directory under the repo (`kubectl apply -k`).
pub(crate) fn kubectl_apply_k(overlay: impl AsRef<Path>) -> std::io::Result<()> {
    let overlay = overlay.as_ref();
    let status = Command::new("kubectl")
        .arg("--context")
        .arg(e2e_kubectl_context())
        .arg("apply")
        .arg("-k")
        .arg(overlay)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "kubectl apply -k {} failed with {status}",
            overlay.display()
        )));
    }
    Ok(())
}

/// Apply a flat manifest (`kubectl apply -f`).
pub(crate) fn kubectl_apply_f(manifest: impl AsRef<Path>) -> std::io::Result<()> {
    let manifest = manifest.as_ref();
    let status = Command::new("kubectl")
        .arg("--context")
        .arg(e2e_kubectl_context())
        .arg("apply")
        .arg("-f")
        .arg(manifest)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "kubectl apply -f {} failed with {status}",
            manifest.display()
        )));
    }
    Ok(())
}

/// Poll until both ext-proc Deployments report Ready; panics if the deadline expires.
pub(crate) async fn wait_ext_proc_rollout(timeout: Duration) {
    let ctx = e2e_kubectl_context();
    let deadline = tokio::time::Instant::now() + timeout;
    for deploy in ["payload-pre-processing", "payload-processing"] {
        let mut ready = false;
        while tokio::time::Instant::now() < deadline {
            let ok = Command::new("kubectl")
                .args([
                    "--context",
                    &ctx,
                    "-n",
                    "istio-system",
                    "rollout",
                    "status",
                    &format!("deployment/{deploy}"),
                    "--timeout=5s",
                ])
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        assert!(
            ready,
            "deployment/{deploy} not ready within {timeout:?}"
        );
    }
}

/// POST raw bytes to `path` (relative to gateway base URL).
pub(crate) async fn post_raw(path: &str, body: &[u8]) -> reqwest::Response {
    let client = http_client();
    let url = format!("{}{}", gateway_url(), path);
    client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.to_vec())
        .send()
        .await
        .expect("post_raw request failed")
}

/// POST a JSON chat completion; returns the response and the client body digest.
pub(crate) async fn chat_completion_with_digest(model: &str, content: &str) -> (reqwest::Response, String) {
    let body = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": content }]
    });
    let bytes = serde_json::to_vec(&body).expect("serialize chat body");
    let digest = hex_sha256(&bytes);
    let resp = post_raw("/v1/chat/completions", &bytes).await;
    (resp, digest)
}

/// POST chat completion with an explicit JSON body value.
pub(crate) async fn chat_json_with_digest(body: &serde_json::Value) -> (reqwest::Response, String) {
    let bytes = serde_json::to_vec(body).expect("serialize chat body");
    let digest = hex_sha256(&bytes);
    let resp = post_raw("/v1/chat/completions", &bytes).await;
    (resp, digest)
}

/// POST with a chunked request body stream (multiple client chunks).
pub(crate) async fn post_raw_chunked(path: &str, chunks: Vec<Vec<u8>>) -> (reqwest::Response, String) {
    let mut combined = Vec::new();
    for chunk in &chunks {
        combined.extend_from_slice(chunk);
    }
    let digest = hex_sha256(&combined);
    let client = http_client();
    let url = format!("{}{}", gateway_url(), path);
    let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::io::Error>));
    let resp = client
        .post(url)
        .header(header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await
        .expect("chunked post failed");
    (resp, digest)
}

/// Compare hop oracle headers to the client-side digest.
pub(crate) fn assert_hop_digests(client_digest: &str, headers: &HeaderMap) {
    for (hop, name) in [(1, HOP_DIGEST_HEADER_1), (2, HOP_DIGEST_HEADER_2)] {
        let value = headers
            .get(name)
            .unwrap_or_else(|| panic!("missing {name} for hop {hop}"));
        let got = value.to_str().unwrap_or_else(|_| panic!("invalid {name} header value"));
        assert_eq!(got, client_digest, "hop {hop} digest mismatch (header {name})");
    }
}

/// Effective idle seconds for a matrix cell.
pub(crate) fn effective_idle_secs(cell: &IdleMatrixCell) -> u64 {
    cell.idle_secs.unwrap_or_else(|| {
        std::env::var("QUALIFICATION_IDLE_SECS")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(300)
    })
}

/// HTTP client with a single idle connection (connection-reuse experiments).
pub(crate) fn http_client_single_pool() -> reqwest::Client {
    use reqwest::header;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer llm-katan-openai-key"),
    );
    reqwest::Client::builder()
        .timeout(crate::fixtures::REQUEST_TIMEOUT)
        .pool_max_idle_per_host(1)
        .default_headers(headers)
        .build()
        .expect("failed to build single-pool HTTP client")
}

/// Path to pool-settings manifest for a topology cell reference.
pub(crate) fn pool_settings_manifest(pool_settings_ref: &str) -> PathBuf {
    repo_root().join(format!(
        "deploy/overlays/e2e-extended/pool-configs/{pool_settings_ref}.yaml"
    ))
}

/// TLS scenario overlay directory (`deploy/overlays/e2e-extended/tls/<scenario>/`).
pub(crate) fn tls_overlay_dir(scenario: &str) -> PathBuf {
    repo_root().join(format!("deploy/overlays/e2e-extended/tls/{scenario}"))
}
