//! Stale-idle + TLS/mTLS matrix (qualification tier).

use std::time::Duration;

use super::{
    helpers::{
        IdleMatrixCell, apply_tls_scenario_overlay, assert_ext_proc_error_details, assert_hop_digests,
        assert_negative_tls_response, chat_completion_with_digest, e2e_kubectl_context, effective_idle_secs,
        fetch_gateway_access_log_tail, http_client_single_pool, kubectl_apply_f, latest_connection_log_line,
        load_topology_profile, parse_connection_ids, pool_settings_manifest, repo_root, tls_overlay_dir,
        wait_ext_proc_rollout,
    },
    report::{ExtendedRunMetadata, IdleMatrixResult},
};
use crate::fixtures::{ensure_gateway_ready, gateway_url};

const TLS_SCENARIOS: &[(&str, &str)] = &[
    ("positive", "positive control"),
    ("server-untrusted", "server_untrusted"),
    ("server-expired", "server_expired"),
    ("server-wrong-san", "server_wrong_san"),
    ("client-missing", "client_missing"),
    ("client-untrusted", "client_untrusted"),
    ("client-expired", "client_expired"),
];

const NEGATIVE_SCENARIOS: &[(&str, &str)] = &[
    ("server-untrusted", "server_untrusted"),
    ("server-expired", "server_expired"),
    ("server-wrong-san", "server_wrong_san"),
    ("client-missing", "client_missing"),
    ("client-untrusted", "client_untrusted"),
    ("client-expired", "client_expired"),
];

/// Apply `deploy/overlays/e2e-extended/tls/<scenario>/` and wait for rollout.
async fn apply_tls_scenario(scenario: &str) -> std::io::Result<()> {
    apply_tls_scenario_overlay(scenario)?;
    wait_ext_proc_rollout(Duration::from_secs(180)).await;
    // EnvoyFilter MERGE needs a short settle window on the gateway.
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok(())
}

