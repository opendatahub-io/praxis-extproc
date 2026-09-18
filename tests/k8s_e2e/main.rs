// SPDX-License-Identifier: Apache-2.0

//! K8s black-box e2e tests for Praxis AI filters.
//!
//! Tests the full BBR + IPP ext-proc topology with header-based model routing:
//!
//! ```text
//! Client → Gateway → [ipp-pre: model_to_header → X-Gateway-Model-Name]
//!                   → [ipp: response headers]
//!                   → HTTPRoute (header match) → llm-katan
//! ```
//!
//! If FDS deferral is broken, ipp-pre's header mutation is silently dropped
//! by Envoy. Without `X-Gateway-Model-Name`, no route matches → 404.
//!
//! Run: `make e2e-test` or `cargo test --features k8s-e2e --test k8s_e2e -- --nocapture`

#![cfg(feature = "k8s-e2e")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::cognitive_complexity,
    clippy::missing_assert_message,
    clippy::needless_raw_strings,
    clippy::needless_raw_string_hashes,
    clippy::missing_docs_in_private_items,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "k8s e2e tests"
)]
#![allow(missing_docs, reason = "k8s e2e test module")]

mod fixtures;

mod anthropic;
mod completions;
mod direct;
mod errors;
mod filters;
mod routing;
