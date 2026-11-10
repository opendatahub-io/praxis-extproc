//! Static checks that ODH ExtProc stays fail-closed on every pre-proc anchor.
//!
//! The ODH `EnvoyFilter` installs `ipp-pre` four times (one per Kuadrant/Istio
//! wasm subFilter name). A single variant reverting to `failure_mode_allow:
//! true` would fail-open when the pre-processor is unavailable. Runtime e2e
//! covers one live filter; this test locks the YAML posture for all four.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    reason = "unit fixture around checked-in YAML"
)]

const ODH_ENVOY_FILTER: &str = include_str!("../deploy/overlays/odh/envoy-filter.yaml");

#[test]
fn odh_ipp_pre_anchors_are_fail_closed() {
    let mut ipp_pre_blocks = 0_usize;
    let mut in_ipp_pre = false;
    let mut saw_failure_mode = false;

    for line in ODH_ENVOY_FILTER.lines() {
        let trimmed = line.trim();
        if trimmed == "name: envoy.filters.http.ext_proc.ipp-pre" {
            assert!(
                !in_ipp_pre || saw_failure_mode,
                "previous ipp-pre block missing failure_mode_allow"
            );
            in_ipp_pre = true;
            saw_failure_mode = false;
            ipp_pre_blocks += 1;
            continue;
        }
        if !in_ipp_pre {
            continue;
        }
        if trimmed.starts_with("name: envoy.filters.http.") && !trimmed.contains("ipp-pre") {
            assert!(saw_failure_mode, "ipp-pre block ended without failure_mode_allow");
            in_ipp_pre = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("failure_mode_allow:") {
            let value = rest.trim();
            assert_eq!(
                value, "false",
                "ipp-pre must fail closed; found failure_mode_allow: {value}"
            );
            saw_failure_mode = true;
        }
    }

    if in_ipp_pre {
        assert!(saw_failure_mode, "trailing ipp-pre block missing failure_mode_allow");
    }

    assert_eq!(
        ipp_pre_blocks, 4,
        "expected four ODH ipp-pre anchors (Istio/Kuadrant subFilter variants), found {ipp_pre_blocks}"
    );
}