fn publish_tls_metadata(validation_mode: &str, negative: Option<&str>) {
    let path = super::report::report_path();
    let now = {
        std::process::Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map_or_else(
                || "unknown".into(),
                |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
            )
    };
    let mut meta = if path.exists() {
        let raw = std::fs::read_to_string(&path).expect("read report");
        serde_json::from_str(&raw).expect("parse report")
    } else {
        ExtendedRunMetadata {
            profile: super::report::load_profile_name(),
            chain: "executed".into(),
            chain_reason: "idle/tls tranche".into(),
            ext_proc_hops: 2,
            envoy_version: "unknown".into(),
            istio_version: "unknown".into(),
            praxis_extproc_image_digest: "unknown".into(),
            tls_validation_mode: validation_mode.into(),
            tls_negative_scenario: negative.map(str::to_owned),
            ext_proc_modes: super::report::ExtProcModes {
                request: "FULL_DUPLEX_STREAMED".into(),
                response: "FULL_DUPLEX_STREAMED".into(),
            },
            idle_matrix_results: vec![],
            started_at: now.clone(),
            finished_at: now.clone(),
            outcome: "pass".into(),
        }
    };
    meta.tls_validation_mode = validation_mode.into();
    meta.tls_negative_scenario = negative.map(str::to_owned);
    meta.finished_at = now;
    meta.write_to(&path).expect("write tls metadata");
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn idle_tls_positive_overlay_applies() {
    ensure_gateway_ready().await;
    apply_tls_scenario("positive")
        .await
        .expect("apply tls positive overlay");
    let (resp, digest) = chat_completion_with_digest("gpt-4", "tls positive control").await;
    assert_eq!(
        resp.status(),
        200,
        "positive TLS cell should return 200 when ext-proc is healthy"
    );
    assert_hop_digests(&digest, resp.headers());
    publish_tls_metadata("verify_trust_chain", None);
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn idle_tls_negative_server_untrusted_expects_503_without_oracle() {
    ensure_gateway_ready().await;
    apply_tls_scenario("positive")
        .await
        .expect("baseline positive overlay before negative cell");
    run_negative_tls_cell("server-untrusted", "server_untrusted").await;
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended"]
async fn idle_tls_negative_matrix_all_scenarios() {
    ensure_gateway_ready().await;
    apply_tls_scenario("positive").await.expect("baseline positive overlay");
    for (dir, label) in NEGATIVE_SCENARIOS {
        run_negative_tls_cell(dir, label).await;
        apply_tls_scenario("positive")
            .await
            .unwrap_or_else(|error| panic!("restore positive after {dir}: {error}"));
    }
    // Cluster is back on positive control; clear the last negative label.
    publish_tls_metadata("verify_trust_chain", None);
}

async fn run_negative_tls_cell(scenario_dir: &str, scenario_label: &str) {
    let before = fetch_gateway_access_log_tail(20);
    apply_tls_scenario(scenario_dir)
        .await
        .unwrap_or_else(|error| panic!("apply tls {scenario_dir}: {error}"));

    let (resp, _digest) = chat_completion_with_digest("gpt-4", &format!("negative tls {scenario_label}")).await;
    assert_negative_tls_response(resp.headers(), resp.status());

    // Poll briefly for the negative access-log line.
    let mut last_logs = String::new();
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        last_logs = fetch_gateway_access_log_tail(80);
        let has_details = last_logs.contains("details=");
        let grew = last_logs.len() > before.len() && has_details;
        let has_ext_proc = last_logs.contains("details=ext_proc") || last_logs.contains("ext_proc_error");
        if !(grew || has_ext_proc) {
            continue;
        }
        let candidate = last_logs.as_str();
        if candidate
            .lines()
            .any(|line| line.contains("details=") && line.contains("ext_proc") && line.contains("code=503"))
        {
            let mode = if scenario_label.starts_with("client_") {
                "verify_trust_chain_mtls"
            } else {
                "verify_trust_chain"
            };
            assert_ext_proc_error_details(candidate);
            publish_tls_metadata(mode, Some(scenario_label));
            return;
        }
    }
    panic!("negative TLS {scenario_label}: expected access-log details with ext_proc; last logs:\n{last_logs}");
}

#[tokio::test]
#[ignore = "qualification tier: requires make e2e-setup-extended; long idle wait"]
async fn idle_matrix_post_idle_request_on_reused_connection() {
    ensure_gateway_ready().await;
    // Ensure validated positive TLS (prior negative cells leave bad leaf material).
    apply_tls_scenario("positive")
        .await
        .expect("restore positive TLS before idle matrix");
    // Access-log reuse overlay is part of e2e-extended; re-apply for determinism.
    kubectl_apply_f(repo_root().join("deploy/overlays/e2e-extended/access-log-reuse.yaml"))
        .expect("apply access-log-reuse");

    let profile = load_topology_profile();
    assert!(
        !profile.idle_matrix.is_empty(),
        "topology idle_matrix should not be empty"
    );

    // Fresh matrix results for this qualification run.
    let report_path = super::report::report_path();
    if report_path.exists() {
        let raw = std::fs::read_to_string(&report_path).expect("read report");
        let mut meta: ExtendedRunMetadata = serde_json::from_str(&raw).expect("parse report");
        meta.idle_matrix_results.clear();
        meta.write_to(&report_path).expect("clear idle matrix results");
    }

    for cell in &profile.idle_matrix {
        run_idle_matrix_cell(&profile.name, cell).await;
    }
}

async fn run_idle_matrix_cell(profile_name: &str, cell: &IdleMatrixCell) {
    // Ensure only this cell's pool EnvoyFilter is active (delete sibling refs).
    let ctx = e2e_kubectl_context();
    drop(
        std::process::Command::new("kubectl")
            .args([
                "--context",
                &ctx,
                "-n",
                "istio-system",
                "delete",
                "envoyfilter",
                "e2e-extended-pool-default-5min-keepalive",
                "e2e-extended-pool-aggressive-idle",
                "--ignore-not-found",
            ])
            .status(),
    );
    let pool_path = pool_settings_manifest(&cell.pool_settings_ref);
    kubectl_apply_f(&pool_path).expect("apply pool settings ref");
    wait_ext_proc_rollout(Duration::from_secs(60)).await;

    // Fresh client per cell so pool settings are not confounded by prior TCP state.
    let client = http_client_single_pool();
    let chat_url = format!("{}/v1/chat/completions", gateway_url());

    // Warm-up on the same path/client pool that the post-idle request will use.
    let warmup_body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": format!("idle-matrix warm-up {}", cell.pool_settings_ref) }]
    });
    let warmup_bytes = serde_json::to_vec(&warmup_body).expect("serialize warm-up");
    let logs_before = fetch_gateway_access_log_tail(5);
    let warmup_resp = client
        .post(&chat_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(warmup_bytes)
        .send()
        .await
        .expect("warm-up request");
    assert_eq!(
        warmup_resp.status(),
        200,
        "warm-up chat should succeed for {}",
        cell.pool_settings_ref
    );

    let mut warmup_ids = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let logs = fetch_gateway_access_log_tail(40);
        if let Some(line) = latest_connection_log_line(&logs)
            && let Some(ids) = parse_connection_ids(line)
        {
            // Prefer a line that appeared after warm-up.
            if !logs_before.contains(line) || warmup_ids.is_none() {
                warmup_ids = Some(ids);
                if !logs_before.contains(line) {
                    break;
                }
            }
        }
    }
    let warmup_ids = warmup_ids.expect("warm-up access-log connection_id");

    let idle_secs = effective_idle_secs(cell);
    tokio::time::sleep(Duration::from_secs(idle_secs)).await;

    let body = serde_json::json!({
        "model": "gpt-4",
        "messages": [{ "role": "user", "content": format!("post-idle {}", cell.pool_settings_ref) }]
    });
    let bytes = serde_json::to_vec(&body).expect("serialize");
    let digest = super::helpers::hex_sha256(&bytes);
    // Snapshot access logs before the post-idle request so warm-up lines cannot
    // satisfy the reuse check.
    let logs_before_post = fetch_gateway_access_log_tail(80);
    let resp = client
        .post(&chat_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(bytes)
        .send()
        .await
        .expect("post-idle request");
    assert_eq!(
        resp.status(),
        200,
        "post-idle request should succeed for {}",
        cell.pool_settings_ref
    );
    assert_hop_digests(&digest, resp.headers());

    let mut post_ids = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let logs = fetch_gateway_access_log_tail(80);
        for line in logs.lines().rev() {
            if logs_before_post.contains(line) {
                continue;
            }
            if let Some(ids) = parse_connection_ids(line) {
                if ids.connection_id == warmup_ids.connection_id {
                    post_ids = Some(ids);
                    break;
                }
                if post_ids.is_none() {
                    post_ids = Some(ids);
                }
            }
        }
        if post_ids
            .as_ref()
            .is_some_and(|ids| ids.connection_id == warmup_ids.connection_id)
        {
            break;
        }
    }
    let post_ids = post_ids.expect("post-idle access-log connection_id");
    assert_eq!(
        post_ids.connection_id, warmup_ids.connection_id,
        "downstream CONNECTION_ID must be reused after idle ({})",
        cell.pool_settings_ref
    );
    if !warmup_ids.upstream_connection_id.is_empty()
        && warmup_ids.upstream_connection_id != "0"
        && !post_ids.upstream_connection_id.is_empty()
        && post_ids.upstream_connection_id != "0"
    {
        assert_eq!(
            post_ids.upstream_connection_id, warmup_ids.upstream_connection_id,
            "upstream CONNECTION_ID should be reused when ext-proc path is warm ({})",
            cell.pool_settings_ref
        );
    }

    let result = IdleMatrixResult {
        idle_secs,
        pool_settings_ref: cell.pool_settings_ref.clone(),
        connection_id: post_ids.connection_id,
        upstream_connection_id: post_ids.upstream_connection_id,
        connection_reused: true,
        outcome: "pass".into(),
        error: None,
    };
    ExtendedRunMetadata::publish_chain_executed(
        profile_name,
        "idle matrix cell executed with access-log reuse evidence",
    )
    .expect("publish chain metadata after idle cell");
    let report_path = super::report::report_path();
    if report_path.exists() {
        let raw = std::fs::read_to_string(&report_path).expect("read report");
        let mut meta: ExtendedRunMetadata = serde_json::from_str(&raw).expect("parse report");
        meta.idle_matrix_results.push(result);
        meta.write_to(&report_path).expect("write idle matrix result");
    }
}

