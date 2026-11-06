// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Red Hat, Inc.

//! K8s black-box e2e tests for Praxis AI filters.
//!
//! Tests the full BBR + IPP ext-proc topology with header-based model routing:
//!
//! ```text
//! Client → Gateway → [ipp-pre: strip X-Gateway-Model-Name → json_body]
//!                   → [ipp: response headers]
//!                   → HTTPRoute (header match) → llm-katan
//! ```
//!
//! BBR strips any caller-supplied routing header before promoting body
//! `model`, so routing identity is body-authoritative. If FDS deferral is
//! broken, ipp-pre's header mutation is silently dropped by Envoy and no
//! route matches → 404.
//!
//! Run one mode at a time through the Makefile, which skips tests that are
//! incompatible with the selected Envoy body mode:
//! `make e2e-test MODE=buffered V=1`
//! `make e2e-test MODE=fds V=1`

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
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::future_not_send,
    clippy::large_futures,
    clippy::needless_pass_by_value,
    reason = "k8s e2e tests"
)]
#![allow(missing_docs, reason = "k8s e2e test module")]

mod fixtures;

mod chained_oracle;
mod completions;
mod direct;
mod errors;
mod fd_streamed_oracle;
mod fds_mode;
mod filters;
mod guardrails;
mod oracle_helpers;
mod pre_processing;
mod routing;
