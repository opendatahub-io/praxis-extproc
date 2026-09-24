// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! Tests for the filter order the ODH overlay `EnvoyFilter` renders.
//!
//! The overlay's `HTTP_FILTER` patches are applied the way istiod applies them,
//! to gateway chains captured from a live Istio gateway, one per Kuadrant
//! anchor variant.

#![allow(
    clippy::tests_outside_test_module,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::missing_docs_in_private_items,
    reason = "tests"
)]
#![allow(missing_docs, reason = "test module")]

use serde_yaml::Value;

// -----------------------------------------------------------------------------
// Constants
// -----------------------------------------------------------------------------

const ENVOY_FILTER: &str = include_str!("../deploy/overlays/odh/envoy-filter.yaml");

const GATEWAY: &str = include_str!("fixtures/istio_gateway_http_filters.yaml");

const IPP_PRE: &str = "envoy.filters.http.ext_proc.ipp-pre";

const IPP: &str = "envoy.filters.http.ext_proc.ipp";

/// Istio's `InferencePool` filter, which binds the endpoint picker.
const EPP: &str = "envoy.filters.http.ext_proc";

const ROUTER: &str = "envoy.filters.http.router";

/// Kuadrant's wasm-shim on RHCL 1.4 (Kuadrant 1.5+).
const WASM: &str = "envoy.filters.http.wasm";

/// Kuadrant's `WasmPlugin` on Istio 1.26-1.29.
const WASMPLUGIN: &str = "extensions.istio.io/wasmplugin/openshift-ingress.kuadrant-maas-default-gateway";

/// Kuadrant's `WasmPlugin` on Istio <=1.25, with Istio's typo.
const WASMPLUGIN_TYPO: &str = "extenstions.istio.io/wasmplugin/openshift-ingress.kuadrant-maas-default-gateway";

/// Kuadrant's `WasmPlugin` translated to a `TrafficExtension` on Istio >=1.30.
const TRAFFIC_EXTENSION: &str =
    "extensions.istio.io/trafficextension/openshift-ingress.kuadrant-maas-default-gateway~istio-translated-wasmplugin";

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[test]
fn epp_runs_after_payload_processing_on_every_kuadrant_variant() {
    for (chain, auth) in [
        (gateway_chain("wasm"), WASM),
        (gateway_chain("wasmplugin"), WASMPLUGIN),
        (with_auth(gateway_chain("wasmplugin"), WASMPLUGIN_TYPO), WASMPLUGIN_TYPO),
        (
            with_auth(gateway_chain("wasmplugin"), TRAFFIC_EXTENSION),
            TRAFFIC_EXTENSION,
        ),
    ] {
        let rendered = apply_http_filter_patches(chain);
        let positions: Vec<usize> = [IPP_PRE, auth, IPP, EPP, ROUTER]
            .iter()
            .map(|name| single_position(&rendered, name))
            .collect();

        assert!(
            positions.is_sorted(),
            "{auth}: want ipp-pre < auth < ipp < EPP < router, got {rendered:?}"
        );
    }
}

#[test]
fn listener_without_auth_anchor_keeps_the_epp() {
    let chain = gateway_chain("wasm").into_iter().filter(|name| name != WASM).collect();
    let rendered = apply_http_filter_patches(chain);

    assert!(
        !rendered.iter().any(|name| name == IPP_PRE || name == IPP),
        "payload processing anchors on auth only: {rendered:?}"
    );
    assert!(
        single_position(&rendered, EPP) < single_position(&rendered, ROUTER),
        "the EPP must survive in front of the router: {rendered:?}"
    );
}

#[test]
fn gateway_without_inference_pool_gets_one_epp_copy() {
    let chain = gateway_chain("wasm").into_iter().filter(|name| name != EPP).collect();
    let rendered = apply_http_filter_patches(chain);

    assert_eq!(
        single_position(&rendered, EPP) + 1,
        single_position(&rendered, ROUTER),
        "the copy lands right before the router: {rendered:?}"
    );
}

