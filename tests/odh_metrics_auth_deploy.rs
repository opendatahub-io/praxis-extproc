//! ODH-only: `metrics_auth` needs SA automount + metrics `ClusterRole`s.
//!
//! Non-OpenShift overlays are unchanged (base keeps the cluster default SA).
//! This locks the ODH merge resolution with #57 so we do not drop the metrics
//! roles again or disable automount on the ODH ExtProc Deployments.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    reason = "deploy fixture around checked-in YAML"
)]

const ODH_RBAC_KUST: &str = include_str!("../deploy/overlays/odh/rbac/kustomization.yaml");
const ODH_KUST: &str = include_str!("../deploy/overlays/odh/kustomization.yaml");
const ODH_PRE_PATCH: &str = include_str!("../deploy/overlays/odh/pre-processing/deployment-patch.yaml");

#[test]
fn odh_keeps_metrics_roles_and_sa_automount() {
    assert!(
        ODH_RBAC_KUST.contains("metrics-auth-clusterrole.yaml"),
        "ODH rbac must keep metrics-auth ClusterRole (TokenReview / SAR)"
    );
    assert!(
        ODH_RBAC_KUST.contains("metrics-reader-clusterrole.yaml"),
        "ODH rbac must keep metrics-reader ClusterRole (Prometheus GET /metrics)"
    );
    assert!(
        !ODH_RBAC_KUST
            .lines()
            .any(|l| matches!(l.trim(), "- clusterrole.yaml" | "- clusterrolebinding.yaml")),
        "ODH rbac must not restore payload-processing-reader"
    );
    assert!(
        ODH_KUST.contains("automountServiceAccountToken") && ODH_KUST.contains("value: true"),
        "ODH post-auth Deployment must automount the SA token"
    );
    assert!(
        ODH_PRE_PATCH.contains("automountServiceAccountToken") && ODH_PRE_PATCH.contains("value: true"),
        "ODH pre-auth Deployment must automount the SA token"
    );
}
