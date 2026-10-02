//! Helpers for `target/e2e-extended/report.json` metadata.

use std::{
    env, fs,
    path::Path,
    sync::{Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};

/// Serialize concurrent report read-modify-write updates within one test process.
fn report_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Default report path (`E2E_EXTENDED_REPORT` override).
pub(crate) fn report_path() -> std::path::PathBuf {
    env::var("E2E_EXTENDED_REPORT").map_or_else(
        |_| std::path::PathBuf::from("target/e2e-extended/report.json"),
        std::path::PathBuf::from,
    )
}

/// Topology profile selected via `E2E_EXTENDED_PROFILE`.
pub(crate) fn load_profile_name() -> String {
    env::var("E2E_EXTENDED_PROFILE").unwrap_or_else(|_| "maas_two_hop_fd_streamed".to_owned())
}

/// One idle-matrix cell result (How? schema).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IdleMatrixResult {
    pub idle_secs: u64,
    pub pool_settings_ref: String,
    pub connection_id: String,
    pub upstream_connection_id: String,
    /// `None` until access-log evidence can determine reuse.
    pub connection_reused: Option<bool>,
    pub outcome: String,
    pub error: Option<String>,
}

/// Top-level qualification run metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ExtendedRunMetadata {
    pub profile: String,
    pub chain: String,
    pub chain_reason: String,
    pub ext_proc_hops: u32,
    pub envoy_version: String,
    pub istio_version: String,
    pub praxis_extproc_image_digest: String,
    pub tls_validation_mode: String,
    pub tls_negative_scenario: Option<String>,
    pub ext_proc_modes: ExtProcModes,
    pub idle_matrix_results: Vec<IdleMatrixResult>,
    pub started_at: String,
    pub finished_at: String,
    pub outcome: String,
}

/// Request/response `BodySendMode` pair recorded in the report.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ExtProcModes {
    pub request: String,
    pub response: String,
}

impl ExtendedRunMetadata {
    /// Serialize `self` to `path` (creates parent directories).
    pub(crate) fn write_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, serde_json::to_string_pretty(self).expect("serialize report"))?;
        Ok(())
    }

    /// Read-modify-write `report.json` under the process-wide report lock.
    pub(crate) fn update_locked(
        update: impl FnOnce(Option<ExtendedRunMetadata>) -> ExtendedRunMetadata,
    ) -> std::io::Result<()> {
        let _guard = report_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = report_path();
        let existing = if path.exists() {
            let raw = fs::read_to_string(&path)?;
            Some(serde_json::from_str(&raw).map_err(std::io::Error::other)?)
        } else {
            None
        };
        let meta = update(existing);
        meta.write_to(&path)
    }

    /// Publish `chain=executed` for a successful chained / FD scenario run.
    pub(crate) fn publish_chain_executed(profile: &str, reason: &str) -> std::io::Result<()> {
        let now = utc_now_rfc3339();
        Self::update_locked(|existing| {
            if let Some(mut meta) = existing {
                meta.chain = "executed".into();
                meta.chain_reason = reason.into();
                meta.profile = profile.into();
                meta.finished_at = now.clone();
                // Successful publish must not retain a prior fail/seeded outcome.
                meta.outcome = "pass".into();
                meta
            } else {
                ExtendedRunMetadata {
                    profile: profile.into(),
                    chain: "executed".into(),
                    chain_reason: reason.into(),
                    ext_proc_hops: 2,
                    envoy_version: "unknown".into(),
                    istio_version: "unknown".into(),
                    praxis_extproc_image_digest: "unknown".into(),
                    tls_validation_mode: "pending_idle_tls_tranche".into(),
                    tls_negative_scenario: None,
                    ext_proc_modes: ExtProcModes {
                        request: "FULL_DUPLEX_STREAMED".into(),
                        response: "FULL_DUPLEX_STREAMED".into(),
                    },
                    idle_matrix_results: vec![],
                    started_at: now.clone(),
                    finished_at: now.clone(),
                    outcome: "pass".into(),
                }
            }
        })
    }

    /// Append one idle-matrix cell under the report lock.
    pub(crate) fn publish_idle_matrix_result(result: IdleMatrixResult) -> std::io::Result<()> {
        Self::update_locked(|existing| {
            let now = utc_now_rfc3339();
            let mut meta = existing.unwrap_or_else(|| ExtendedRunMetadata {
                profile: load_profile_name(),
                chain: "deferred".into(),
                chain_reason: "idle matrix cell before chain publish".into(),
                ext_proc_hops: 2,
                envoy_version: "unknown".into(),
                istio_version: "unknown".into(),
                praxis_extproc_image_digest: "unknown".into(),
                tls_validation_mode: "pending_idle_tls_tranche".into(),
                tls_negative_scenario: None,
                ext_proc_modes: ExtProcModes {
                    request: "FULL_DUPLEX_STREAMED".into(),
                    response: "FULL_DUPLEX_STREAMED".into(),
                },
                idle_matrix_results: vec![],
                started_at: now.clone(),
                finished_at: now.clone(),
                outcome: "pass".into(),
            });
            meta.idle_matrix_results.push(result);
            meta.finished_at = now;
            meta
        })
    }
}

/// UTC RFC3339 timestamp (matches `hack/e2e-extended-report.sh`).
fn utc_now_rfc3339() -> String {
    if let Ok(output) = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        && output.status.success()
    {
        return String::from_utf8_lossy(&output.stdout).trim().to_owned();
    }
    "unknown".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_json_round_trips() {
        let meta = ExtendedRunMetadata {
            profile: "maas_two_hop_fd_streamed".into(),
            chain: "deferred".into(),
            chain_reason: "harness tranche; chained scenarios not yet executed".into(),
            ext_proc_hops: 2,
            envoy_version: "test".into(),
            istio_version: "test".into(),
            praxis_extproc_image_digest: "sha256:deadbeef".into(),
            tls_validation_mode: "pending_idle_tls_tranche".into(),
            tls_negative_scenario: None,
            ext_proc_modes: ExtProcModes {
                request: "FULL_DUPLEX_STREAMED".into(),
                response: "FULL_DUPLEX_STREAMED".into(),
            },
            idle_matrix_results: vec![],
            started_at: "2026-01-01T00:00:00Z".into(),
            finished_at: "2026-01-01T00:01:00Z".into(),
            outcome: "pass".into(),
        };

        let scratch = env::temp_dir().join(format!("praxis-extproc-report-ut-{}-roundtrip", std::process::id()));
        let path = scratch.join("report.json");
        meta.write_to(&path).expect("write report");
        let json = fs::read_to_string(&path).expect("read report");
        drop(fs::remove_dir_all(&scratch));

        let back: ExtendedRunMetadata = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.profile, "maas_two_hop_fd_streamed", "profile round-trip");
        assert_eq!(back.chain, "deferred", "chain status round-trip");
        assert_eq!(back.ext_proc_hops, 2, "hop count round-trip");
    }
}