#[test]
fn epp_copy_matches_istio_rendered_filter() {
    let patches = http_filter_patches();
    let insert = patches.last().expect("HTTP_FILTER patches should not be empty");

    assert_eq!(
        insert["patch"]["value"],
        gateway()["inference_pool_filter"],
        "the EPP copy must match the filter Istio renders; per-route overrides are keyed on its name"
    );
}

#[test]
fn epp_move_is_the_last_http_filter_pair() {
    let patches = http_filter_patches();
    let tail: Vec<(&str, &str)> = patches
        .iter()
        .skip(patches.len().saturating_sub(2))
        .map(|patch| (operation(patch), anchor(patch)))
        .collect();

    assert_eq!(
        tail,
        [("REMOVE", EPP), ("INSERT_BEFORE", ROUTER)],
        "tooling that reads the EnvoyFilter expects the EPP move last"
    );
}

// -----------------------------------------------------------------------------
// Test Utilities
// -----------------------------------------------------------------------------

/// Apply the overlay's `HTTP_FILTER` patches in list order, as istiod's
/// `patchHTTPFilters` does: subFilter names match exactly, inserts land at the
/// first match and are skipped without one, and `REMOVE` drops every match.
fn apply_http_filter_patches(mut chain: Vec<String>) -> Vec<String> {
    for patch in http_filter_patches() {
        let matcher = &patch["match"];
        assert_eq!(matcher["context"], "GATEWAY", "patch must target gateways: {patch:?}");
        assert_eq!(
            matcher["listener"]["filterChain"]["filter"]["name"], "envoy.filters.network.http_connection_manager",
            "patch must target the HTTP connection manager: {patch:?}"
        );
        assert!(
            matcher.get("routeConfiguration").is_none(),
            "patch must not match routes: {patch:?}"
        );

        let anchor = anchor(&patch);
        assert!(!anchor.is_empty(), "patch must match a subFilter: {patch:?}");
        let inserted = || {
            patch["patch"]["value"]["name"]
                .as_str()
                .expect("insert needs a name")
                .to_owned()
        };
        match (operation(&patch), chain.iter().position(|name| name == anchor)) {
            ("REMOVE", _) => chain.retain(|name| name != anchor),
            ("INSERT_BEFORE", Some(i)) => chain.insert(i, inserted()),
            ("INSERT_AFTER", Some(i)) => chain.insert(i + 1, inserted()),
            ("INSERT_BEFORE" | "INSERT_AFTER", None) => {},
            (operation, _) => panic!("operation {operation} is not modelled"),
        }
    }
    chain
}

fn http_filter_patches() -> Vec<Value> {
    let filter: Value = serde_yaml::from_str(ENVOY_FILTER).expect("envoy-filter.yaml should parse");
    filter["spec"]["configPatches"]
        .as_sequence()
        .expect("configPatches should be a list")
        .iter()
        .filter(|patch| patch["applyTo"] == "HTTP_FILTER")
        .cloned()
        .collect()
}

fn gateway() -> Value {
    serde_yaml::from_str(GATEWAY).expect("gateway fixture should parse")
}

fn gateway_chain(variant: &str) -> Vec<String> {
    gateway()["chains"][variant]
        .as_sequence()
        .expect("chain should be a list")
        .iter()
        .map(|name| name.as_str().expect("filter name should be a string").to_owned())
        .collect()
}

/// Swap the captured `WasmPlugin` for another Kuadrant auth filter name.
fn with_auth(chain: Vec<String>, auth: &str) -> Vec<String> {
    chain
        .into_iter()
        .map(|name| if name == WASMPLUGIN { auth.to_owned() } else { name })
        .collect()
}

fn single_position(chain: &[String], name: &str) -> usize {
    let positions: Vec<usize> = chain
        .iter()
        .enumerate()
        .filter(|(_, filter)| *filter == name)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(positions.len(), 1, "{name} must appear exactly once in {chain:?}");
    positions[0]
}

fn operation(patch: &Value) -> &str {
    patch["patch"]["operation"].as_str().unwrap_or_default()
}

fn anchor(patch: &Value) -> &str {
    patch["match"]["listener"]["filterChain"]["filter"]["subFilter"]["name"]
        .as_str()
        .unwrap_or_default()
}