#[tokio::test]
#[ignore = "qualification tier: TLS matrix wiring"]
async fn idle_tls_scenario_inventory_covers_how_spec() {
    for (dir, _label) in TLS_SCENARIOS {
        let overlay = tls_overlay_dir(dir);
        let kustomization = overlay.join("kustomization.yaml");
        assert!(kustomization.is_file(), "missing tls overlay for {dir}");
        assert!(
            !overlay.join("placeholder-configmap.yaml").exists(),
            "placeholder configmap must be removed for {dir}"
        );
        assert!(
            overlay.join("plugins-pre.yaml").is_file() && overlay.join("plugins-post.yaml").is_file(),
            "tls scenario {dir} must ship provided-TLS plugin ConfigMaps"
        );
    }
    assert_eq!(
        e2e_kubectl_context(),
        std::env::var("E2E_CONTEXT").unwrap_or_else(|_| "kind-praxis-e2e".to_owned()),
        "kubectl context helper respects E2E_CONTEXT"
    );
}

#[cfg(test)]
mod unit {
    use super::super::helpers::{latest_details_log_line, parse_connection_ids};

    #[test]
    fn parse_connection_ids_from_reuse_line() {
        let line = "[2026-01-01T00:00:00.000Z] connection_id=21 upstream_connection_id=7 code=200";
        let ids = parse_connection_ids(line).expect("parse");
        assert_eq!(ids.connection_id, "21");
        assert_eq!(ids.upstream_connection_id, "7");
    }

    #[test]
    fn latest_details_prefers_newest() {
        let logs = "code=200 details=via_upstream flags=-\ncode=503 details=ext_proc_error flags=-\n";
        let line = latest_details_log_line(logs).expect("line");
        assert!(line.contains("ext_proc_error"));
    }
}
